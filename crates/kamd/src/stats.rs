//! Bounded request statistics and model cache.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use kam_kiro::ModelInfo;
use serde::{Deserialize, Serialize};

const MAX_DIMENSION_KEYS: usize = 1_024;
const OTHER_DIMENSION: &str = "other";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLog {
    pub timestamp: i64,
    #[serde(default)]
    pub trace_id: String,
    pub request_id: String,
    pub path: String,
    pub model: String,
    pub original_model: String,
    pub kiro_model: String,
    pub account_id: String,
    pub endpoint: String,
    pub duration_ms: u64,
    pub status: u16,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub credits: f64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counter {
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub credits: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Counter {
    fn record(&mut self, request: &RequestLog) {
        self.requests += 1;
        self.successes += u64::from(request.status < 400);
        self.failures += u64::from(request.status >= 400);
        self.credits += request.credits;
        self.input_tokens += request.input_tokens;
        self.output_tokens += request.output_tokens;
    }

    fn merge(&mut self, other: &Self) {
        self.requests += other.requests;
        self.successes += other.successes;
        self.failures += other.failures;
        self.credits += other.credits;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowBucket {
    pub total: Counter,
    pub by_account: HashMap<String, Counter>,
    pub by_endpoint: HashMap<String, Counter>,
    pub by_model: HashMap<String, Counter>,
    pub latencies_ms: VecDeque<u64>,
}

impl WindowBucket {
    fn record(&mut self, request: &RequestLog) {
        self.total.record(request);
        record_dimension(&mut self.by_account, &request.account_id, request);
        record_dimension(&mut self.by_endpoint, &request.endpoint, request);
        record_dimension(&mut self.by_model, &request.model, request);
        self.latencies_ms.push_back(request.duration_ms);
        while self.latencies_ms.len() > 2_000 {
            self.latencies_ms.pop_front();
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProxyStats {
    pub total: Counter,
    pub by_account: HashMap<String, Counter>,
    pub by_endpoint: HashMap<String, Counter>,
    pub by_model: HashMap<String, Counter>,
    pub recent_requests: VecDeque<RequestLog>,
    pub latencies_ms: VecDeque<u64>,
    #[serde(default)]
    pub minute_buckets: BTreeMap<i64, WindowBucket>,
}

impl ProxyStats {
    fn bound_dimensions(&mut self) {
        bound_dimensions(&mut self.by_account);
        bound_dimensions(&mut self.by_endpoint);
        bound_dimensions(&mut self.by_model);
        for bucket in self.minute_buckets.values_mut() {
            bound_dimensions(&mut bucket.by_account);
            bound_dimensions(&mut bucket.by_endpoint);
            bound_dimensions(&mut bucket.by_model);
        }
    }

    pub fn percentiles(&self) -> (u64, u64, u64) {
        let mut values = self.latencies_ms.iter().copied().collect::<Vec<_>>();
        values.sort_unstable();
        (
            percentile(&values, 0.50),
            percentile(&values, 0.95),
            percentile(&values, 0.99),
        )
    }
}

pub struct StatsStore {
    path: PathBuf,
    state: Mutex<ProxyStats>,
    sender: tokio::sync::broadcast::Sender<RequestLog>,
}

impl StatsStore {
    pub async fn load(path: &Path) -> anyhow::Result<Self> {
        let mut state = match tokio::fs::read_to_string(path).await {
            // Bootstrap and older releases used `{}` as the canonical empty
            // statistics document. Keep that representation compatible while
            // still rejecting partially corrupt/non-conforming objects.
            Ok(raw) if matches!(raw.trim(), "" | "{}") => ProxyStats::default(),
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|error| anyhow::anyhow!("parse {}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProxyStats::default(),
            Err(error) => return Err(error.into()),
        };
        state.bound_dimensions();
        let (sender, _) = tokio::sync::broadcast::channel(1_024);
        Ok(Self {
            path: path.into(),
            state: Mutex::new(state),
            sender,
        })
    }

    #[cfg(test)]
    pub fn empty(path: &Path) -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(1_024);
        Self {
            path: path.into(),
            state: Mutex::new(ProxyStats::default()),
            sender,
        }
    }

    pub fn record(&self, request: RequestLog) {
        let notification = request.clone();
        let mut state = lock(&self.state);
        state.total.record(&request);
        record_dimension(&mut state.by_account, &request.account_id, &request);
        record_dimension(&mut state.by_endpoint, &request.endpoint, &request);
        record_dimension(&mut state.by_model, &request.model, &request);
        state.latencies_ms.push_back(request.duration_ms);
        let minute = request.timestamp.div_euclid(60);
        state
            .minute_buckets
            .entry(minute)
            .or_default()
            .record(&request);
        let oldest = minute - 60 * 24 * 30;
        state.minute_buckets.retain(|bucket, _| *bucket >= oldest);
        state.recent_requests.push_back(request);
        while state.recent_requests.len() > 1_000 {
            state.recent_requests.pop_front();
        }
        while state.latencies_ms.len() > 10_000 {
            state.latencies_ms.pop_front();
        }
        drop(state);
        let _result = self.sender.send(notification);
    }

    pub fn snapshot(&self, recent: Option<usize>) -> ProxyStats {
        let mut output = lock(&self.state).clone();
        if let Some(maximum) = recent {
            while output.recent_requests.len() > maximum {
                output.recent_requests.pop_front();
            }
        }
        output
    }

    pub fn window(&self, since: Option<i64>, recent: Option<usize>) -> ProxyStats {
        if since.is_none() {
            return self.snapshot(recent);
        }
        let cutoff = since.unwrap_or(i64::MIN);
        let state = lock(&self.state);
        let mut output = ProxyStats::default();
        for bucket in state
            .minute_buckets
            .range(cutoff.div_euclid(60)..)
            .map(|(_, bucket)| bucket)
        {
            output.total.merge(&bucket.total);
            merge_dimensions(&mut output.by_account, &bucket.by_account);
            merge_dimensions(&mut output.by_endpoint, &bucket.by_endpoint);
            merge_dimensions(&mut output.by_model, &bucket.by_model);
            output
                .latencies_ms
                .extend(bucket.latencies_ms.iter().copied());
        }
        output.recent_requests = state
            .recent_requests
            .iter()
            .filter(|request| request.timestamp >= cutoff)
            .cloned()
            .collect();
        if let Some(maximum) = recent {
            while output.recent_requests.len() > maximum {
                output.recent_requests.pop_front();
            }
        }
        output
    }

    pub async fn persist(&self) -> anyhow::Result<()> {
        let snapshot = lock(&self.state).clone();
        kam_store::atomic::write_json_atomically(&self.path, &snapshot, Some(0o600)).await
    }

    pub async fn follow(
        &self,
        after_request_id: Option<&str>,
        tail: usize,
        wait_ms: u64,
        level: Option<&str>,
        account: Option<&str>,
    ) -> Vec<RequestLog> {
        // Subscribe before inspecting the ring to avoid missing a record between
        // the initial read and registration with the broadcast channel.
        let mut receiver = self.sender.subscribe();
        let current = self.filtered_logs(after_request_id, tail, level, account);
        if !current.is_empty() || wait_ms == 0 {
            return current;
        }
        let deadline = std::time::Duration::from_millis(wait_ms.min(60_000));
        let _result = tokio::time::timeout(deadline, async {
            loop {
                match receiver.recv().await {
                    Ok(request) if matches_log(&request, level, account) => break,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;
        self.filtered_logs(after_request_id, tail, level, account)
    }

    fn filtered_logs(
        &self,
        after_request_id: Option<&str>,
        tail: usize,
        level: Option<&str>,
        account: Option<&str>,
    ) -> Vec<RequestLog> {
        let state = lock(&self.state);
        let start = after_request_id
            .and_then(|id| {
                state
                    .recent_requests
                    .iter()
                    .position(|request| request.request_id == id)
                    .map(|index| index + 1)
            })
            .unwrap_or_else(|| state.recent_requests.len().saturating_sub(tail));
        state
            .recent_requests
            .iter()
            .skip(start)
            .filter(|request| matches_log(request, level, account))
            .take(tail.min(1_000))
            .cloned()
            .collect()
    }
}

fn matches_log(request: &RequestLog, level: Option<&str>, account: Option<&str>) -> bool {
    if account.is_some_and(|account| request.account_id != account) {
        return false;
    }
    match level.map(str::to_ascii_lowercase).as_deref() {
        Some("error") => request.status >= 500,
        Some("warn" | "warning") => request.status >= 400,
        _ => true,
    }
}

fn merge_dimensions(target: &mut HashMap<String, Counter>, source: &HashMap<String, Counter>) {
    for (key, counter) in source {
        merge_dimension(target, key, counter);
    }
}

fn record_dimension(dimensions: &mut HashMap<String, Counter>, key: &str, request: &RequestLog) {
    dimension_entry(dimensions, key).record(request);
}

fn merge_dimension(dimensions: &mut HashMap<String, Counter>, key: &str, counter: &Counter) {
    dimension_entry(dimensions, key).merge(counter);
}

fn dimension_entry<'a>(dimensions: &'a mut HashMap<String, Counter>, key: &str) -> &'a mut Counter {
    let bounded_key = if dimensions.contains_key(key)
        || (key != OTHER_DIMENSION && dimensions.len() < MAX_DIMENSION_KEYS - 1)
    {
        key
    } else {
        OTHER_DIMENSION
    };
    dimensions.entry(bounded_key.to_owned()).or_default()
}

fn bound_dimensions(dimensions: &mut HashMap<String, Counter>) {
    if dimensions.len() < MAX_DIMENSION_KEYS {
        return;
    }
    let entries = std::mem::take(dimensions);
    for (key, counter) in entries {
        merge_dimension(dimensions, &key, &counter);
    }
}

#[derive(Default)]
pub struct ModelCache {
    state: Mutex<ModelCacheState>,
}

#[derive(Default)]
struct ModelCacheState {
    models: Vec<ModelInfo>,
    updated_at_ms: u64,
    refreshing: bool,
}

impl ModelCache {
    pub fn get(&self, ttl_ms: u64) -> (Vec<ModelInfo>, bool) {
        let state = lock(&self.state);
        let fresh =
            !state.models.is_empty() && now_ms().saturating_sub(state.updated_at_ms) <= ttl_ms;
        (state.models.clone(), fresh)
    }

    pub fn begin_refresh(&self) -> bool {
        let mut state = lock(&self.state);
        if state.refreshing {
            false
        } else {
            state.refreshing = true;
            true
        }
    }

    pub fn finish_refresh(&self, models: Vec<ModelInfo>) {
        let mut state = lock(&self.state);
        if !models.is_empty() {
            state.models = models;
            state.updated_at_ms = now_ms();
        }
        state.refreshing = false;
    }
}

fn percentile(values: &[u64], quantile: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let index = ((values.len() - 1) as f64 * quantile).round() as usize;
    values[index]
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, timestamp: i64, status: u16) -> RequestLog {
        RequestLog {
            timestamp,
            trace_id: format!("trace-{id}"),
            request_id: id.into(),
            path: "/v1/messages".into(),
            model: "mapped".into(),
            original_model: "client".into(),
            kiro_model: "kiro".into(),
            account_id: "acc_test".into(),
            endpoint: "amazonq".into(),
            duration_ms: 10,
            status,
            input_tokens: 1,
            output_tokens: 1,
            credits: 0.1,
            error: (status >= 400).then(|| "failed".into()),
        }
    }

    #[test]
    fn time_windows_are_not_limited_by_the_recent_request_ring() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = StatsStore::empty(&directory.path().join("stats.json"));
        let now = 2_000_000_000;
        for index in 0..1_500 {
            store.record(request(&format!("req-{index}"), now, 200));
        }
        assert_eq!(store.snapshot(None).recent_requests.len(), 1_000);
        assert_eq!(store.window(Some(now - 60), None).total.requests, 1_500);
    }

    #[test]
    fn dimensions_are_bounded_in_totals_buckets_and_merged_windows() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = StatsStore::empty(&directory.path().join("stats.json"));
        let request_count = MAX_DIMENSION_KEYS + 100;
        for index in 0..request_count {
            let mut entry = request(&format!("req-{index}"), index as i64 * 60, 200);
            entry.account_id = format!("account-{index}");
            entry.endpoint = format!("endpoint-{index}");
            entry.model = format!("model-{index}");
            store.record(entry);
        }

        let snapshot = store.snapshot(None);
        for dimensions in [
            &snapshot.by_account,
            &snapshot.by_endpoint,
            &snapshot.by_model,
        ] {
            assert_eq!(dimensions.len(), MAX_DIMENSION_KEYS);
            assert_eq!(
                dimensions
                    .get(OTHER_DIMENSION)
                    .expect("overflow bucket")
                    .requests,
                101
            );
        }
        assert!(snapshot.minute_buckets.values().all(|bucket| {
            bucket.by_account.len() <= MAX_DIMENSION_KEYS
                && bucket.by_endpoint.len() <= MAX_DIMENSION_KEYS
                && bucket.by_model.len() <= MAX_DIMENSION_KEYS
        }));

        let window = store.window(Some(0), None);
        assert_eq!(window.total.requests, request_count as u64);
        assert!(window.by_account.len() <= MAX_DIMENSION_KEYS);
        assert!(window.by_endpoint.len() <= MAX_DIMENSION_KEYS);
        assert!(window.by_model.len() <= MAX_DIMENSION_KEYS);
    }

    #[tokio::test]
    async fn log_follow_wakes_on_a_matching_record() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = std::sync::Arc::new(StatsStore::empty(&directory.path().join("stats.json")));
        let follower = {
            let store = std::sync::Arc::clone(&store);
            tokio::spawn(async move { store.follow(None, 10, 1_000, Some("error"), None).await })
        };
        tokio::task::yield_now().await;
        store.record(request("ok", 1, 200));
        store.record(request("failed", 2, 503));
        let entries = follower.await.expect("join");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].request_id, "failed");
    }

    #[tokio::test]
    async fn bootstrap_empty_stats_remain_compatible_but_partial_state_fails() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        tokio::fs::write(&path, "{}\n").await.expect("write empty");
        let store = StatsStore::load(&path).await.expect("load bootstrap state");
        assert_eq!(store.snapshot(None).total.requests, 0);

        tokio::fs::write(&path, r#"{"total":{}}"#)
            .await
            .expect("write partial");
        assert!(StatsStore::load(&path).await.is_err());
    }
}

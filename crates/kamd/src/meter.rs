//! API-key credit reservations and persistent multi-dimensional usage.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use kam_core::config::ApiKeyConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MeterError {
    #[error("invalid API key")]
    Unauthorized,
    #[error("API key credit limit exceeded")]
    LimitExceeded,
    #[error("service daily credit limit exceeded")]
    DailyLimitExceeded,
    #[error("usage persistence failed: {0}")]
    Persist(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageBucket {
    pub requests: u64,
    pub credits: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl UsageBucket {
    fn add(&mut self, record: &UsageRecord) {
        self.requests += 1;
        self.credits += record.credits;
        self.input_tokens += record.input_tokens;
        self.output_tokens += record.output_tokens;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub timestamp: i64,
    pub model: String,
    pub original_model: Option<String>,
    pub kiro_model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub credits: f64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub token_usage_source: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApiKeyUsage {
    pub total_requests: u64,
    pub total_credits: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub daily: BTreeMap<String, UsageBucket>,
    pub by_model: HashMap<String, UsageBucket>,
    pub by_original_model: HashMap<String, UsageBucket>,
    pub by_kiro_model: HashMap<String, UsageBucket>,
    pub by_path: HashMap<String, UsageBucket>,
    pub history: VecDeque<UsageRecord>,
}

impl ApiKeyUsage {
    fn add(&mut self, record: UsageRecord) {
        self.total_requests += 1;
        self.total_credits += record.credits;
        self.total_input_tokens += record.input_tokens;
        self.total_output_tokens += record.output_tokens;
        self.daily
            .entry(utc_day(record.timestamp))
            .or_default()
            .add(&record);
        self.by_model
            .entry(record.model.clone())
            .or_default()
            .add(&record);
        if let Some(model) = &record.original_model {
            self.by_original_model
                .entry(model.clone())
                .or_default()
                .add(&record);
        }
        if let Some(model) = &record.kiro_model {
            self.by_kiro_model
                .entry(model.clone())
                .or_default()
                .add(&record);
        }
        self.by_path
            .entry(record.path.clone())
            .or_default()
            .add(&record);
        self.history.push_back(record);
        while self.history.len() > 100 {
            self.history.pop_front();
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub credits_limit: Option<f64>,
    pub reserved_credits: f64,
    pub usage: ApiKeyUsage,
}

struct KeyState {
    id: String,
    config: ApiKeyConfig,
    usage: ApiKeyUsage,
    reserved: f64,
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    api_keys: HashMap<String, ApiKeyUsage>,
    #[serde(default)]
    service_daily: ServiceDailyUsage,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ServiceDailyUsage {
    day: String,
    used: f64,
}

#[derive(Debug, Default)]
struct ServiceCreditState {
    day: String,
    used: f64,
    reserved: f64,
    limit: f64,
}

pub struct Meter {
    path: PathBuf,
    keys: Mutex<HashMap<String, KeyState>>,
    service: Mutex<ServiceCreditState>,
    persist_lock: tokio::sync::Mutex<()>,
    persist_scheduled: AtomicBool,
    dirty_generation: AtomicU64,
}

pub struct CreditReservation {
    meter: Arc<Meter>,
    key_id: Option<String>,
    estimate: f64,
    settled: bool,
}

impl Meter {
    #[cfg(test)]
    pub fn empty(path: &Path, configs: &[ApiKeyConfig]) -> Arc<Self> {
        let keys = configs
            .iter()
            .cloned()
            .map(|config| {
                let id = config.id.clone().unwrap_or_else(|| key_id(&config.key));
                (
                    id.clone(),
                    KeyState {
                        id,
                        config,
                        usage: ApiKeyUsage::default(),
                        reserved: 0.0,
                    },
                )
            })
            .collect();
        Arc::new(Self {
            path: path.into(),
            keys: Mutex::new(keys),
            service: Mutex::new(ServiceCreditState {
                day: utc_day(now_secs()),
                ..ServiceCreditState::default()
            }),
            persist_lock: tokio::sync::Mutex::new(()),
            persist_scheduled: AtomicBool::new(false),
            dirty_generation: AtomicU64::new(0),
        })
    }

    pub async fn load(path: &Path, configs: &[ApiKeyConfig]) -> Result<Arc<Self>, MeterError> {
        let persisted = match tokio::fs::read_to_string(path).await {
            Ok(raw) if !raw.trim().is_empty() => {
                serde_json::from_str::<Persisted>(&raw).map_err(|error| {
                    MeterError::Persist(format!("parse {}: {error}", path.display()))
                })?
            }
            Ok(_) => Persisted::default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
            Err(error) => {
                return Err(MeterError::Persist(format!(
                    "read {}: {error}",
                    path.display()
                )))
            }
        };
        let today = utc_day(now_secs());
        let service_used = if persisted.service_daily.day == today {
            persisted.service_daily.used.max(0.0)
        } else {
            0.0
        };
        let keys = configs
            .iter()
            .cloned()
            .map(|config| {
                let generated = config.id.is_none();
                let id = config.id.clone().unwrap_or_else(|| key_id(&config.key));
                let usage = persisted
                    .api_keys
                    .get(&id)
                    .or_else(|| {
                        generated
                            .then(|| persisted.api_keys.get(&legacy_key_id(&config.key)))
                            .flatten()
                    })
                    .cloned()
                    .unwrap_or_default();
                (
                    id.clone(),
                    KeyState {
                        id,
                        config,
                        usage,
                        reserved: 0.0,
                    },
                )
            })
            .collect();
        Ok(Arc::new(Self {
            path: path.into(),
            keys: Mutex::new(keys),
            service: Mutex::new(ServiceCreditState {
                day: today,
                used: service_used,
                reserved: 0.0,
                limit: 0.0,
            }),
            persist_lock: tokio::sync::Mutex::new(()),
            persist_scheduled: AtomicBool::new(false),
            dirty_generation: AtomicU64::new(0),
        }))
    }

    pub fn set_daily_limit(&self, limit: f64) {
        lock(&self.service).limit = limit.max(0.0);
    }

    pub fn daily_snapshot(&self) -> (String, f64, f64, f64) {
        let service = lock(&self.service);
        (
            service.day.clone(),
            service.used,
            service.reserved,
            service.limit,
        )
    }

    pub async fn reset_daily(&self) -> Result<(), MeterError> {
        {
            let mut service = lock(&self.service);
            service.day = utc_day(now_secs());
            service.used = 0.0;
        }
        self.persist().await
    }

    pub fn authenticate(&self, presented: Option<&str>) -> Result<Option<String>, MeterError> {
        let keys = lock(&self.keys);
        if keys.is_empty() {
            return Ok(None);
        }
        let presented = presented.ok_or(MeterError::Unauthorized)?;
        keys.values()
            .find(|state| state.config.enabled && constant_time_eq(&state.config.key, presented))
            .map(|state| Some(state.id.clone()))
            .ok_or(MeterError::Unauthorized)
    }

    pub fn reserve(
        self: &Arc<Self>,
        key_id: Option<&str>,
        estimate: f64,
    ) -> Result<CreditReservation, MeterError> {
        let estimate = estimate.max(0.0);
        {
            let mut service = lock(&self.service);
            reset_service_day_if_needed(&mut service);
            if service.limit > 0.0 && service.used + service.reserved + estimate > service.limit {
                return Err(MeterError::DailyLimitExceeded);
            }
            service.reserved += estimate;
        }
        if let Some(id) = key_id {
            let mut keys = lock(&self.keys);
            let Some(state) = keys.get_mut(id) else {
                let mut service = lock(&self.service);
                service.reserved = (service.reserved - estimate).max(0.0);
                return Err(MeterError::Unauthorized);
            };
            if state
                .config
                .credits_limit
                .is_some_and(|limit| state.usage.total_credits + state.reserved + estimate > limit)
            {
                let mut service = lock(&self.service);
                service.reserved = (service.reserved - estimate).max(0.0);
                return Err(MeterError::LimitExceeded);
            }
            state.reserved += estimate;
        }
        Ok(CreditReservation {
            meter: Arc::clone(self),
            key_id: key_id.map(str::to_string),
            estimate,
            settled: false,
        })
    }

    pub fn list(&self) -> Vec<ApiKeyView> {
        lock(&self.keys)
            .values()
            .map(|state| ApiKeyView {
                id: state.id.clone(),
                name: state.config.name.clone(),
                enabled: state.config.enabled,
                credits_limit: state.config.credits_limit,
                reserved_credits: state.reserved,
                usage: state.usage.clone(),
            })
            .collect()
    }

    pub async fn reset_usage(self: &Arc<Self>, id_or_name: &str) -> Result<bool, MeterError> {
        {
            let mut keys = lock(&self.keys);
            let Some(state) = keys
                .values_mut()
                .find(|state| state.id == id_or_name || state.config.name == id_or_name)
            else {
                return Ok(false);
            };
            state.usage = ApiKeyUsage::default();
        }
        self.persist().await?;
        Ok(true)
    }

    pub fn replace_configs(&self, configs: &[ApiKeyConfig]) {
        let mut keys = lock(&self.keys);
        let mut next = HashMap::new();
        for config in configs.iter().cloned() {
            let id = config.id.clone().unwrap_or_else(|| key_id(&config.key));
            let (usage, reserved) = keys
                .remove(&id)
                .map(|state| (state.usage, state.reserved))
                .unwrap_or_default();
            next.insert(
                id.clone(),
                KeyState {
                    id,
                    config,
                    usage,
                    reserved,
                },
            );
        }
        *keys = next;
    }

    pub async fn persist(&self) -> Result<(), MeterError> {
        let _persist_guard = self.persist_lock.lock().await;
        let persisted = Persisted {
            api_keys: lock(&self.keys)
                .iter()
                .map(|(id, state)| (id.clone(), state.usage.clone()))
                .collect(),
            service_daily: {
                let service = lock(&self.service);
                ServiceDailyUsage {
                    day: service.day.clone(),
                    used: service.used,
                }
            },
        };
        kam_store::atomic::write_json_atomically(&self.path, &persisted, Some(0o600))
            .await
            .map_err(|error| MeterError::Persist(error.to_string()))
    }

    fn mark_dirty(self: &Arc<Self>) {
        self.dirty_generation.fetch_add(1, Ordering::AcqRel);
        if self
            .persist_scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let meter = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let generation = meter.dirty_generation.load(Ordering::Acquire);
                if let Err(error) = meter.persist().await {
                    tracing::error!(%error, "failed to persist metering state");
                }
                if meter.dirty_generation.load(Ordering::Acquire) != generation {
                    continue;
                }
                meter.persist_scheduled.store(false, Ordering::Release);
                if meter.dirty_generation.load(Ordering::Acquire) != generation
                    && meter
                        .persist_scheduled
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    continue;
                }
                break;
            }
        });
    }
}

impl CreditReservation {
    /// Reserve capacity for an additional upstream round while keeping the
    /// original request atomic from the caller's perspective.
    pub fn extend(&mut self, additional: f64) -> Result<(), MeterError> {
        let additional = additional.max(0.0);
        if additional == 0.0 {
            return Ok(());
        }
        {
            let mut service = lock(&self.meter.service);
            reset_service_day_if_needed(&mut service);
            if service.limit > 0.0 && service.used + service.reserved + additional > service.limit {
                return Err(MeterError::DailyLimitExceeded);
            }
            service.reserved += additional;
        }
        if let Some(id) = &self.key_id {
            let mut keys = lock(&self.meter.keys);
            let Some(state) = keys.get_mut(id) else {
                let mut service = lock(&self.meter.service);
                service.reserved = (service.reserved - additional).max(0.0);
                return Err(MeterError::Unauthorized);
            };
            if state.config.credits_limit.is_some_and(|limit| {
                state.usage.total_credits + state.reserved + additional > limit
            }) {
                let mut service = lock(&self.meter.service);
                service.reserved = (service.reserved - additional).max(0.0);
                return Err(MeterError::LimitExceeded);
            }
            state.reserved += additional;
        }
        self.estimate += additional;
        Ok(())
    }

    pub async fn settle(mut self, record: UsageRecord) -> Result<(), MeterError> {
        let mut durable_limit_state = {
            let mut service = lock(&self.meter.service);
            reset_service_day_if_needed(&mut service);
            service.reserved = (service.reserved - self.estimate).max(0.0);
            service.used += record.credits.max(0.0);
            service.limit > 0.0
        };
        if let Some(id) = &self.key_id {
            if let Some(state) = lock(&self.meter.keys).get_mut(id) {
                state.reserved = (state.reserved - self.estimate).max(0.0);
                state.usage.add(record);
                durable_limit_state |= state.config.credits_limit.is_some();
            }
        }
        self.settled = true;
        if durable_limit_state {
            self.meter.persist().await
        } else {
            self.meter.mark_dirty();
            Ok(())
        }
    }
}

impl Drop for CreditReservation {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        {
            let mut service = lock(&self.meter.service);
            service.reserved = (service.reserved - self.estimate).max(0.0);
        }
        if let Some(id) = &self.key_id {
            if let Some(state) = lock(&self.meter.keys).get_mut(id) {
                state.reserved = (state.reserved - self.estimate).max(0.0);
            }
        }
    }
}

fn reset_service_day_if_needed(service: &mut ServiceCreditState) {
    let today = utc_day(now_secs());
    if service.day != today {
        service.day = today;
        service.used = 0.0;
    }
}

fn key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8]
        .iter()
        .fold(String::from("ak_"), |mut output, byte| {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
            output
        })
}

fn legacy_key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    format!("ak_{:02x}{:02x}", digest[0], digest[1])
}

fn constant_time_eq(expected: &str, presented: &str) -> bool {
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    let mut difference = expected.len() ^ presented.len();
    for index in 0..expected.len().max(presented.len()) {
        difference |= expected.get(index).copied().unwrap_or(0) as usize
            ^ presented.get(index).copied().unwrap_or(0) as usize;
    }
    difference == 0
}

fn utc_day(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

// Howard Hinnant's civil calendar conversion, epoch adjusted to Unix day zero.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kam_core::config::ApiKeyFormat;

    fn usage(credits: f64) -> UsageRecord {
        UsageRecord {
            timestamp: now_secs(),
            model: "mapped".into(),
            original_model: Some("client".into()),
            kiro_model: Some("kiro".into()),
            input_tokens: 100,
            output_tokens: 50,
            credits,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            token_usage_source: "server".into(),
            path: "/v1/messages".into(),
        }
    }

    #[tokio::test]
    async fn reservations_prevent_concurrent_limit_overshoot() {
        let directory = tempfile::tempdir().expect("tempdir");
        let meter = Meter::load(
            &directory.path().join("daily.json"),
            &[ApiKeyConfig {
                id: Some("ak_test".into()),
                name: "test".into(),
                key: "secret".into(),
                format: ApiKeyFormat::Sk,
                enabled: true,
                credits_limit: Some(10.0),
            }],
        )
        .await
        .expect("load");
        let first = meter.reserve(Some("ak_test"), 6.0).expect("reserve");
        assert!(matches!(
            meter.reserve(Some("ak_test"), 6.0),
            Err(MeterError::LimitExceeded)
        ));
        drop(first);
        assert!(meter.reserve(Some("ak_test"), 6.0).is_ok());
    }

    #[tokio::test]
    async fn service_daily_limit_and_auto_round_extensions_survive_restart() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daily.json");
        let meter = Meter::load(&path, &[]).await.expect("load");
        meter.set_daily_limit(10.0);
        let mut reservation = meter.reserve(None, 3.0).expect("first round");
        reservation.extend(3.0).expect("second round");
        assert!(matches!(
            reservation.extend(5.0),
            Err(MeterError::DailyLimitExceeded)
        ));
        reservation.settle(usage(6.0)).await.expect("settle");

        let reloaded = Meter::load(&path, &[]).await.expect("reload");
        reloaded.set_daily_limit(10.0);
        assert!(matches!(
            reloaded.reserve(None, 5.0),
            Err(MeterError::DailyLimitExceeded)
        ));
        assert_eq!(reloaded.daily_snapshot().1, 6.0);
    }

    #[tokio::test]
    async fn corrupt_usage_file_fails_closed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daily.json");
        tokio::fs::write(&path, "{not-json").await.expect("write");
        assert!(matches!(
            Meter::load(&path, &[]).await,
            Err(MeterError::Persist(_))
        ));
    }

    #[tokio::test]
    async fn reset_usage_persists_immediately_without_releasing_inflight_credit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daily.json");
        let config = ApiKeyConfig {
            id: Some("ak_test".into()),
            name: "test".into(),
            key: "secret".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            credits_limit: Some(10.0),
        };
        let meter = Meter::load(&path, std::slice::from_ref(&config))
            .await
            .expect("load");
        let settled = meter.reserve(Some("ak_test"), 1.0).expect("reserve");
        settled.settle(usage(1.0)).await.expect("settle");
        let inflight = meter.reserve(Some("ak_test"), 4.0).expect("inflight");
        assert!(meter.reset_usage("ak_test").await.expect("reset"));
        let view = meter.list().pop().expect("key");
        assert_eq!(view.usage.total_credits, 0.0);
        assert_eq!(view.reserved_credits, 4.0);
        drop(inflight);

        let reloaded = Meter::load(&path, &[config]).await.expect("reload");
        assert_eq!(reloaded.list()[0].usage.total_credits, 0.0);
    }

    #[test]
    fn utc_date_conversion_is_stable() {
        assert_eq!(utc_day(0), "1970-01-01");
        assert_eq!(utc_day(1_704_067_200), "2024-01-01");
    }
}

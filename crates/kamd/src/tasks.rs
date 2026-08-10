//! Periodic refresh and persistence scheduler.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kam_notify::{WebhookEvent, WebhookEventKind};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::state::AppState;

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct TaskRun {
    pub last_run_at: Option<i64>,
    pub last_result: Option<String>,
    pub run_count: u64,
}

#[derive(Clone, Default)]
pub struct TaskRegistry {
    state: Arc<Mutex<BTreeMap<String, TaskRun>>>,
}

impl TaskRegistry {
    pub fn record(&self, name: &str, result: impl Into<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let run = state.entry(name.into()).or_default();
        run.last_run_at = Some(crate::meter::now_secs());
        run.last_result = Some(result.into());
        run.run_count += 1;
    }

    pub fn snapshot(&self, state: &AppState) -> serde_json::Value {
        let config = state.config.current();
        let runs = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        serde_json::json!({
            "token_refresh":{"interval_ms":config.tasks.token_refresh_interval_ms,"run":runs.get("token_refresh")},
            "status_check":{"interval_ms":config.tasks.status_check_interval_ms,"run":runs.get("status_check")},
            "stats_persist":{"interval_ms":config.tasks.stats_persist_interval_ms,"run":runs.get("stats_persist")},
            "daily_reset":{"interval_ms":86_400_000u64,"run":runs.get("daily_reset")},
            "model_cache_refresh":{"interval_ms":config.models.cache_ttl_ms,"run":runs.get("model_cache_refresh")},
            "health_recheck":{"interval_ms":config.pool.cooldown.quota_reset_ms,"run":runs.get("health_recheck")}
        })
    }
}

pub fn spawn(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_account_file_watcher(Arc::clone(&state), shutdown.clone());
    spawn_token_refresh(Arc::clone(&state), shutdown.clone());
    spawn_adaptive_admission(Arc::clone(&state), shutdown.clone());
    spawn_model_refresh(Arc::clone(&state), shutdown.clone());
    spawn_status_check(Arc::clone(&state), shutdown.clone());
    spawn_health_recheck(Arc::clone(&state), shutdown.clone());
    spawn_daily_reset(Arc::clone(&state), shutdown.clone());
    spawn_persistence(state, shutdown);
}

fn spawn_account_file_watcher(state: Arc<AppState>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        let path = state.paths.accounts_file.clone();
        let mut previous = account_storage_signature(&path).await;
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let current = account_storage_signature(&path).await;
                    if current == previous {
                        continue;
                    }
                    previous = current;
                    match kam_store::accounts::AccountStore::load(&path).await {
                        Ok(next) => {
                            let disk = serde_json::to_value(next.all()).ok();
                            let memory = state.with_accounts(|accounts| {
                                serde_json::to_value(accounts.all()).ok()
                            });
                            if disk != memory {
                                let count = next.len();
                                state.apply_account_file_reload(next).await;
                                info!(count, path = %path.display(), "accounts file reloaded");
                            }
                        }
                        Err(error) => {
                            warn!(%error, path = %path.display(), "accounts file reload failed; keeping current accounts");
                        }
                    }
                }
            }
        }
    });
}

async fn account_storage_signature(path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    if let Ok(bytes) = tokio::fs::read(path).await {
        files.push((path.display().to_string(), bytes));
    }
    let mut directory_name = path.as_os_str().to_os_string();
    directory_name.push(".d");
    let directory = std::path::PathBuf::from(directory_name);
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(_) => return files,
    };
    let mut paths = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.is_ok_and(|kind| kind.is_file()) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    for delta in paths {
        if let Ok(bytes) = tokio::fs::read(&delta).await {
            files.push((delta.display().to_string(), bytes));
        }
    }
    files
}

fn spawn_status_check(state: Arc<AppState>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            let delay = Duration::from_millis(
                state
                    .config
                    .current()
                    .tasks
                    .status_check_interval_ms
                    .max(10_000),
            );
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(delay) => {
                    let result = status_check(&state).await;
                    state.task_registry.record(
                        "status_check",
                        result.unwrap_or_else(|error| error.to_string()),
                    );
                }
            }
        }
    });
}

fn spawn_health_recheck(state: Arc<AppState>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            let delay = Duration::from_millis(
                state
                    .config
                    .current()
                    .pool
                    .cooldown
                    .quota_reset_ms
                    .max(10_000),
            );
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(delay) => {
                    let result = health_recheck(&state).await;
                    state.task_registry.record(
                        "health_recheck",
                        result.unwrap_or_else(|error| error.to_string()),
                    );
                }
            }
        }
    });
}

fn spawn_model_refresh(state: Arc<AppState>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            let delay =
                Duration::from_millis(state.config.current().models.cache_ttl_ms.max(60_000));
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(delay) => {
                    let result = refresh_models(&state).await;
                    state.task_registry.record(
                        "model_cache_refresh",
                        result.unwrap_or_else(|error| error.to_string()),
                    );
                }
            }
        }
    });
}

fn spawn_daily_reset(state: Arc<AppState>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            let now = crate::meter::now_secs();
            let wait = 86_400 - now.rem_euclid(86_400);
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(wait as u64)) => {
                    if let Err(error) = state.meter.reset_daily().await {
                        warn!(%error, "failed to persist daily credit reset");
                    }
                    state.task_registry.record("daily_reset", "ok");
                }
            }
        }
    });
}

fn spawn_adaptive_admission(state: Arc<AppState>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            let delay = Duration::from_millis(
                state
                    .config
                    .current()
                    .server
                    .adaptive
                    .check_interval_ms
                    .max(1_000),
            );
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(delay) => {
                    let (_, _, p99) = state.stats.snapshot(None).percentiles();
                    let limit = state.adjust_admission(p99).await;
                    debug!(p99_ms = p99, admission_limit = limit, "adaptive admission check");
                }
            }
        }
    });
}

fn spawn_token_refresh(state: Arc<AppState>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            let delay = Duration::from_millis(
                state
                    .config
                    .current()
                    .tasks
                    .token_refresh_interval_ms
                    .max(1_000),
            );
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(delay) => {
                    let pool = state.pool();
                    let failures = state.refresh_expiring_tokens(&pool).await;
                    let failure_count = failures.len();
                    for (account_id, error) in failures {
                        warn!(%account_id, %error, "background token refresh failed");
                        let mut event = WebhookEvent::new(
                            WebhookEventKind::TokenExpired,
                            "Kiro token refresh failed",
                            error.to_string(),
                        );
                        event.account_id = Some(account_id);
                        state.notifier().emit(event);
                    }
                    if let Err(error) = persist_pool_accounts(&state).await {
                        warn!(%error, "failed to persist refreshed credentials");
                    }
                    state.task_registry.record(
                        "token_refresh",
                        format!("ok: {failure_count} failures"),
                    );
                }
            }
        }
    });
}

fn spawn_persistence(state: Arc<AppState>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            let delay = Duration::from_millis(
                state
                    .config
                    .current()
                    .tasks
                    .stats_persist_interval_ms
                    .max(1_000),
            );
            tokio::select! {
                _ = shutdown.cancelled() => {
                    persist_usage(&state).await;
                    break;
                }
                _ = tokio::time::sleep(delay) => persist_usage(&state).await,
            }
        }
    });
}

async fn persist_usage(state: &Arc<AppState>) {
    if let Err(error) = state.meter.persist().await {
        warn!(%error, "failed to persist API key usage");
    }
    if let Err(error) = state.stats.persist().await {
        warn!(%error, "failed to persist proxy stats");
    }
    let (p50, p95, p99) = state.stats.snapshot(None).percentiles();
    debug!(
        p50_ms = p50,
        p95_ms = p95,
        p99_ms = p99,
        "request latency percentiles"
    );
    state.task_registry.record("stats_persist", "ok");
}

pub(crate) async fn refresh_models(state: &Arc<AppState>) -> anyhow::Result<String> {
    if !state.config.current().models.dynamic_discovery {
        return Ok("disabled".into());
    }
    let pool = state.pool();
    let accounts = pool
        .snapshot()
        .await
        .into_iter()
        .filter(|account| account.enabled)
        .collect::<Vec<_>>();
    if accounts.is_empty() {
        state.models.finish_refresh(Vec::new());
        anyhow::bail!("no enabled account");
    }
    let mut union = std::collections::BTreeMap::new();
    let mut failures = Vec::new();
    for account in accounts {
        match state.kiro().list_models(&account).await {
            Ok(models) => {
                if let Some(runtime) = pool.get(&account.id).await {
                    runtime
                        .set_supported_models(models.iter().map(|model| model.model_id.clone()))
                        .await;
                }
                for model in models {
                    union.entry(model.model_id.clone()).or_insert(model);
                }
            }
            Err(error) => failures.push(format!("{}: {error}", account.id)),
        }
    }
    if union.is_empty() && !failures.is_empty() {
        state.models.finish_refresh(Vec::new());
        anyhow::bail!("model discovery failed: {}", failures.join("; "));
    }
    let models = union.into_values().collect::<Vec<_>>();
    let count = models.len();
    state.models.finish_refresh(models);
    Ok(format!("ok: {count} models, {} failures", failures.len()))
}

async fn status_check(state: &Arc<AppState>) -> anyhow::Result<String> {
    let pool = state.pool();
    let accounts = pool.snapshot().await;
    let mut healthy = 0;
    let mut failed = 0;
    for mut account in accounts.into_iter().filter(|account| account.enabled) {
        match refresh_account_usage(state, &pool, &account.id).await {
            Ok(true) => {
                if let Some(runtime) = pool.get(&account.id).await {
                    account = runtime.account.read().await.clone();
                }
            }
            Ok(false) => {}
            Err(error) => {
                debug!(account_id = %account.id, %error, "usage status check failed");
                failed += 1;
            }
        }
        if let Some(usage) = &account.usage {
            if usage.limit > 0.0 {
                let remaining = (100.0 - usage.percent_used).clamp(0.0, 100.0);
                let mut event = WebhookEvent::new(
                    WebhookEventKind::LowCredit,
                    "Kiro account credit is low",
                    format!("{remaining:.1}% credit remains"),
                );
                event.account_id = Some(account.id.clone());
                event.remaining_percent = Some(remaining);
                state.notifier().emit(event);
            }
        }
        if state.config.current().models.dynamic_discovery {
            match state.kiro().list_models(&account).await {
                Ok(models) => {
                    if let Some(runtime) = pool.get(&account.id).await {
                        runtime
                            .set_supported_models(models.iter().map(|model| model.model_id.clone()))
                            .await;
                    }
                    healthy += 1;
                }
                Err(error) => {
                    warn!(account_id = %account.id, %error, "status model check failed");
                    failed += 1;
                }
            }
        } else {
            healthy += 1;
        }
    }
    persist_pool_accounts(state).await?;
    Ok(format!("ok: {healthy} healthy, {failed} failed"))
}

pub(crate) async fn refresh_account_usage(
    state: &Arc<AppState>,
    pool: &kam_pool::AccountPool,
    account_id: &str,
) -> anyhow::Result<bool> {
    let runtime = pool
        .get(account_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("account not found: {account_id}"))?;
    let account = runtime.account.read().await.clone();
    let limits = state.kiro().get_usage_limits(&account).await?;
    let usage = limits.normalized_usage(crate::meter::now_secs());
    let subscription = limits.normalized_subscription();
    if usage.is_none() && subscription.is_none() {
        return Ok(false);
    }
    let mut account = runtime.account.write().await;
    if let Some(usage) = usage {
        account.credit_exhausted = usage.limit > 0.0 && usage.current >= usage.limit;
        account.usage = Some(usage);
    }
    if let Some(subscription) = subscription {
        account.subscription = Some(subscription);
    }
    Ok(true)
}

async fn health_recheck(state: &Arc<AppState>) -> anyhow::Result<String> {
    let pool = state.pool();
    let accounts = pool.snapshot().await;
    let mut recovered = 0;
    for account in accounts.into_iter().filter(|account| account.enabled) {
        let exhausted = pool
            .get(&account.id)
            .await
            .is_some_and(|runtime| runtime.health() == kam_pool::AccountHealth::Exhausted);
        if !account.credit_exhausted && !exhausted {
            continue;
        }
        let usage_recovered = refresh_account_usage(state, &pool, &account.id)
            .await
            .ok()
            .filter(|updated| *updated)
            .is_some()
            && pool.get(&account.id).await.is_some_and(|runtime| {
                runtime
                    .account
                    .try_read()
                    .map(|account| !account.credit_exhausted)
                    .unwrap_or(false)
            });
        if !usage_recovered {
            continue;
        }
        if state.config.current().models.dynamic_discovery {
            let Ok(models) = state.kiro().list_models(&account).await else {
                continue;
            };
            if let Some(runtime) = pool.get(&account.id).await {
                runtime
                    .set_supported_models(models.into_iter().map(|model| model.model_id))
                    .await;
            }
        }
        if pool.reset_health(&account.id).await {
            recovered += 1;
        }
    }
    if recovered > 0 {
        persist_pool_accounts(state).await?;
    }
    Ok(format!("ok: recovered {recovered}"))
}

pub async fn run_named(state: &Arc<AppState>, name: &str) -> anyhow::Result<serde_json::Value> {
    let result = match name {
        "token_refresh" => {
            let pool = state.pool();
            let failures = state.refresh_expiring_tokens(&pool).await;
            persist_pool_accounts(state).await?;
            format!("ok: {} failures", failures.len())
        }
        "stats_persist" => {
            state.meter.persist().await?;
            state.stats.persist().await?;
            "ok".into()
        }
        "model_cache_refresh" => refresh_models(state).await?,
        "status_check" => status_check(state).await?,
        "daily_reset" => {
            state.meter.reset_daily().await?;
            "ok".into()
        }
        "health_recheck" => health_recheck(state).await?,
        _ => anyhow::bail!("unknown task {name}"),
    };
    state.task_registry.record(name, result.clone());
    Ok(serde_json::json!({"name":name,"result":result}))
}

pub(crate) async fn persist_pool_accounts(state: &Arc<AppState>) -> anyhow::Result<()> {
    let snapshot = state.pool().snapshot().await;
    let _transaction = state.lock_account_mutation().await;
    let mut next = state.with_accounts(Clone::clone);
    for account in snapshot {
        next.replace_if_changed(account);
    }
    next.save().await?;
    state.replace_accounts(next);
    Ok(())
}

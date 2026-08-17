//! 服务运行态共享句柄。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use kproxy_core::config::Config;
use kproxy_core::paths::Paths;
use kproxy_kiro::endpoint::EndpointOverrides;
use kproxy_kiro::KiroClient;
use kproxy_notify::Notifier;
use kproxy_pool::{AccountPool, RefreshError, TokenRefresher};
use kproxy_store::accounts::AccountStore;
use kproxy_store::config_loader::ConfigHandle;
use kproxy_translate::TokenCountCache;
use tokio_util::sync::CancellationToken;

use crate::http::prompt_cache::PromptCacheTracker;
use crate::http::stream::KeepaliveHub;
use crate::meter::Meter;
use crate::stats::{ModelCache, StatsStore};
use crate::tasks::TaskRegistry;

pub struct TokenRefreshReport {
    pub checked: usize,
    pub eligible: usize,
    pub refreshed: Vec<(String, String)>,
    pub failures: Vec<(String, String, RefreshError)>,
}

impl TokenRefreshReport {
    pub fn summary(&self) -> String {
        format!(
            "ok: {} checked, {} eligible, {} refreshed, {} failures",
            self.checked,
            self.eligible,
            self.refreshed.len(),
            self.failures.len()
        )
    }
}

/// 进程内共享状态。
pub struct AppState {
    /// 当前生效配置。
    pub config: ConfigHandle,
    /// 账号库快照。
    pub accounts: Arc<RwLock<AccountStore>>,
    /// 解析后的文件路径。
    pub paths: Paths,
    pool: RwLock<AccountPool>,
    kiro: RwLock<KiroClient>,
    pub meter: Arc<Meter>,
    notifier: RwLock<Notifier>,
    pub tokenizer: TokenCountCache,
    pub stats: Arc<StatsStore>,
    pub models: Arc<ModelCache>,
    model_refresh: tokio::sync::Notify,
    refresher: RwLock<TokenRefresher>,
    tls_config: RwLock<Option<axum_server::tls_rustls::RustlsConfig>>,
    runtime_handle: Option<tokio::runtime::Handle>,
    runtime_config: RwLock<Config>,
    pub admission: Arc<AdmissionGate>,
    pub connections: Arc<AdmissionGate>,
    pub body_budget: Arc<BodyBudget>,
    pub keepalive: KeepaliveHub,
    pub prompt_cache: PromptCacheTracker,
    pub task_registry: TaskRegistry,
    /// Dynamically managed API proxy listeners.
    pub proxy_services: Arc<crate::http::ProxyServiceManager>,
    /// Shared graceful-shutdown token, including automatic quota shutdown.
    pub shutdown: CancellationToken,
    quota_shutdown_started: AtomicBool,
    /// 启动时刻。
    pub started_at: Instant,
    config_reloaded_at: AtomicI64,
    account_mutation: tokio::sync::Mutex<()>,
    config_mutation: tokio::sync::Mutex<()>,
}

impl AppState {
    /// 构造共享状态。
    #[cfg(test)]
    pub fn new(paths: Paths, config: ConfigHandle, accounts: AccountStore) -> Self {
        let current = config.current();
        let meter = Meter::empty(&paths.daily_file, &current.api_key);
        let stats = Arc::new(StatsStore::empty(&paths.stats_file));
        Self::build(paths, config, accounts, meter, stats)
            .expect("default runtime components must initialize")
    }

    pub async fn load(
        paths: Paths,
        config: ConfigHandle,
        mut accounts: AccountStore,
    ) -> anyhow::Result<Self> {
        let current = config.current();
        accounts.set_compression_threshold(current.storage.compression_threshold);
        accounts.set_incremental_write(current.storage.incremental_write);
        let meter = Meter::load(&paths.daily_file, &current.api_key).await?;
        let stats = Arc::new(StatsStore::load(&paths.stats_file).await?);
        Self::build(paths, config, accounts, meter, stats)
    }

    fn build(
        paths: Paths,
        config: ConfigHandle,
        mut accounts: AccountStore,
        meter: Arc<Meter>,
        stats: Arc<StatsStore>,
    ) -> anyhow::Result<Self> {
        let current = config.current();
        meter.set_daily_limit(current.pool.daily_credit_limit);
        accounts.set_compression_threshold(current.storage.compression_threshold);
        accounts.set_incremental_write(current.storage.incremental_write);
        let overrides = EndpointOverrides {
            codewhisperer_url: std::env::var("KPROXY_CODEWHISPERER_URL").ok(),
            amazonq_url: std::env::var("KPROXY_AMAZONQ_URL").ok(),
        };
        let kiro = KiroClient::new(current.upstream.clone(), overrides)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let refresher = TokenRefresher::new(current.effective_token_refresh_before_expiry())
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let pool = AccountPool::new(accounts.all().to_vec(), current.pool.clone());
        let notifier = Notifier::new(current.webhook.clone(), current.notify.clone(), 1_024);
        let admission_limit = current.server.max_concurrent_requests.max(1);
        let connection_limit = current.server.max_connections.max(1);
        Ok(Self {
            config,
            accounts: Arc::new(RwLock::new(accounts)),
            paths,
            pool: RwLock::new(pool),
            kiro: RwLock::new(kiro),
            meter,
            notifier: RwLock::new(notifier),
            tokenizer: TokenCountCache::new(512).map_err(anyhow::Error::msg)?,
            stats,
            models: Arc::new(ModelCache::default()),
            model_refresh: tokio::sync::Notify::new(),
            refresher: RwLock::new(refresher),
            tls_config: RwLock::new(None),
            runtime_handle: tokio::runtime::Handle::try_current().ok(),
            runtime_config: RwLock::new(current.as_ref().clone()),
            admission: Arc::new(AdmissionGate::new(admission_limit)),
            connections: Arc::new(AdmissionGate::new(connection_limit)),
            body_budget: Arc::new(BodyBudget::new(128 * 1024 * 1024)),
            keepalive: KeepaliveHub::new(),
            prompt_cache: PromptCacheTracker::default(),
            task_registry: TaskRegistry::default(),
            proxy_services: Arc::new(crate::http::ProxyServiceManager::default()),
            shutdown: CancellationToken::new(),
            quota_shutdown_started: AtomicBool::new(false),
            started_at: Instant::now(),
            config_reloaded_at: AtomicI64::new(0),
            account_mutation: tokio::sync::Mutex::new(()),
            config_mutation: tokio::sync::Mutex::new(()),
        })
    }

    /// 运行时长。
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// 记录配置重载时间。
    pub fn mark_config_reloaded(&self, at: i64) {
        self.config_reloaded_at.store(at, Ordering::Relaxed);
    }

    /// 上次配置重载时间。
    pub fn config_reloaded_at(&self) -> Option<i64> {
        let value = self.config_reloaded_at.load(Ordering::Relaxed);
        (value != 0).then_some(value)
    }

    /// Ensures quota exhaustion notifications and shutdown are initiated once.
    pub fn begin_quota_shutdown(&self) -> bool {
        self.quota_shutdown_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// 只读访问账号库。
    pub fn with_accounts<R>(&self, inspect: impl FnOnce(&AccountStore) -> R) -> R {
        match self.accounts.read() {
            Ok(guard) => inspect(&guard),
            Err(poisoned) => inspect(&poisoned.into_inner()),
        }
    }

    /// 整体替换账号库快照。
    pub fn replace_accounts(&self, next: AccountStore) {
        match self.accounts.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// 应用账号文件的外部变更，并用新快照重建调度池。
    pub async fn apply_account_file_reload(&self, mut next: AccountStore) {
        let config = self.config.current();
        next.set_compression_threshold(config.storage.compression_threshold);
        next.set_incremental_write(config.storage.incremental_write);
        let invalidated_ids = self.with_accounts(|current| {
            current
                .all()
                .iter()
                .filter_map(|account| match next.find(&account.id) {
                    None => Some(account.id.clone()),
                    Some(replacement)
                        if replacement.credentials.access_token
                            != account.credentials.access_token
                            || replacement.credentials.auth_method
                                != account.credentials.auth_method =>
                    {
                        Some(account.id.clone())
                    }
                    Some(_) => None,
                })
                .collect::<Vec<_>>()
        });
        let snapshot = next.all().to_vec();
        self.replace_accounts(next);
        self.pool().replace_accounts(snapshot).await;
        let endpoint_cache = self.kiro().endpoint_cache();
        for account_id in invalidated_ids {
            endpoint_cache.clear_account(&account_id);
        }
        self.request_model_refresh();
    }

    /// 请求模型发现任务尽快刷新，不绕过任务自身的 singleflight 保护。
    pub fn request_model_refresh(&self) {
        self.model_refresh.notify_one();
    }

    /// 等待账号或运维操作触发一次模型目录刷新。
    pub async fn wait_for_model_refresh(&self) {
        self.model_refresh.notified().await;
    }

    /// 串行化账号的「复制、修改、落盘、提交」事务。
    pub async fn lock_account_mutation(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.account_mutation.lock().await
    }

    /// Serializes config mutations initiated through the admin API.
    pub async fn lock_config_mutation(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.config_mutation.lock().await
    }

    /// Starts, stops, or replaces proxy listeners to match a config snapshot.
    pub async fn reconcile_proxy_services(
        self: &Arc<Self>,
        config: &Config,
    ) -> Vec<(String, String)> {
        self.proxy_services
            .reconcile(Arc::clone(self), &config.proxy_service)
            .await
    }

    /// 事务化应用热配置；任一代理监听启动失败时恢复上一份运行配置。
    pub async fn apply_config_transaction(self: &Arc<Self>, next: &Config) -> Result<(), String> {
        let _mutation = self.lock_config_mutation().await;
        let previous = self.runtime_config_snapshot();
        self.config.replace(next.clone());
        self.apply_runtime_config(next);
        let failures = self.reconcile_proxy_services(next).await;
        if failures.is_empty() {
            return Ok(());
        }

        let apply_error = format_service_failures(&failures);
        self.config.replace(previous.clone());
        self.apply_runtime_config(&previous);
        let rollback_failures = self.reconcile_proxy_services(&previous).await;
        if rollback_failures.is_empty() {
            Err(format!(
                "proxy service apply failed; previous config restored: {apply_error}"
            ))
        } else {
            Err(format!(
                "proxy service apply failed ({apply_error}); rollback also failed: {}",
                format_service_failures(&rollback_failures)
            ))
        }
    }

    /// 获取最后一次完整应用到运行时组件的配置快照。
    pub fn runtime_config_snapshot(&self) -> Config {
        read_lock(&self.runtime_config).clone()
    }

    /// 管理面 socket 路径。
    pub fn admin_socket(&self) -> PathBuf {
        PathBuf::from(&self.config.current().admin.socket)
    }

    pub fn pool(&self) -> AccountPool {
        read_lock(&self.pool).clone()
    }

    pub fn kiro(&self) -> KiroClient {
        read_lock(&self.kiro).clone()
    }

    pub fn notifier(&self) -> Notifier {
        read_lock(&self.notifier).clone()
    }

    pub fn refresher(&self) -> TokenRefresher {
        read_lock(&self.refresher).clone()
    }

    /// Refreshes one account and immediately re-enables endpoints rejected by
    /// the previous token. Successful endpoint preferences are retained.
    pub async fn refresh_account_token(
        &self,
        pool: &AccountPool,
        account_id: &str,
        force: bool,
    ) -> Result<bool, RefreshError> {
        let changed = self
            .refresher()
            .refresh_account(pool, account_id, force)
            .await?;
        if changed {
            self.kiro().endpoint_cache().clear_failures(account_id);
        }
        Ok(changed)
    }

    /// Refreshes every expiring account through the same cache-invalidation
    /// path as request-triggered and manual refreshes.
    pub async fn refresh_expiring_tokens(&self, pool: &AccountPool) -> TokenRefreshReport {
        let before_expiry = self
            .config
            .current()
            .effective_token_refresh_before_expiry();
        let accounts = pool.snapshot().await;
        let mut report = TokenRefreshReport {
            checked: accounts.len(),
            eligible: 0,
            refreshed: Vec::new(),
            failures: Vec::new(),
        };
        for account in accounts {
            if account.is_token_expiring(crate::meter::now_secs(), before_expiry) {
                report.eligible += 1;
                let account_id = account.id.clone();
                let account_name = account.display_name().to_owned();
                match self.refresh_account_token(pool, &account.id, false).await {
                    Ok(true) => report.refreshed.push((account_id, account_name)),
                    Ok(false) => {}
                    Err(error) => report.failures.push((account_id, account_name, error)),
                }
            }
        }
        report
    }

    pub fn install_tls_config(&self, config: axum_server::tls_rustls::RustlsConfig) {
        *write_lock(&self.tls_config) = Some(config);
    }

    /// 将文件热重载同步到依赖配置构造的运行时组件。
    pub fn apply_runtime_config(&self, next: &Config) {
        let previous = read_lock(&self.runtime_config).clone();
        if serde_json::to_string(&previous.log).ok() != serde_json::to_string(&next.log).ok() {
            if let Err(error) = crate::logging::reload_config(&next.log) {
                tracing::warn!(%error, "failed to hot-reload log configuration");
            }
        }
        self.meter.replace_configs(&next.api_key);
        self.meter.set_daily_limit(next.pool.daily_credit_limit);
        self.admission
            .set_maximum(next.server.max_concurrent_requests.max(1));
        self.connections
            .set_maximum(next.server.max_connections.max(1));
        match self.accounts.write() {
            Ok(mut accounts) => {
                accounts.set_compression_threshold(next.storage.compression_threshold);
                accounts.set_incremental_write(next.storage.incremental_write);
            }
            Err(poisoned) => {
                let mut accounts = poisoned.into_inner();
                accounts.set_compression_threshold(next.storage.compression_threshold);
                accounts.set_incremental_write(next.storage.incremental_write);
            }
        }
        if serde_json::to_string(&previous.pool).ok() != serde_json::to_string(&next.pool).ok() {
            self.pool().update_config(next.pool.clone());
        }
        let upstream_changed = serde_json::to_string(&previous.upstream).ok()
            != serde_json::to_string(&next.upstream).ok();
        if upstream_changed {
            let overrides = EndpointOverrides {
                codewhisperer_url: std::env::var("KPROXY_CODEWHISPERER_URL").ok(),
                amazonq_url: std::env::var("KPROXY_AMAZONQ_URL").ok(),
            };
            if let Ok(client) = KiroClient::new(next.upstream.clone(), overrides) {
                *write_lock(&self.kiro) = client;
            }
        }
        if previous.upstream.token_refresh_before_expiry
            != next.upstream.token_refresh_before_expiry
            || previous.tasks.token_refresh_interval_ms != next.tasks.token_refresh_interval_ms
        {
            if let Ok(refresher) = TokenRefresher::new(next.effective_token_refresh_before_expiry())
            {
                *write_lock(&self.refresher) = refresher;
            }
        }
        if previous.server.tls.enabled
            && next.server.tls.enabled
            && serde_json::to_string(&previous.server.tls).ok()
                != serde_json::to_string(&next.server.tls).ok()
        {
            if let (Some(tls), Some(runtime)) = (
                read_lock(&self.tls_config).clone(),
                self.runtime_handle.as_ref(),
            ) {
                let next_tls = next.server.tls.clone();
                runtime.spawn(async move {
                    let result = if let (Some(cert), Some(key)) =
                        (next_tls.cert.as_ref(), next_tls.key.as_ref())
                    {
                        tls.reload_from_pem(cert.as_bytes().to_vec(), key.as_bytes().to_vec())
                            .await
                    } else if let (Some(cert), Some(key)) =
                        (next_tls.cert_path.as_ref(), next_tls.key_path.as_ref())
                    {
                        tls.reload_from_pem_file(cert, key).await
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "TLS certificate and key are required",
                        ))
                    };
                    if let Err(error) = result {
                        tracing::error!(%error, "failed to hot-reload TLS certificate");
                    }
                });
            }
        }
        if serde_json::to_string(&previous.webhook).ok()
            != serde_json::to_string(&next.webhook).ok()
            || serde_json::to_string(&previous.notify).ok()
                != serde_json::to_string(&next.notify).ok()
        {
            let current = self.notifier();
            *write_lock(&self.notifier) =
                current.reconfigured(next.webhook.clone(), next.notify.clone(), 1_024);
        }
        *write_lock(&self.runtime_config) = next.clone();
    }

    /// 根据 P99 延迟收缩或恢复全局准入上限，返回新的有效上限。
    pub async fn adjust_admission(&self, p99_ms: u64) -> usize {
        let adaptive = self.config.current().server.adaptive.clone();
        let configured = self.config.current().server.max_concurrent_requests.max(1);
        if !adaptive.enabled {
            self.admission.set_maximum(configured);
            return configured;
        }
        let current = self.admission.maximum();
        let step = (configured / 10).max(1);
        let next = if p99_ms > adaptive.p99_degrade_ms {
            current.saturating_sub(step).max(1)
        } else if p99_ms > 0 && p99_ms < adaptive.p99_recover_ms {
            current.saturating_add(step).min(configured)
        } else {
            current.min(configured)
        };
        self.admission.set_maximum(next);
        next
    }
}

fn format_service_failures(failures: &[(String, String)]) -> String {
    failures
        .iter()
        .map(|(service_id, error)| format!("{service_id}: {error}"))
        .collect::<Vec<_>>()
        .join("; ")
}

pub struct AdmissionGate {
    maximum: AtomicUsize,
    current: AtomicUsize,
}

impl AdmissionGate {
    fn new(maximum: usize) -> Self {
        Self {
            maximum: AtomicUsize::new(maximum.max(1)),
            current: AtomicUsize::new(0),
        }
    }

    pub fn maximum(&self) -> usize {
        self.maximum.load(Ordering::Acquire)
    }

    pub fn set_maximum(&self, maximum: usize) {
        self.maximum.store(maximum.max(1), Ordering::Release);
    }

    pub fn try_acquire(self: &Arc<Self>) -> Option<AdmissionGuard> {
        self.current
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                (current < self.maximum()).then_some(current + 1)
            })
            .ok()
            .map(|_| AdmissionGuard {
                gate: Arc::clone(self),
            })
    }
}

pub struct AdmissionGuard {
    gate: Arc<AdmissionGate>,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        self.gate.current.fetch_sub(1, Ordering::AcqRel);
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct BodyBudget {
    maximum: usize,
    current: AtomicUsize,
}

impl BodyBudget {
    fn new(maximum: usize) -> Self {
        Self {
            maximum,
            current: AtomicUsize::new(0),
        }
    }

    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Option<BodyGuard> {
        let result = self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.maximum)
            });
        result.ok().map(|_| BodyGuard {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

pub struct BodyGuard {
    budget: Arc<BodyBudget>,
    bytes: usize,
}

impl BodyGuard {
    pub fn reserve_more(&mut self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let reserved = self
            .budget
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.budget.maximum)
            })
            .is_ok();
        if reserved {
            self.bytes = self.bytes.saturating_add(bytes);
        }
        reserved
    }
}

impl Drop for BodyGuard {
    fn drop(&mut self) {
        self.budget.current.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod token_refresh_report_tests {
    use super::TokenRefreshReport;

    #[test]
    fn summary_distinguishes_checks_from_actual_refreshes() {
        let report = TokenRefreshReport {
            checked: 3,
            eligible: 1,
            refreshed: vec![("acc_one".into(), "Team account".into())],
            failures: Vec::new(),
        };
        assert_eq!(
            report.summary(),
            "ok: 3 checked, 1 eligible, 1 refreshed, 0 failures"
        );
    }
}

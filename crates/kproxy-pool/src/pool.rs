use std::cmp::Ordering as CmpOrdering;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use kproxy_core::account::Account;
use kproxy_core::config::PoolConfig;
use rand::Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Notify, RwLock};

use crate::state::{AccountHealth, AccountState};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PoolError {
    #[error("no account can serve model {0}")]
    NoAvailableAccount(String),
    #[error("account request queue is full")]
    QueueFull,
    #[error("timed out waiting for an account permit")]
    QueueTimeout,
    #[error("all matching accounts have exhausted their credit allowance")]
    CreditsExhausted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreExplanation {
    pub account_id: String,
    pub score: f64,
    pub active_factor: f64,
    pub credit_factor: f64,
    pub idle_factor: f64,
    pub eligible: bool,
    pub reason: String,
}

#[derive(Clone)]
pub struct AccountPool {
    accounts: Arc<RwLock<IndexMap<String, Arc<AccountState>>>>,
    config: Arc<StdRwLock<PoolConfig>>,
    notify: Arc<Notify>,
    queued: Arc<AtomicUsize>,
}

pub struct AccountLease {
    state: Arc<AccountState>,
    notify: Arc<Notify>,
}

struct QueueGuard {
    queued: Arc<AtomicUsize>,
}

impl QueueGuard {
    fn enter(queued: Arc<AtomicUsize>) -> (Self, usize) {
        let position = queued.fetch_add(1, Ordering::AcqRel);
        (Self { queued }, position)
    }
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        self.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

impl AccountLease {
    pub async fn account(&self) -> Account {
        self.state.account.read().await.clone()
    }

    pub fn account_id(&self) -> String {
        self.state
            .account
            .try_read()
            .map(|account| account.id.clone())
            .unwrap_or_default()
    }

    pub async fn settle_credits(&mut self, actual: f64) {
        if !actual.is_finite() || actual <= 0.0 {
            return;
        }
        // This optimistic increment only feeds account-selection scoring. The
        // service/API-key ledgers remain owned by kproxyd::meter, and the next
        // upstream status refresh replaces this estimate with authoritative
        // usage instead of accumulating it a second time.
        let mut account = self.state.account.write().await;
        let Some(usage) = account.usage.as_mut() else {
            return;
        };
        let optimistic = usage.current + actual;
        usage.current = if usage.limit > 0.0 {
            optimistic.min(usage.limit)
        } else {
            optimistic
        };
        usage.percent_used = if usage.limit > 0.0 {
            (usage.current / usage.limit * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        usage.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(usage.updated_at);
    }
}

impl Drop for AccountLease {
    fn drop(&mut self) {
        self.state.release_slot();
        self.state.mark_used_now();
        self.notify.notify_waiters();
    }
}

impl AccountPool {
    pub fn new(accounts: Vec<Account>, config: PoolConfig) -> Self {
        let states = accounts
            .into_iter()
            .map(|account| {
                let id = account.id.clone();
                let state = AccountState::new(account, config.max_concurrent_per_account);
                (id, state)
            })
            .collect();
        Self {
            accounts: Arc::new(RwLock::new(states)),
            config: Arc::new(StdRwLock::new(config)),
            notify: Arc::new(Notify::new()),
            queued: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub async fn acquire(
        &self,
        model: &str,
        estimated_credits: f64,
        prefer_ids: &[String],
    ) -> Result<AccountLease, PoolError> {
        if self.candidates(model, prefer_ids).await.is_empty() {
            return Err(PoolError::NoAvailableAccount(model.into()));
        }
        if let Some(lease) = self.try_acquire(model, estimated_credits, prefer_ids).await {
            return Ok(lease);
        }
        let config = self.config();
        let (_queue_guard, queue_position) = QueueGuard::enter(Arc::clone(&self.queued));
        let queue_disabled = config.max_queue_size == 0;
        let overflow = queue_disabled || queue_position >= config.max_queue_size;
        if overflow && config.queue_full_wait_ms == 0 {
            return Err(PoolError::QueueFull);
        }
        let wait = if queue_disabled {
            0
        } else if overflow {
            config.queue_full_wait_ms
        } else {
            config.max_queue_wait_ms
        };
        let deadline = Instant::now() + Duration::from_millis(wait);
        loop {
            if wait == 0 {
                return Err(if overflow {
                    PoolError::QueueFull
                } else {
                    PoolError::NoAvailableAccount(model.into())
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(PoolError::QueueTimeout);
            }
            if tokio::time::timeout(deadline - now, self.notify.notified())
                .await
                .is_err()
            {
                return Err(PoolError::QueueTimeout);
            }
            if let Some(lease) = self.try_acquire(model, estimated_credits, prefer_ids).await {
                return Ok(lease);
            }
        }
    }

    /// Acquires an eligible account while guaranteeing that accounts already
    /// attempted by the caller cannot be selected again.
    pub async fn acquire_excluding(
        &self,
        model: &str,
        estimated_credits: f64,
        excluded_ids: &HashSet<String>,
    ) -> Result<AccountLease, PoolError> {
        let candidate_ids = self
            .snapshot()
            .await
            .into_iter()
            .filter(|account| !excluded_ids.contains(&account.id))
            .map(|account| account.id)
            .collect::<Vec<_>>();
        if candidate_ids.is_empty() {
            return Err(PoolError::NoAvailableAccount(model.into()));
        }
        self.acquire(model, estimated_credits, &candidate_ids).await
    }

    async fn try_acquire(
        &self,
        model: &str,
        _estimated_credits: f64,
        prefer_ids: &[String],
    ) -> Option<AccountLease> {
        let mut candidates = self.candidates(model, prefer_ids).await;
        candidates
            .sort_by(|left, right| left.0.partial_cmp(&right.0).unwrap_or(CmpOrdering::Equal));
        let maximum = self.config().max_concurrent_per_account;
        for (_, state) in candidates {
            if !state.try_acquire_slot(maximum) {
                continue;
            }
            state.mark_used_now();
            return Some(AccountLease {
                state,
                notify: Arc::clone(&self.notify),
            });
        }
        None
    }

    async fn candidates(
        &self,
        model: &str,
        prefer_ids: &[String],
    ) -> Vec<(f64, Arc<AccountState>)> {
        let accounts = self.accounts.read().await;
        let mut output = Vec::new();
        for state in accounts.values() {
            if !prefer_ids.is_empty() {
                let id = state.account.read().await.id.clone();
                if !prefer_ids.contains(&id) {
                    continue;
                }
            }
            if self.eligible(state, model).await {
                let jitter = rand::thread_rng().gen_range(0.0..f64::EPSILON * 1024.0);
                output.push((self.score(state).await.score + jitter, Arc::clone(state)));
            }
        }
        output
    }

    async fn eligible(&self, state: &Arc<AccountState>, model: &str) -> bool {
        if state.cooling_expired().await {
            state.set_health(AccountHealth::Available);
        }
        if state.health() != AccountHealth::Available {
            return false;
        }
        let account = state.account.read().await;
        if !account.enabled || account.credit_exhausted {
            return false;
        }
        if low_credit(&account, &self.config()) {
            state.set_health(AccountHealth::Exhausted);
            return false;
        }
        if model.is_empty() {
            return true;
        }
        if let Some(models) = state.supported_models.read().await.as_ref() {
            let models = models.iter().cloned().collect::<Vec<_>>();
            return kproxy_translate::model::can_resolve_dynamic_model(model, &models);
        }
        can_serve_subscription(&account, model)
    }

    async fn score(&self, state: &Arc<AccountState>) -> ScoreExplanation {
        let account = state.account.read().await;
        let config = self.config();
        let active_divisor = if config.max_concurrent_per_account == 0 {
            10.0
        } else {
            config.max_concurrent_per_account as f64
        };
        let active_factor = (state.active() as f64 / active_divisor).min(1.0);
        let credit_factor = account
            .usage
            .as_ref()
            .filter(|usage| usage.limit > 0.0)
            .map(|usage| (usage.current / usage.limit).clamp(0.0, 1.0))
            .unwrap_or(0.0);
        let idle_ms = now_ms().saturating_sub(state.last_used_ms());
        let idle_factor =
            1.0 - (idle_ms as f64 / config.balance.idle_window_ms.max(1) as f64).min(1.0);
        let score = config.balance.weight_active * active_factor
            + config.balance.weight_credit * credit_factor
            + config.balance.weight_idle * idle_factor;
        ScoreExplanation {
            account_id: account.id.clone(),
            score,
            active_factor,
            credit_factor,
            idle_factor,
            eligible: true,
            reason: "available".into(),
        }
    }

    pub async fn explain(&self, model: &str) -> Vec<ScoreExplanation> {
        let accounts = self.accounts.read().await;
        let mut explanations = Vec::new();
        for state in accounts.values() {
            if self.eligible(state, model).await {
                explanations.push(self.score(state).await);
            } else {
                let account = state.account.read().await;
                explanations.push(ScoreExplanation {
                    account_id: account.id.clone(),
                    score: f64::INFINITY,
                    active_factor: 0.0,
                    credit_factor: 0.0,
                    idle_factor: 0.0,
                    eligible: false,
                    reason: format!("{:?}", state.health()).to_ascii_lowercase(),
                });
            }
        }
        explanations.sort_by(|left, right| {
            left.score
                .partial_cmp(&right.score)
                .unwrap_or(CmpOrdering::Equal)
        });
        explanations
    }

    pub async fn record_success(&self, account_id: &str) {
        if let Some(state) = self.get(account_id).await {
            state.consecutive_errors.store(0, Ordering::Relaxed);
            if state.health() == AccountHealth::Cooling {
                state.set_health(AccountHealth::Available);
            }
        }
    }

    pub async fn record_error(&self, account_id: &str) {
        let Some(state) = self.get(account_id).await else {
            return;
        };
        let count = state.consecutive_errors.fetch_add(1, Ordering::Relaxed) + 1;
        let config = self.config();
        let duration = if count >= config.cooldown.max_error_count {
            Duration::from_millis(config.cooldown.cooldown_ms)
        } else {
            Duration::from_millis(config.cooldown.error_cooldown_ms)
        };
        state.cool_for(duration).await;
    }

    pub async fn record_quota_error(&self, account_id: &str) {
        let Some(state) = self.get(account_id).await else {
            return;
        };
        let now = Instant::now();
        let config = self.config();
        let window = Duration::from_millis(config.cooldown.quota_error_window_ms);
        let mut errors = state.quota_errors.lock().await;
        while errors
            .front()
            .is_some_and(|time| now.duration_since(*time) > window)
        {
            errors.pop_front();
        }
        errors.push_back(now);
        if errors.len() as u32 >= config.cooldown.quota_error_threshold {
            state.set_health(AccountHealth::Exhausted);
            state.account.write().await.credit_exhausted = true;
        } else {
            state
                .cool_for(Duration::from_millis(config.cooldown.error_cooldown_ms))
                .await;
        }
    }

    pub async fn mark_banned(&self, account_id: &str) {
        if let Some(state) = self.get(account_id).await {
            state.set_health(AccountHealth::Banned);
        }
    }

    pub async fn reset_health(&self, account_id: &str) -> bool {
        let Some(state) = self.get(account_id).await else {
            return false;
        };
        state.reset_health().await;
        self.notify.notify_waiters();
        true
    }

    pub async fn get(&self, account_id: &str) -> Option<Arc<AccountState>> {
        self.accounts.read().await.get(account_id).cloned()
    }

    pub async fn snapshot(&self) -> Vec<Account> {
        let states = self
            .accounts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut output = Vec::with_capacity(states.len());
        for state in states {
            output.push(state.account.read().await.clone());
        }
        output
    }

    /// Returns true only when at least one enabled account can serve the model
    /// and every such account has a persisted or usage-derived credit stop.
    pub async fn all_matching_credit_exhausted(&self, model: &str) -> bool {
        let states = self
            .accounts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let config = self.config();
        let mut matched = false;
        for state in states {
            let account = state.account.read().await;
            if !account.enabled {
                continue;
            }
            let model_matches = if model.is_empty() {
                true
            } else if let Some(models) = state.supported_models.read().await.as_ref() {
                let models = models.iter().cloned().collect::<Vec<_>>();
                kproxy_translate::model::can_resolve_dynamic_model(model, &models)
            } else {
                can_serve_subscription(&account, model)
            };
            if !model_matches {
                continue;
            }
            matched = true;
            if !account.credit_exhausted && !low_credit(&account, &config) {
                return false;
            }
        }
        matched
    }

    pub async fn replace_accounts(&self, accounts: Vec<Account>) {
        let mut next = IndexMap::new();
        let mut current = self.accounts.write().await;
        for account in accounts {
            let id = account.id.clone();
            if let Some(existing) = current.get(&id) {
                *existing.account.write().await = account;
                next.insert(id, Arc::clone(existing));
            } else {
                next.insert(
                    id,
                    AccountState::new(account, self.config().max_concurrent_per_account),
                );
            }
        }
        *current = next;
        self.notify.notify_waiters();
    }

    pub async fn health_counts(&self) -> [usize; 5] {
        let accounts = self.accounts.read().await;
        let mut counts = [0; 5];
        for state in accounts.values() {
            counts[state.health() as usize] += 1;
        }
        counts
    }

    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    pub async fn active(&self) -> usize {
        self.accounts
            .read()
            .await
            .values()
            .map(|state| state.active())
            .sum()
    }

    pub async fn reset_daily_credits(&self) {
        self.notify.notify_waiters();
    }

    pub fn update_config(&self, config: PoolConfig) {
        match self.config.write() {
            Ok(mut current) => *current = config,
            Err(poisoned) => *poisoned.into_inner() = config,
        }
        self.notify.notify_waiters();
    }

    fn config(&self) -> PoolConfig {
        self.config
            .read()
            .map(|config| config.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }
}

fn low_credit(account: &Account, config: &PoolConfig) -> bool {
    let Some(usage) = &account.usage else {
        return false;
    };
    let remaining = (usage.limit - usage.current).max(0.0);
    let ratio = if usage.limit > 0.0 {
        remaining / usage.limit
    } else {
        0.0
    };
    (config.low_credit_ratio > 0.0 && ratio <= config.low_credit_ratio)
        || (config.low_credit_min_remaining > 0.0
            && usage.limit > 0.0
            && remaining <= config.low_credit_min_remaining)
}

fn can_serve_subscription(account: &Account, model: &str) -> bool {
    kproxy_kiro::static_subscription_can_serve(
        account
            .subscription
            .as_ref()
            .map(|subscription| subscription.kind),
        model,
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use kproxy_core::account::{
        Account, AuthMethod, Credentials, Subscription, SubscriptionKind, Usage,
    };

    use super::*;

    fn account(id: &str, used: f64, subscription: SubscriptionKind) -> Account {
        Account {
            id: id.into(),
            email: format!("{id}@example.com"),
            label: None,
            enabled: true,
            machine_id: "a".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: Credentials {
                access_token: "at".into(),
                refresh_token: None,
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: i64::MAX,
                auth_method: AuthMethod::Idc,
            },
            usage: Some(Usage {
                current: used,
                limit: 100.0,
                percent_used: used,
                next_reset_date: None,
                updated_at: 0,
            }),
            subscription: Some(Subscription {
                kind: subscription,
                title: None,
                raw_type: None,
                expires_at: None,
                days_remaining: None,
            }),
            tags: Vec::new(),
            created_at: 0,
            credit_exhausted: false,
        }
    }

    fn immediate_config() -> PoolConfig {
        PoolConfig {
            max_queue_size: 0,
            queue_full_wait_ms: 0,
            max_queue_wait_ms: 0,
            ..PoolConfig::default()
        }
    }

    #[tokio::test]
    async fn credit_weight_prefers_the_less_used_account() {
        let pool = AccountPool::new(
            vec![
                account("high", 90.0, SubscriptionKind::Pro),
                account("low", 10.0, SubscriptionKind::Pro),
            ],
            immediate_config(),
        );
        let lease = pool
            .acquire("claude-sonnet", 0.0, &[])
            .await
            .expect("lease");
        assert_eq!(lease.account().await.id, "low");
    }

    #[tokio::test]
    async fn free_account_is_skipped_for_opus() {
        let pool = AccountPool::new(
            vec![
                account("free", 0.0, SubscriptionKind::Free),
                account("pro", 80.0, SubscriptionKind::Pro),
            ],
            immediate_config(),
        );
        let lease = pool
            .acquire("claude-opus-4", 0.0, &[])
            .await
            .expect("lease");
        assert_eq!(lease.account().await.id, "pro");
    }

    #[tokio::test]
    async fn free_account_is_skipped_for_non_opus_premium_model() {
        let pool = AccountPool::new(
            vec![
                account("free", 0.0, SubscriptionKind::Free),
                account("pro", 80.0, SubscriptionKind::Pro),
            ],
            immediate_config(),
        );
        let lease = pool
            .acquire("claude-sonnet-4.6", 0.0, &[])
            .await
            .expect("lease");
        assert_eq!(lease.account().await.id, "pro");
    }

    #[tokio::test]
    async fn settlement_optimistically_updates_account_usage_and_score() {
        let pool = AccountPool::new(
            vec![account("only", 10.0, SubscriptionKind::Pro)],
            immediate_config(),
        );
        let before = pool.explain("claude-sonnet-4.6").await.remove(0);
        let mut lease = pool
            .acquire("claude-sonnet-4.6", 5.5, &[])
            .await
            .expect("lease");
        lease.settle_credits(5.5).await;
        let account = lease.account().await;
        let usage = account.usage.expect("usage");
        assert_eq!(usage.current, 15.5);
        assert_eq!(usage.percent_used, 15.5);
        drop(lease);
        let after = pool.explain("claude-sonnet-4.6").await.remove(0);
        assert!(after.credit_factor > before.credit_factor);
    }

    #[tokio::test]
    async fn exclusions_prevent_retrying_the_same_failed_account() {
        let pool = AccountPool::new(
            vec![
                account("preferred", 0.0, SubscriptionKind::Pro),
                account("fallback", 50.0, SubscriptionKind::Pro),
            ],
            immediate_config(),
        );
        let excluded = HashSet::from(["preferred".to_string()]);
        let lease = pool
            .acquire_excluding("claude-sonnet", 0.0, &excluded)
            .await
            .expect("fallback lease");
        assert_eq!(lease.account().await.id, "fallback");
        let all = HashSet::from(["preferred".to_string(), "fallback".to_string()]);
        assert!(matches!(
            pool.acquire_excluding("claude-sonnet", 0.0, &all).await,
            Err(PoolError::NoAvailableAccount(_))
        ));
    }

    #[tokio::test]
    async fn dropping_a_lease_releases_permit_and_credit_reservation() {
        let mut config = immediate_config();
        config.max_concurrent_per_account = 1;
        config.daily_credit_limit = 1.0;
        let pool = AccountPool::new(vec![account("only", 0.0, SubscriptionKind::Pro)], config);
        let first = pool
            .acquire("claude-sonnet", 0.8, &[])
            .await
            .expect("first");
        assert!(matches!(
            pool.acquire("claude-sonnet", 0.1, &[]).await,
            Err(PoolError::QueueFull)
        ));
        drop(first);
        tokio::task::yield_now().await;
        let next = pool.acquire("claude-sonnet", 0.3, &[]).await.expect("next");
        assert_eq!(next.account().await.id, "only");
    }

    #[tokio::test]
    async fn cancelling_a_waiter_releases_its_queue_slot() {
        let config = PoolConfig {
            max_concurrent_per_account: 1,
            max_queue_size: 1,
            max_queue_wait_ms: 2_000,
            ..PoolConfig::default()
        };
        let pool = AccountPool::new(vec![account("only", 0.0, SubscriptionKind::Pro)], config);
        let held = pool.acquire("claude-sonnet", 0.0, &[]).await.expect("held");
        let waiting_pool = pool.clone();
        let waiter =
            tokio::spawn(async move { waiting_pool.acquire("claude-sonnet", 0.0, &[]).await });

        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.queued() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiter entered queue");
        waiter.abort();
        let _cancelled = waiter.await;

        assert_eq!(pool.queued(), 0);
        drop(held);
    }

    #[tokio::test]
    async fn releasing_one_account_wakes_a_compatible_waiter() {
        let config = PoolConfig {
            max_concurrent_per_account: 1,
            max_queue_size: 4,
            max_queue_wait_ms: 2_000,
            ..PoolConfig::default()
        };
        let pool = AccountPool::new(
            vec![
                account("a", 0.0, SubscriptionKind::Pro),
                account("b", 0.0, SubscriptionKind::Pro),
            ],
            config,
        );
        pool.get("a")
            .await
            .expect("a")
            .set_supported_models(["model-a".to_string()])
            .await;
        pool.get("b")
            .await
            .expect("b")
            .set_supported_models(["model-b".to_string()])
            .await;
        let held = pool.acquire("model-a", 0.0, &[]).await.expect("held");
        let waiting_pool = pool.clone();
        let waiter = tokio::spawn(async move { waiting_pool.acquire("model-a", 0.0, &[]).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held);
        let lease = waiter.await.expect("join").expect("woken");
        assert_eq!(lease.account().await.id, "a");
    }

    #[tokio::test]
    async fn service_daily_credit_limit_is_not_multiplied_inside_account_pool() {
        let config = PoolConfig {
            daily_credit_limit: 1.0,
            max_queue_size: 0,
            queue_full_wait_ms: 0,
            ..PoolConfig::default()
        };
        let pool = AccountPool::new(vec![account("only", 0.0, SubscriptionKind::Pro)], config);
        assert!(pool.acquire("claude-sonnet", 1.1, &[]).await.is_ok());
    }

    #[tokio::test]
    async fn client_model_alias_is_matched_against_discovered_kiro_models() {
        let pool = AccountPool::new(
            vec![account("alias", 0.0, SubscriptionKind::Pro)],
            immediate_config(),
        );
        pool.get("alias")
            .await
            .expect("account")
            .set_supported_models(["claude-sonnet-4.6".into()])
            .await;
        let lease = pool
            .acquire("claude-4.6-sonnet", 0.0, &[])
            .await
            .expect("client alias should resolve through the discovered catalog");
        assert_eq!(lease.account().await.id, "alias");
    }

    #[tokio::test]
    async fn reports_exhaustion_only_when_every_matching_account_is_out_of_credit() {
        let mut exhausted = account("out", 100.0, SubscriptionKind::Pro);
        exhausted.credit_exhausted = true;
        let pool = AccountPool::new(
            vec![exhausted, account("ready", 10.0, SubscriptionKind::Pro)],
            immediate_config(),
        );
        for id in ["out", "ready"] {
            pool.get(id)
                .await
                .expect("account")
                .set_supported_models(["claude-sonnet-4-20250514".into()])
                .await;
        }
        assert!(!pool.all_matching_credit_exhausted("claude-sonnet-4").await);
        pool.get("ready")
            .await
            .expect("ready")
            .account
            .write()
            .await
            .credit_exhausted = true;
        assert!(pool.all_matching_credit_exhausted("claude-sonnet-4").await);
        assert!(!pool.all_matching_credit_exhausted("claude-opus-4").await);
    }
}

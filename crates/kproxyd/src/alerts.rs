//! Incident-based operator alerts.

use std::sync::Arc;

use kproxy_core::account::{Account, Usage};
use kproxy_notify::{WebhookEvent, WebhookEventKind};
use kproxy_pool::{account_credit_state, AccountCreditState};

use crate::state::AppState;

const RECOVERY_CONFIRMATIONS: u8 = 2;

#[derive(Debug, Clone)]
enum CreditObservation {
    Unknown,
    Known {
        state: AccountCreditState,
        usage: Option<Usage>,
        generation: Option<u64>,
    },
}

pub async fn sync_quota_incidents(state: &Arc<AppState>) {
    let _sync = state.lock_quota_alert_sync().await;
    let accounts = state.pool().snapshot().await;
    let config = state.runtime_config_snapshot().pool;
    for account in &accounts {
        sync_account_credit_incident(state, account, &config);
    }
    sync_service_quota_inner(state, &accounts).await;
}

pub async fn sync_account_quota(state: &Arc<AppState>, account_id: &str) {
    let _sync = state.lock_quota_alert_sync().await;
    let pool = state.pool();
    let Some(runtime) = pool.get(account_id).await else {
        resolve_account_credit_incidents(state, account_id);
        return;
    };
    let account = runtime.account.read().await.clone();
    let config = state.runtime_config_snapshot().pool;
    sync_account_credit_incident(state, &account, &config);
}

fn sync_account_credit_incident(
    state: &AppState,
    account: &Account,
    config: &kproxy_core::config::PoolConfig,
) {
    if !account.enabled {
        resolve_account_credit_incidents(state, &account.id);
        state.clear_credit_transition(&account.id);
        return;
    }
    let notifier = state.notifier();
    let exhausted_active =
        notifier.incident_active(WebhookEventKind::AccountQuotaExhausted, Some(&account.id));
    let protected_active =
        notifier.incident_active(WebhookEventKind::AccountCreditProtected, Some(&account.id));
    match authoritative_credit_observation(state, account, config) {
        CreditObservation::Unknown => {}
        CreditObservation::Known {
            state: AccountCreditState::Available,
            generation,
            ..
        } => {
            if (exhausted_active || protected_active)
                && !transition_confirmed(
                    state,
                    &account.id,
                    generation,
                    AccountCreditState::Available,
                )
            {
                return;
            }
            resolve_account_credit_incidents(state, &account.id);
            state.clear_credit_transition(&account.id);
        }
        CreditObservation::Known {
            state: AccountCreditState::Protected,
            usage: Some(usage),
            generation,
        } => {
            if exhausted_active
                && !transition_confirmed(
                    state,
                    &account.id,
                    generation,
                    AccountCreditState::Protected,
                )
            {
                return;
            }
            state
                .notifier()
                .resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some(&account.id));
            emit_account_credit_protected(state, account, &usage, config);
            state.clear_credit_transition(&account.id);
        }
        CreditObservation::Known {
            state: AccountCreditState::Exhausted,
            usage,
            ..
        } => {
            state
                .notifier()
                .resolve_incident(WebhookEventKind::AccountCreditProtected, Some(&account.id));
            emit_account_quota(state, account, usage.as_ref());
            state.clear_credit_transition(&account.id);
        }
        CreditObservation::Known {
            state: AccountCreditState::Protected,
            usage: None,
            ..
        } => {}
    }
}

fn transition_confirmed(
    state: &AppState,
    account_id: &str,
    generation: Option<u64>,
    observed: AccountCreditState,
) -> bool {
    let Some(generation) = generation else {
        return false;
    };
    state.observe_credit_transition(account_id, generation, observed) >= RECOVERY_CONFIRMATIONS
}

fn authoritative_credit_observation(
    state: &AppState,
    account: &Account,
    config: &kproxy_core::config::PoolConfig,
) -> CreditObservation {
    let authoritative = state.authoritative_usage(&account.id);
    if account.credit_exhausted {
        let authoritative_exhausted = authoritative.as_ref().is_some_and(|observation| {
            observation.usage.limit > 0.0 && observation.usage.current >= observation.usage.limit
        });
        return CreditObservation::Known {
            state: AccountCreditState::Exhausted,
            usage: authoritative_exhausted
                .then(|| authoritative.as_ref().map(|value| value.usage.clone()))
                .flatten(),
            generation: authoritative.map(|value| value.generation),
        };
    }
    let Some(authoritative) = authoritative else {
        return CreditObservation::Unknown;
    };
    let mut observed = account.clone();
    observed.usage = Some(authoritative.usage.clone());
    observed.credit_exhausted = false;
    CreditObservation::Known {
        state: account_credit_state(&observed, config),
        usage: Some(authoritative.usage),
        generation: Some(authoritative.generation),
    }
}

fn resolve_account_credit_incidents(state: &AppState, account_id: &str) {
    let notifier = state.notifier();
    notifier.resolve_incident(WebhookEventKind::AccountCreditProtected, Some(account_id));
    notifier.resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some(account_id));
}

pub async fn sync_service_quota(state: &Arc<AppState>) {
    let _sync = state.lock_quota_alert_sync().await;
    let accounts = state.pool().snapshot().await;
    sync_service_quota_inner(state, &accounts).await;
}

async fn sync_service_quota_inner(state: &Arc<AppState>, accounts: &[Account]) {
    let config = state.runtime_config_snapshot();
    let mut services = config
        .proxy_service
        .iter()
        .filter(|service| service.enabled)
        .map(|service| service.name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    services.sort_by_key(|name| name.to_ascii_lowercase());
    services.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    if services.is_empty() {
        state
            .notifier()
            .resolve_incident(WebhookEventKind::ServiceQuotaExhausted, None);
        state.clear_credit_transition("__service_quota__");
        return;
    }
    let enabled = accounts
        .iter()
        .filter(|account| account.enabled)
        .collect::<Vec<_>>();
    if enabled.is_empty() {
        state
            .notifier()
            .resolve_incident(WebhookEventKind::ServiceQuotaExhausted, None);
        state.clear_credit_transition("__service_quota__");
        return;
    }
    let observations = enabled
        .iter()
        .map(|account| authoritative_credit_observation(state, account, &config.pool))
        .collect::<Vec<_>>();
    let all_known = observations
        .iter()
        .all(|observation| !matches!(observation, CreditObservation::Unknown));
    let all_exhausted = observations.iter().all(|observation| {
        matches!(
            observation,
            CreditObservation::Known {
                state: AccountCreditState::Exhausted,
                ..
            }
        )
    });
    if !all_exhausted {
        let notifier = state.notifier();
        if !all_known {
            return;
        }
        let active = notifier.incident_active(WebhookEventKind::ServiceQuotaExhausted, None);
        let generation = observations
            .iter()
            .filter_map(|observation| match observation {
                CreditObservation::Known { generation, .. } => *generation,
                CreditObservation::Unknown => None,
            })
            .max();
        if active
            && !transition_confirmed(
                state,
                "__service_quota__",
                generation,
                AccountCreditState::Available,
            )
        {
            return;
        }
        notifier.resolve_incident(WebhookEventKind::ServiceQuotaExhausted, None);
        state.clear_credit_transition("__service_quota__");
        return;
    }
    state.clear_credit_transition("__service_quota__");
    let exhausted = enabled.len();
    let service_names = services
        .iter()
        .map(|name| format!("`{}`", markdown_code(name)))
        .collect::<Vec<_>>()
        .join("、");
    let message = format!(
        "- **代理服务：** {service_names}\n\
         - **账号状态：** {exhausted} / {exhausted} 个启用账号额度耗尽\n\
         - **影响：** 没有可用额度账号，新的 API 代理请求将被拒绝\n\
         - **处理建议：** 补充账号额度，或导入并启用有额度的账号"
    );
    state.notifier().emit(WebhookEvent::new(
        WebhookEventKind::ServiceQuotaExhausted,
        "KProxy API 代理服务额度耗尽",
        message,
    ));
}

pub fn emit_token_refresh_failure(
    state: &AppState,
    account_id: &str,
    account_name: &str,
    error: &str,
) {
    let message = format!(
        "- **账号：** `{}`\n\
         - **账号 ID：** `{}`\n\
         - **失败原因：** `{}`\n\
         - **影响：** Token 无法自动续期，该账号可能无法继续代理请求",
        markdown_code(account_name),
        markdown_code(account_id),
        markdown_code(error),
    );
    let mut event = WebhookEvent::new(
        WebhookEventKind::TokenRefreshFailed,
        "KProxy 账号 Token 刷新失败",
        message,
    );
    event.account_id = Some(account_id.to_owned());
    state.notifier().emit(event);
}

pub fn resolve_token_refresh_failure(state: &AppState, account_id: &str) {
    state
        .notifier()
        .resolve_incident(WebhookEventKind::TokenRefreshFailed, Some(account_id));
}

fn emit_account_quota(state: &AppState, account: &Account, usage: Option<&Usage>) {
    let credit = usage
        .map(|usage| format!("{:.2} / {:.2} credits", usage.current, usage.limit))
        .unwrap_or_else(|| "上游已返回额度耗尽".into());
    let message = format!(
        "- **账号：** `{}`\n\
         - **账号 ID：** `{}`\n\
         - **额度：** `{credit}`\n\
         - **影响：** 该账号已停止参与请求调度",
        markdown_code(account.display_name()),
        markdown_code(&account.id),
    );
    let mut event = WebhookEvent::new(
        WebhookEventKind::AccountQuotaExhausted,
        "KProxy 账号额度耗尽",
        message,
    );
    event.account_id = Some(account.id.clone());
    state.notifier().emit(event);
}

fn emit_account_credit_protected(
    state: &AppState,
    account: &Account,
    usage: &Usage,
    config: &kproxy_core::config::PoolConfig,
) {
    let Some(event) = account_credit_protected_event(account, usage, config) else {
        return;
    };
    state.notifier().emit(event);
}

fn account_credit_protected_event(
    account: &Account,
    usage: &Usage,
    config: &kproxy_core::config::PoolConfig,
) -> Option<WebhookEvent> {
    let usage = (usage.limit > 0.0).then_some(usage)?;
    let remaining = (usage.limit - usage.current).max(0.0);
    let remaining_percent = (remaining / usage.limit * 100.0).clamp(0.0, 100.0);
    let mut thresholds = Vec::new();
    if config.low_credit_ratio > 0.0 {
        thresholds.push(format!(
            "剩余比例 ≤ {:.2}%",
            config.low_credit_ratio * 100.0
        ));
    }
    if config.low_credit_min_remaining > 0.0 {
        thresholds.push(format!(
            "剩余额度 ≤ {:.2} credits",
            config.low_credit_min_remaining
        ));
    }
    let message = format!(
        "- **账号：** `{}`\n\
         - **账号 ID：** `{}`\n\
         - **额度：** {:.2} / {:.2} credits\n\
         - **剩余：** {remaining:.2} credits（{remaining_percent:.2}%）\n\
         - **保护阈值：** {}\n\
         - **影响：** 账号仍有额度，但已暂停参与请求调度，以保留最后可用额度",
        markdown_code(account.display_name()),
        markdown_code(&account.id),
        usage.current,
        usage.limit,
        thresholds.join(" 或 "),
    );
    let mut event = WebhookEvent::new(
        WebhookEventKind::AccountCreditProtected,
        "KProxy 账号剩余额度保护",
        message,
    );
    event.account_id = Some(account.id.clone());
    Some(event)
}

fn markdown_code(value: &str) -> String {
    value
        .replace(['\r', '\n'], " ")
        .replace('`', "'")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use kproxy_core::account::{AuthMethod, Credentials, Usage};
    use kproxy_core::config::{Config, PoolConfig};
    use kproxy_store::accounts::AccountStore;
    use kproxy_store::config_loader::ConfigHandle;

    use super::*;

    fn account_with_usage(current: f64, limit: f64) -> Account {
        Account {
            id: "acc_00000001".into(),
            email: "protected@example.com".into(),
            label: None,
            enabled: true,
            machine_id: "0".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: Credentials {
                access_token: "token".into(),
                refresh_token: None,
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: 1,
                auth_method: AuthMethod::Idc,
            },
            usage: Some(Usage {
                current,
                limit,
                percent_used: current / limit * 100.0,
                next_reset_date: None,
                updated_at: 1,
            }),
            subscription: None,
            tags: Vec::new(),
            created_at: 1,
            credit_exhausted: false,
        }
    }

    async fn test_state() -> (tempfile::TempDir, AppState) {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = kproxy_core::paths::Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        kproxy_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let state = AppState::new(paths, ConfigHandle::new(Config::default()), accounts);
        (directory, state)
    }

    #[test]
    fn protected_credit_event_reports_remaining_credit_and_scheduler_threshold() {
        let account = account_with_usage(97.0, 100.0);

        assert_eq!(
            account_credit_state(&account, &PoolConfig::default()),
            AccountCreditState::Protected
        );

        let event = account_credit_protected_event(
            &account,
            account.usage.as_ref().expect("usage"),
            &PoolConfig::default(),
        )
        .expect("protected credit event");

        assert_eq!(event.kind, WebhookEventKind::AccountCreditProtected);
        assert_eq!(event.account_id.as_deref(), Some("acc_00000001"));
        assert!(event.message.contains("剩余：** 3.00 credits（3.00%）"));
        assert!(event.message.contains("剩余额度 ≤ 4.00 credits"));
        assert!(event.message.contains("已暂停参与请求调度"));
    }

    #[tokio::test]
    async fn optimistic_usage_cannot_change_authoritative_alert_state() {
        let (_directory, state) = test_state().await;
        let mut account = account_with_usage(100.0, 100.0);
        assert!(matches!(
            authoritative_credit_observation(&state, &account, &PoolConfig::default()),
            CreditObservation::Unknown
        ));

        let authoritative = Usage {
            current: 97.0,
            limit: 100.0,
            percent_used: 97.0,
            next_reset_date: None,
            updated_at: 2,
        };
        state.record_authoritative_usage(&account.id, authoritative.clone());
        assert!(matches!(
            authoritative_credit_observation(&state, &account, &PoolConfig::default()),
            CreditObservation::Known {
                state: AccountCreditState::Protected,
                ..
            }
        ));

        account.usage.as_mut().expect("usage").current = 100.0;
        assert!(matches!(
            authoritative_credit_observation(&state, &account, &PoolConfig::default()),
            CreditObservation::Known {
                state: AccountCreditState::Protected,
                ..
            }
        ));

        let first = state.authoritative_usage(&account.id).expect("observation");
        assert_eq!(
            state.observe_credit_transition(
                &account.id,
                first.generation,
                AccountCreditState::Available,
            ),
            1
        );
        assert_eq!(
            state.observe_credit_transition(
                &account.id,
                first.generation,
                AccountCreditState::Available,
            ),
            1,
            "the same authoritative generation must not count twice"
        );
        state.record_authoritative_usage(&account.id, authoritative);
        let second = state.authoritative_usage(&account.id).expect("observation");
        assert_eq!(
            state.observe_credit_transition(
                &account.id,
                second.generation,
                AccountCreditState::Available,
            ),
            2
        );
    }
}

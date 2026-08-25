//! Incident-based operator alerts.

use std::sync::Arc;

use kproxy_core::account::Account;
use kproxy_notify::{WebhookEvent, WebhookEventKind};
use kproxy_pool::{account_credit_state, AccountCreditState};

use crate::state::AppState;

pub async fn sync_quota_incidents(state: &Arc<AppState>) {
    let accounts = state.pool().snapshot().await;
    let config = state.runtime_config_snapshot().pool;
    for account in &accounts {
        sync_account_credit_incident(state, account, &config);
    }
    sync_service_quota(state).await;
}

pub async fn sync_account_quota(state: &Arc<AppState>, account_id: &str) {
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
        return;
    }
    match account_credit_state(account, config) {
        AccountCreditState::Available => resolve_account_credit_incidents(state, &account.id),
        AccountCreditState::Protected => {
            state
                .notifier()
                .resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some(&account.id));
            emit_account_credit_protected(state, account, config);
        }
        AccountCreditState::Exhausted => {
            state
                .notifier()
                .resolve_incident(WebhookEventKind::AccountCreditProtected, Some(&account.id));
            emit_account_quota(state, account);
        }
    }
}

fn resolve_account_credit_incidents(state: &AppState, account_id: &str) {
    let notifier = state.notifier();
    notifier.resolve_incident(WebhookEventKind::AccountCreditProtected, Some(account_id));
    notifier.resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some(account_id));
}

pub async fn sync_service_quota(state: &Arc<AppState>) {
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
    let pool = state.pool();
    if services.is_empty() || !pool.all_enabled_credit_exhausted().await {
        state
            .notifier()
            .resolve_incident(WebhookEventKind::ServiceQuotaExhausted, None);
        return;
    }
    let exhausted = pool
        .snapshot()
        .await
        .into_iter()
        .filter(|account| account.enabled)
        .count();
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

fn emit_account_quota(state: &AppState, account: &Account) {
    let credit = account
        .usage
        .as_ref()
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
    config: &kproxy_core::config::PoolConfig,
) {
    let Some(event) = account_credit_protected_event(account, config) else {
        return;
    };
    state.notifier().emit(event);
}

fn account_credit_protected_event(
    account: &Account,
    config: &kproxy_core::config::PoolConfig,
) -> Option<WebhookEvent> {
    let usage = account.usage.as_ref().filter(|usage| usage.limit > 0.0)?;
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
    use kproxy_core::config::PoolConfig;

    use super::*;

    #[test]
    fn protected_credit_event_reports_remaining_credit_and_scheduler_threshold() {
        let account = Account {
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
                current: 97.0,
                limit: 100.0,
                percent_used: 97.0,
                next_reset_date: None,
                updated_at: 1,
            }),
            subscription: None,
            tags: Vec::new(),
            created_at: 1,
            credit_exhausted: false,
        };

        assert_eq!(
            account_credit_state(&account, &PoolConfig::default()),
            AccountCreditState::Protected
        );

        let event = account_credit_protected_event(&account, &PoolConfig::default())
            .expect("protected credit event");

        assert_eq!(event.kind, WebhookEventKind::AccountCreditProtected);
        assert_eq!(event.account_id.as_deref(), Some("acc_00000001"));
        assert!(event.message.contains("剩余：** 3.00 credits（3.00%）"));
        assert!(event.message.contains("剩余额度 ≤ 4.00 credits"));
        assert!(event.message.contains("已暂停参与请求调度"));
    }
}

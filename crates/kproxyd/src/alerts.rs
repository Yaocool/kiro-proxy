//! Incident-based operator alerts.

use std::sync::Arc;

use kproxy_core::account::Account;
use kproxy_notify::{WebhookEvent, WebhookEventKind};
use kproxy_pool::{account_credit_state, AccountCreditState};

use crate::state::AppState;

pub async fn sync_quota_incidents(state: &Arc<AppState>) {
    let account_ids = state
        .pool()
        .snapshot()
        .await
        .into_iter()
        .map(|account| account.id)
        .collect::<Vec<_>>();
    for account_id in account_ids {
        sync_account_quota(state, &account_id).await;
    }
    sync_service_quota(state).await;
}

pub async fn sync_account_quota(state: &Arc<AppState>, account_id: &str) {
    let pool = state.pool();
    let Some(runtime) = pool.get(account_id).await else {
        state
            .notifier()
            .resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some(account_id));
        return;
    };
    let account = runtime.account.read().await.clone();
    let config = state.runtime_config_snapshot().pool;
    if account.enabled && account_credit_state(&account, &config) == AccountCreditState::Exhausted {
        emit_account_quota(state, &account);
    } else {
        state
            .notifier()
            .resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some(account_id));
    }
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

fn markdown_code(value: &str) -> String {
    value
        .replace(['\r', '\n'], " ")
        .replace('`', "'")
        .trim()
        .to_owned()
}

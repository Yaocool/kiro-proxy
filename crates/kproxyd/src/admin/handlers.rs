//! 管理面各方法实现。

use std::sync::Arc;

use futures::{stream, StreamExt};
use kproxy_core::account::Account;
use kproxy_core::config::{ApiKeyConfig, ApiKeyFormat, Config, ProxyServiceConfig};
use kproxy_core::ids::{new_account_id, new_machine_id};
use kproxy_ipc::protocol::*;
use kproxy_store::accounts::AccountStore;
use kproxy_store::config_loader::{load_config, merge_hot_reload};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::state::AppState;

type Handled = Result<serde_json::Value, RpcError>;

/// 分发一个 RPC 请求。
pub async fn dispatch(state: &Arc<AppState>, request: Request) -> Response {
    let id = request.id;
    let outcome = match request.method.as_str() {
        method::STATUS => handle_status(state).await,
        method::CONFIG_SHOW => handle_config_show(state).await,
        method::CONFIG_PATH => to_value(handle_config_path(state)),
        method::CONFIG_RELOAD => handle_config_reload(state).await,
        method::ACCOUNT_LIST => handle_account_list(state, request.params).await,
        method::ACCOUNT_SHOW => handle_account_show(state, request.params).await,
        method::ACCOUNT_IMPORT => handle_account_import(state, request.params).await,
        method::ACCOUNT_EXPORT => handle_account_export(state, request.params),
        method::ACCOUNT_ADD_SSO => handle_account_add_sso(state, request.params).await,
        method::ACCOUNT_REMOVE => handle_account_remove(state, request.params).await,
        method::ACCOUNT_SET_ENABLED => handle_account_set_enabled(state, request.params).await,
        method::ACCOUNT_TAG => handle_account_tag(state, request.params).await,
        method::ACCOUNT_REGEN_MACHINE_ID => {
            handle_regenerate_machine_id(state, request.params).await
        }
        method::ACCOUNT_REFRESH => handle_account_refresh(state, request.params).await,
        method::ACCOUNT_PROBE | method::DIAGNOSE_ACCOUNT => {
            handle_account_probe(state, request.params).await
        }
        method::DIAGNOSE_ENDPOINTS => handle_diagnose_endpoints(request.params).await,
        method::SUBSCRIPTIONS => handle_subscriptions(state, request.params).await,
        method::ACCOUNT_RESET_HEALTH => handle_account_reset_health(state, request.params).await,
        method::POOL => handle_pool(state, request.params).await,
        method::TASKS => Ok(state.task_registry.snapshot(state)),
        method::TASK_RUN => handle_task_run(state, request.params).await,
        method::STATS => handle_stats(state, request.params),
        method::LOGS => handle_logs(state, request.params).await,
        method::MODELS => handle_models(state).await,
        method::APIKEY_LIST => to_value(state.meter.list()),
        method::APIKEY_RESET_USAGE => handle_apikey_reset(state, request.params).await,
        method::SERVICE_LIST => handle_service_list(state).await,
        method::SERVICE_CREATE => handle_service_create(state, request.params).await,
        method::SERVICE_DELETE => handle_service_delete(state, request.params).await,
        method::SERVICE_APIKEYS => handle_service_apikeys(state, request.params),
        method::WEBHOOK_LIST => handle_webhook_list(state),
        method::WEBHOOK_TEST => handle_webhook_test(state, request.params),
        method::WEBHOOK_LOGS => handle_webhook_logs(state, request.params),
        other => Err(RpcError::unknown_method(other)),
    };
    match outcome {
        Ok(result) => Response::ok(id, result),
        Err(error) => Response::err(id, error),
    }
}

async fn handle_logs(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize, Default)]
    struct Params {
        after_request_id: Option<String>,
        #[serde(default = "default_tail")]
        tail: usize,
        #[serde(default)]
        wait_ms: u64,
        level: Option<String>,
        account: Option<String>,
    }
    fn default_tail() -> usize {
        50
    }
    let params: Params = parse_params(params)?;
    let entries = state
        .stats
        .follow(
            params.after_request_id.as_deref(),
            params.tail,
            params.wait_ms,
            params.level.as_deref(),
            params.account.as_deref(),
        )
        .await;
    to_value(serde_json::json!({"entries":entries}))
}

fn handle_webhook_list(state: &Arc<AppState>) -> Handled {
    let logs = state.notifier().logs(1_000);
    let mut targets = state
        .config
        .current()
        .webhook
        .iter()
        .map(|target| {
            let latest = logs.iter().find(|log| log.target == target.name);
            serde_json::json!({
                "name":target.name,
                "type":target.kind,
                "events":target.events,
                "enabled":target.enabled,
                "status":latest.map(|log| if log.success {"ok"} else {"failed"}).unwrap_or("never"),
                "last_delivery_at":latest.map(|log| log.timestamp)
            })
        })
        .collect::<Vec<_>>();
    targets.sort_by(|left, right| {
        compare_display_text(
            left["name"].as_str().unwrap_or_default(),
            right["name"].as_str().unwrap_or_default(),
        )
    });
    to_value(targets)
}

fn handle_account_export(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize, Default)]
    struct Params {
        #[serde(default)]
        redact: bool,
    }
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        parse_params(params)?
    };
    let mut accounts = state.with_accounts(|store| store.all().to_vec());
    accounts.sort_by(|left, right| {
        compare_account_identity(&left.email, &left.id, &right.email, &right.id)
    });
    if params.redact {
        for account in &mut accounts {
            account.credentials.access_token = "<redacted>".into();
            account.credentials.refresh_token = account
                .credentials
                .refresh_token
                .as_ref()
                .map(|_| "<redacted>".into());
            account.credentials.client_secret = account
                .credentials
                .client_secret
                .as_ref()
                .map(|_| "<redacted>".into());
        }
    }
    to_value(accounts)
}

async fn handle_diagnose_endpoints(params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize)]
    struct Params {
        #[serde(default = "default_region")]
        region: String,
    }
    fn default_region() -> String {
        "us-east-1".into()
    }
    let params: Params = parse_params(params)?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let endpoints = vec![
        (
            "codewhisperer",
            kproxy_kiro::endpoint::CODEWHISPERER_URL.to_string(),
        ),
        ("amazonq", kproxy_kiro::endpoint::AMAZONQ_URL.to_string()),
        (
            "oidc",
            format!("https://oidc.{}.amazonaws.com/token", params.region),
        ),
    ];
    let probes = futures::future::join_all(endpoints.into_iter().map(|(name, url)| {
        let client = client.clone();
        async move {
            let started = std::time::Instant::now();
            match client.post(&url).json(&serde_json::json!({})).send().await {
                Ok(response) => serde_json::json!({
                    "name":name,"url":url,"reachable":true,
                    "status":response.status().as_u16(),
                    "latency_ms":started.elapsed().as_millis()
                }),
                Err(error) => serde_json::json!({
                    "name":name,"url":url,"reachable":false,
                    "latency_ms":started.elapsed().as_millis(),
                    "error":kproxy_translate::sanitize_error_message(&error.to_string())
                }),
            }
        }
    }))
    .await;
    to_value(probes)
}

async fn handle_subscriptions(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize)]
    struct Params {
        id: Option<String>,
    }
    let params: Params = parse_params(params)?;
    let account = state
        .pool()
        .snapshot()
        .await
        .into_iter()
        .find(|account| {
            account.enabled
                && params
                    .id
                    .as_deref()
                    .is_none_or(|id| account.id == id || account.email == id)
        })
        .ok_or_else(|| RpcError::bad_params("no matching enabled account"))?;
    let subscriptions = state
        .kiro()
        .list_subscriptions(&account)
        .await
        .map_err(|error| {
            RpcError::internal(kproxy_translate::sanitize_error_message(&error.to_string()))
        })?;
    to_value(serde_json::json!({
        "account_id":account.id,
        "plans":subscriptions.subscription_plans,
        "disclaimer":subscriptions.disclaimer
    }))
}

fn to_value<T: serde::Serialize>(value: T) -> Handled {
    serde_json::to_value(value).map_err(|error| RpcError::internal(error.to_string()))
}

fn parse_params<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|error| RpcError::bad_params(error.to_string()))
}

fn compare_display_text(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_ascii_lowercase()
        .cmp(&right.to_ascii_lowercase())
        .then_with(|| left.cmp(right))
}

async fn handle_status(state: &Arc<AppState>) -> Handled {
    let config = state.config.current();
    let services = state.proxy_services.views(&config.proxy_service).await;
    let running_services = services
        .iter()
        .filter(|service| service.running)
        .collect::<Vec<_>>();
    let listen = if running_services.is_empty() {
        "-".to_string()
    } else {
        running_services
            .iter()
            .map(|service| format!("{}:{}", service.host, service.port))
            .collect::<Vec<_>>()
            .join(",")
    };
    let (total, enabled) = state.with_accounts(|store| {
        (
            store.len(),
            store.all().iter().filter(|account| account.enabled).count(),
        )
    });
    let hint = if total == 0 {
        Some("无可用账号，请先添加：kproxy account import".to_string())
    } else if enabled == 0 {
        Some("所有账号均已停用，代理无法服务请求".to_string())
    } else {
        None
    };
    let pool = state.pool();
    let mut health = [0usize; 5];
    for account in pool
        .snapshot()
        .await
        .into_iter()
        .filter(|account| account.enabled)
    {
        if let Some(runtime) = pool.get(&account.id).await {
            health[runtime.health() as usize] += 1;
        }
    }
    let stats = state.stats.snapshot(None);
    let request_count = stats.total.requests;
    let success_rate = if request_count == 0 {
        0.0
    } else {
        stats.total.successes as f64 * 100.0 / request_count as f64
    };
    let average_latency_ms = if stats.latencies_ms.is_empty() {
        0
    } else {
        stats.latencies_ms.iter().sum::<u64>() / stats.latencies_ms.len() as u64
    };
    let (daily_credit_day, daily_credit_used, daily_credit_reserved, daily_credit_limit) =
        state.meter.daily_snapshot();
    let enabled_services = services.iter().filter(|service| service.enabled).count();
    let mut readiness_reasons = state.task_registry.readiness_issues(state);
    if enabled_services == 0 {
        readiness_reasons.push("no enabled proxy service is configured".to_string());
    } else if running_services.len() < enabled_services {
        readiness_reasons.push(format!(
            "only {} of {enabled_services} enabled proxy services are running",
            running_services.len()
        ));
    }
    if health[0] == 0 {
        readiness_reasons.push("no account is currently available".to_string());
    }
    if let Some(error) = state.meter.recovery_error() {
        readiness_reasons.push(format!("metering recovery required: {error}"));
    }
    let ready = readiness_reasons.is_empty();
    to_value(StatusResult {
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        uptime_secs: state.uptime_secs(),
        listen,
        proxy_service_total: services.len(),
        proxy_service_running: running_services.len(),
        admin_socket: state.admin_socket().display().to_string(),
        account_total: total,
        account_enabled: enabled,
        account_available: health[0],
        account_cooling: health[1],
        account_exhausted: health[2],
        account_banned: health[3],
        active_requests: pool.active().await,
        max_concurrent_requests: state.admission.maximum(),
        queued_requests: pool.queued(),
        request_count,
        success_rate,
        average_latency_ms,
        credits: stats.total.credits,
        daily_credit_day,
        daily_credit_used,
        daily_credit_reserved,
        daily_credit_limit,
        config_path: state.paths.config_file.display().to_string(),
        config_reloaded_at: state.config_reloaded_at(),
        hint,
        ready,
        readiness_reasons,
    })
}

async fn handle_config_show(state: &Arc<AppState>) -> Handled {
    let raw = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .unwrap_or_default();
    let effective_json = serde_json::to_value(state.config.current().as_ref())
        .map_err(|error| RpcError::internal(error.to_string()))?;
    to_value(ConfigShowResult {
        path: state.paths.config_file.display().to_string(),
        raw,
        effective_json,
    })
}

fn handle_config_path(state: &Arc<AppState>) -> ConfigPathResult {
    ConfigPathResult {
        config_file: state.paths.config_file.display().to_string(),
        accounts_file: state.paths.accounts_file.display().to_string(),
        daily_file: state.paths.daily_file.display().to_string(),
        stats_file: state.paths.stats_file.display().to_string(),
        admin_socket: state.admin_socket().display().to_string(),
    }
}

async fn handle_config_reload(state: &Arc<AppState>) -> Handled {
    let next = match load_config(&state.paths.config_file).await {
        Ok(config) => config,
        Err(error) => {
            return to_value(ConfigReloadResult {
                applied: false,
                error: Some(error.to_string()),
                needs_restart: vec![],
            });
        }
    };
    if let Err(error) = next.validate() {
        return to_value(ConfigReloadResult {
            applied: false,
            error: Some(error.to_string()),
            needs_restart: vec![],
        });
    }

    let current = state.config.current();
    let (next, needs_restart) = merge_hot_reload(&current, next);
    let needs_restart = needs_restart
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    for field in &needs_restart {
        warn!(field = %field, "configuration field requires restart");
    }
    if let Err(error) = state.apply_config_transaction(&next).await {
        warn!(%error, "configuration reload rolled back");
        return to_value(ConfigReloadResult {
            applied: false,
            error: Some(error),
            needs_restart,
        });
    }
    state.mark_config_reloaded(now_secs());
    to_value(ConfigReloadResult {
        applied: true,
        error: None,
        needs_restart,
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn summarize(account: &Account) -> AccountSummary {
    let mut tags = account.tags.clone();
    tags.sort();
    AccountSummary {
        id: account.id.clone(),
        email: account.email.clone(),
        label: account.label.clone(),
        enabled: account.enabled,
        health: None,
        tags,
        subscription: account
            .subscription
            .as_ref()
            .map(|subscription| format!("{:?}", subscription.kind)),
        credit_current: account.usage.as_ref().map(|usage| usage.current),
        credit_limit: account.usage.as_ref().map(|usage| usage.limit),
        token_expires_at: account.credentials.expires_at,
        credit_exhausted: account.credit_exhausted,
    }
}

fn compare_account_identity(
    left_email: &str,
    left_id: &str,
    right_email: &str,
    right_id: &str,
) -> std::cmp::Ordering {
    compare_display_text(left_email, right_email).then_with(|| left_id.cmp(right_id))
}

async fn handle_account_list(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: AccountListParams = if params.is_null() {
        AccountListParams::default()
    } else {
        parse_params(params)?
    };
    let source = state.with_accounts(|store| store.all().to_vec());
    let pool = state.pool();
    let mut accounts = Vec::new();
    for account in source
        .iter()
        .filter(|account| {
            params
                .tag
                .as_ref()
                .is_none_or(|tag| account.tags.iter().any(|account_tag| account_tag == tag))
        })
        .filter(|account| !params.enabled_only.unwrap_or(false) || account.enabled)
    {
        let mut summary = summarize(account);
        summary.health = if !account.enabled {
            Some("disabled".into())
        } else if account.credit_exhausted {
            Some("exhausted".into())
        } else if let Some(runtime) = pool.get(&account.id).await {
            Some(format!("{:?}", runtime.health()).to_ascii_lowercase())
        } else {
            Some("unavailable".into())
        };
        if params
            .status
            .as_deref()
            .is_some_and(|status| summary.health.as_deref() != Some(status))
        {
            continue;
        }
        accounts.push(summary);
    }
    match params.sort.as_deref() {
        Some("credit") => accounts.sort_by(|left, right| {
            left.credit_current
                .unwrap_or(f64::INFINITY)
                .total_cmp(&right.credit_current.unwrap_or(f64::INFINITY))
                .then_with(|| {
                    compare_account_identity(&left.email, &left.id, &right.email, &right.id)
                })
        }),
        Some("email") | None => accounts.sort_by(|left, right| {
            compare_account_identity(&left.email, &left.id, &right.email, &right.id)
        }),
        Some("id") => accounts.sort_by(|left, right| left.id.cmp(&right.id)),
        Some(other) => {
            return Err(RpcError::bad_params(format!(
                "unsupported account sort field: {other}"
            )))
        }
    }
    to_value(AccountListResult { accounts })
}

async fn handle_account_show(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: AccountRefParams = parse_params(params)?;
    let account = state
        .with_accounts(|store| store.find(&params.id).cloned())
        .ok_or_else(|| RpcError::bad_params(format!("account not found: {}", params.id)))?;
    let pool = state.pool();
    let runtime = pool.get(&account.id).await;
    let mut summary = summarize(&account);
    let (supported_models, active_requests) = if let Some(runtime) = runtime {
        summary.health = Some(if !account.enabled {
            "disabled".into()
        } else if account.credit_exhausted {
            "exhausted".into()
        } else {
            format!("{:?}", runtime.health()).to_ascii_lowercase()
        });
        (runtime.supported_models().await, runtime.active())
    } else {
        summary.health = Some("unavailable".into());
        (Vec::new(), 0)
    };
    let preferred_endpoint = state
        .kiro()
        .endpoint_cache()
        .preferred(&account.id, kproxy_kiro::EndpointPurpose::Generation)
        .map(|endpoint| format!("{endpoint:?}").to_ascii_lowercase());
    let recent_errors = state
        .stats
        .snapshot(Some(100))
        .recent_requests
        .iter()
        .rev()
        .filter(|request| request.account_id == account.id && request.status >= 400)
        .take(5)
        .map(|request| {
            request
                .error
                .clone()
                .unwrap_or_else(|| format!("HTTP {}", request.status))
        })
        .collect();
    to_value(AccountDetail {
        summary,
        machine_id: account.machine_id,
        region: account.credentials.region,
        auth_method: format!("{:?}", account.credentials.auth_method),
        created_at: account.created_at,
        usage_updated_at: account.usage.as_ref().map(|usage| usage.updated_at),
        usage: account.usage,
        subscription_detail: account.subscription,
        supported_models,
        preferred_endpoint,
        active_requests,
        max_concurrent_requests: state.config.current().pool.max_concurrent_per_account,
        recent_errors,
    })
}

async fn commit_account_change<F, T>(state: &Arc<AppState>, mutate: F) -> Result<T, RpcError>
where
    F: FnOnce(&mut AccountStore) -> Result<T, RpcError>,
{
    let _transaction = state.lock_account_mutation().await;
    let mut next = state.with_accounts(Clone::clone);
    let result = mutate(&mut next)?;
    next.save()
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let pool_accounts = next.all().to_vec();
    state.replace_accounts(next);
    state.pool().replace_accounts(pool_accounts).await;
    state.request_model_refresh();
    Ok(result)
}

async fn handle_account_import(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: AccountImportParams = parse_params(params)?;
    let result = commit_account_change(state, move |store| {
        let mut imported = 0usize;
        let mut skipped = Vec::new();
        for account in params.accounts {
            validate_account_input(&account)?;
            let id = account.id.clone();
            match store.insert(account) {
                Ok(()) => imported += 1,
                Err(_) => skipped.push(id),
            }
        }
        Ok(AccountImportResult { imported, skipped })
    })
    .await?;
    to_value(result)
}

fn validate_account_input(account: &Account) -> Result<(), RpcError> {
    let valid_id = account.id.len() == 12
        && account.id.starts_with("acc_")
        && account.id[4..]
            .chars()
            .all(|character| character.is_ascii_hexdigit());
    if !valid_id {
        return Err(RpcError::bad_params(format!(
            "invalid account id: {}",
            account.id
        )));
    }
    if !account.email.contains('@') || account.email.trim().len() < 3 {
        return Err(RpcError::bad_params("invalid account email"));
    }
    if account.machine_id.len() != 64
        || !account
            .machine_id
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(RpcError::bad_params(
            "machine_id must be 64 hexadecimal characters",
        ));
    }
    if account.credentials.access_token.trim().is_empty() {
        return Err(RpcError::bad_params("access_token must not be empty"));
    }
    if account.credentials.region.trim().is_empty() {
        return Err(RpcError::bad_params("credentials.region must not be empty"));
    }
    if account
        .profile_arn
        .as_deref()
        .is_some_and(|arn| !arn.starts_with("arn:"))
    {
        return Err(RpcError::bad_params("profileArn must be an ARN"));
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct AccountAddSsoParams {
    email: String,
    password: String,
    start_url: String,
    #[serde(default = "default_sso_region")]
    region: String,
    #[serde(default)]
    headful: bool,
}

fn default_sso_region() -> String {
    "us-east-1".into()
}

async fn handle_account_add_sso(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: AccountAddSsoParams = parse_params(params)?;
    let email = params.email.trim().to_ascii_lowercase();
    if state.with_accounts(|store| store.find(&email).is_some()) {
        return Err(RpcError::bad_params(format!(
            "account already exists: {email}"
        )));
    }
    let credentials = crate::sso::login(crate::sso::SsoLoginRequest {
        email: email.clone(),
        password: params.password,
        start_url: params.start_url,
        region: params.region,
        headful: params.headful,
    })
    .await
    .map_err(|error| RpcError::internal(error.to_string()))?;
    let mut account = Account {
        id: new_account_id(),
        email: email.clone(),
        label: None,
        enabled: true,
        machine_id: new_machine_id(),
        profile_arn: None,
        credentials,
        usage: None,
        subscription: None,
        tags: Vec::new(),
        created_at: now_secs(),
        credit_exhausted: false,
    };
    let limits = state
        .kiro()
        .get_usage_limits(&account)
        .await
        .map_err(|error| {
            RpcError::internal(format!(
                "SSO login succeeded, but Kiro account validation failed; account was not saved: {}",
                kproxy_translate::sanitize_error_message(&error.to_string())
            ))
        })?;
    validate_sso_identity(&email, &limits)?;
    if let Some(usage) = limits.normalized_usage(now_secs()) {
        account.credit_exhausted = usage.limit > 0.0 && usage.current >= usage.limit;
        account.usage = Some(usage);
    }
    account.subscription = limits.normalized_subscription();
    let summary = summarize(&account);
    commit_account_change(state, move |store| {
        store
            .insert(account)
            .map_err(|error| RpcError::bad_params(error.to_string()))
    })
    .await?;
    to_value(summary)
}

fn validate_sso_identity(
    expected_email: &str,
    limits: &kproxy_kiro::UsageLimits,
) -> Result<(), RpcError> {
    let actual = limits
        .user_info
        .as_ref()
        .map(|identity| identity.email.trim())
        .filter(|email| !email.is_empty())
        .ok_or_else(|| {
            RpcError::internal(
                "Kiro account validation did not return an authenticated identity; account was not saved",
            )
        })?;
    if sso_identities_match(expected_email, actual) {
        return Ok(());
    }
    Err(RpcError::bad_params(format!(
        "SSO identity mismatch: requested {expected_email}, but Kiro authenticated {actual}; account was not saved"
    )))
}

fn sso_identities_match(expected_email: &str, actual_identity: &str) -> bool {
    let expected_email = expected_email.trim();
    let actual_identity = actual_identity.trim();
    if expected_email.is_empty() || actual_identity.is_empty() {
        return false;
    }
    if expected_email.eq_ignore_ascii_case(actual_identity) {
        return true;
    }
    let expected = canonical_sso_identity(expected_email);
    let actual = canonical_sso_identity(actual_identity);
    !expected.is_empty() && expected == actual
}

fn canonical_sso_identity(value: &str) -> String {
    value
        .trim()
        .split('@')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|character| *character != '.')
        .flat_map(char::to_lowercase)
        .collect()
}

async fn handle_account_remove(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: AccountRefParams = parse_params(params)?;
    let id = params.id;
    let removed = commit_account_change(state, |store| {
        store
            .remove(&id)
            .map(|account| account.id)
            .ok_or_else(|| RpcError::bad_params(format!("account not found: {id}")))
    })
    .await?;
    state.kiro().endpoint_cache().clear_account(&removed);
    to_value(serde_json::json!({"removed": removed}))
}

async fn handle_account_set_enabled(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: AccountSetEnabledParams = parse_params(params)?;
    let id = params.id;
    let enabled = params.enabled;
    commit_account_change(state, |store| {
        if store.update(&id, |account| account.enabled = enabled) {
            Ok(())
        } else {
            Err(RpcError::bad_params(format!("account not found: {id}")))
        }
    })
    .await?;
    to_value(serde_json::json!({"id": id, "enabled": enabled}))
}

async fn handle_account_tag(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: AccountTagParams = parse_params(params)?;
    let id = params.id;
    let add = params.add;
    let remove = params.remove;
    let tags = commit_account_change(state, |store| {
        let updated = store.update(&id, |account| {
            for tag in &add {
                if !account.tags.contains(tag) {
                    account.tags.push(tag.clone());
                }
            }
            account.tags.retain(|tag| !remove.contains(tag));
            account.tags.sort();
        });
        if !updated {
            return Err(RpcError::bad_params(format!("account not found: {id}")));
        }
        Ok(store
            .find(&id)
            .map(|account| account.tags.clone())
            .unwrap_or_default())
    })
    .await?;
    to_value(serde_json::json!({"id": id, "tags": tags}))
}

async fn handle_regenerate_machine_id(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: AccountRefParams = parse_params(params)?;
    let id = params.id;
    let machine_id = new_machine_id();
    commit_account_change(state, |store| {
        if store.update(&id, |account| account.machine_id = machine_id.clone()) {
            Ok(())
        } else {
            Err(RpcError::bad_params(format!("account not found: {id}")))
        }
    })
    .await?;
    to_value(serde_json::json!({"id": id, "machine_id": machine_id}))
}

async fn handle_account_refresh(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize)]
    struct Params {
        id: Option<String>,
        #[serde(default)]
        all: bool,
    }
    let params: Params = parse_params(params)?;
    let pool = state.pool();
    let ids = if params.all {
        pool.snapshot()
            .await
            .into_iter()
            .filter(|account| account.enabled)
            .map(|account| account.id)
            .collect::<Vec<_>>()
    } else {
        vec![params
            .id
            .ok_or_else(|| RpcError::bad_params("id is required unless all=true"))?]
    };
    let mut refreshed = Vec::new();
    for id in ids {
        match state.refresh_account_token(&pool, &id, true).await {
            Ok(changed) => {
                let usage_refreshed = crate::tasks::refresh_account_usage(state, &pool, &id)
                    .await
                    .unwrap_or(false);
                refreshed.push(serde_json::json!({
                    "id":id,"ok":true,"refreshed":changed,
                    "usage_refreshed":usage_refreshed
                }))
            }
            Err(error) => refreshed.push(serde_json::json!({
                "id":id,"ok":false,"error":kproxy_translate::sanitize_error_message(&error.to_string())
            })),
        }
    }
    persist_pool_snapshot(state).await?;
    to_value(refreshed)
}

async fn handle_account_probe(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize)]
    struct Params {
        id: Option<String>,
        #[serde(default)]
        all: bool,
        #[serde(default = "default_probe_timeout")]
        timeout_secs: u64,
        #[serde(default = "default_probe_concurrency")]
        concurrency: usize,
    }
    fn default_probe_timeout() -> u64 {
        45
    }
    fn default_probe_concurrency() -> usize {
        1
    }
    let params: Params = parse_params(params)?;
    if params.timeout_secs == 0 || params.timeout_secs > 300 {
        return Err(RpcError::bad_params(
            "timeout_secs must be between 1 and 300",
        ));
    }
    if !(1..=8).contains(&params.concurrency) {
        return Err(RpcError::bad_params("concurrency must be between 1 and 8"));
    }
    let pool = state.pool();
    let accounts = if params.all {
        pool.snapshot()
            .await
            .into_iter()
            .filter(|account| account.enabled)
            .collect::<Vec<_>>()
    } else {
        let id = params
            .id
            .as_deref()
            .ok_or_else(|| RpcError::bad_params("id is required unless all=true"))?;
        vec![pool
            .get(id)
            .await
            .ok_or_else(|| RpcError::bad_params(format!("account not found: {id}")))?
            .account
            .read()
            .await
            .clone()]
    };
    let output = stream::iter(
        accounts
            .into_iter()
            .map(|account| probe_account(state, &pool, account, params.timeout_secs)),
    )
    .buffer_unordered(params.concurrency)
    .collect::<Vec<_>>()
    .await;
    if params.all {
        to_value(output)
    } else {
        output
            .into_iter()
            .next()
            .ok_or_else(|| RpcError::bad_params("no enabled account"))
    }
}

async fn probe_account(
    state: &Arc<AppState>,
    pool: &kproxy_pool::AccountPool,
    account: Account,
    timeout_secs: u64,
) -> serde_json::Value {
    let account_id = account.id.clone();
    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        probe_account_inner(state, pool, account),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => serde_json::json!({
            "account_id":account_id,
            "ok":false,
            "error":format!("probe timed out after {timeout_secs}s")
        }),
    }
}

async fn probe_account_inner(
    state: &Arc<AppState>,
    pool: &kproxy_pool::AccountPool,
    account: Account,
) -> serde_json::Value {
    let models = state.kiro().list_models(&account).await;
    let models = match models {
        Ok(models) => models,
        Err(error) => {
            return serde_json::json!({
                "account_id":account.id,"ok":false,
                "error":kproxy_translate::sanitize_error_message(&error.to_string())
            })
        }
    };
    if let Some(pool_account) = pool.get(&account.id).await {
        pool_account
            .set_supported_models(models.iter().map(|model| model.model_id.clone()))
            .await;
    }
    let model = models
        .first()
        .map(|model| model.model_id.clone())
        .unwrap_or_else(|| "minimax-m2.5".into());
    let request = kproxy_translate::OpenAiRequest {
        model: model.clone(),
        messages: vec![kproxy_translate::OpenAiMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String(
                "Hi, reply with \"pong\" only.".into(),
            )),
            tool_calls: vec![],
            tool_call_id: None,
        }],
        temperature: Some(0.0),
        top_p: None,
        max_tokens: Some(16),
        max_completion_tokens: None,
        stream: false,
        stream_options: None,
        tools: vec![],
        tool_choice: None,
        parallel_tool_calls: true,
        thinking: None,
        response_format: None,
    };
    let payload = kproxy_translate::openai_to_kiro(
        &request,
        &kproxy_translate::TranslationOptions::new(model, "CLI"),
    );
    let result = match state.kiro().generate(&account, &payload, None).await {
        Ok(response) => {
            let (endpoint, response, _upstream_permit) = response.into_parts();
            match state.kiro().collect_events(response).await {
                Ok(events) => serde_json::json!({
                    "ok":events.iter().any(|event| matches!(event, kproxy_kiro::KiroEvent::AssistantResponse { .. })),
                    "endpoint":endpoint.name
                }),
                Err(error) => {
                    serde_json::json!({"ok":false,"error":kproxy_translate::sanitize_error_message(&error.to_string())})
                }
            }
        }
        Err(error) => {
            serde_json::json!({"ok":false,"error":kproxy_translate::sanitize_error_message(&error.to_string())})
        }
    };
    let ok = result["ok"].clone();
    serde_json::json!({"account_id":account.id,"models":models,"probe":result,"ok":ok})
}

async fn handle_account_reset_health(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize)]
    struct Params {
        id: Option<String>,
        #[serde(default)]
        all: bool,
    }
    let params: Params = parse_params(params)?;
    let pool = state.pool();
    let ids = if params.all {
        pool.snapshot()
            .await
            .into_iter()
            .map(|account| account.id)
            .collect::<Vec<_>>()
    } else {
        vec![params
            .id
            .ok_or_else(|| RpcError::bad_params("id is required unless all=true"))?]
    };
    let mut reset = Vec::new();
    for id in ids {
        if pool.reset_health(&id).await {
            reset.push(id);
        }
    }
    if reset.is_empty() && !params.all {
        return Err(RpcError::bad_params("account not found"));
    }
    persist_pool_snapshot(state).await?;
    to_value(serde_json::json!({"reset":reset}))
}

#[derive(serde::Deserialize)]
struct PoolParams {
    #[serde(default = "default_pool_model")]
    model: String,
}

fn default_pool_model() -> String {
    "minimax-m2.5".into()
}

async fn handle_pool(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: PoolParams = if params.is_null() {
        PoolParams {
            model: default_pool_model(),
        }
    } else {
        parse_params(params)?
    };
    let pool = state.pool();
    let scores = pool.explain(&params.model).await;
    to_value(serde_json::json!({"model":params.model,"queued":pool.queued(),"accounts":scores}))
}

#[derive(serde::Deserialize, Default)]
struct StatsParams {
    #[serde(default)]
    detail: bool,
    recent: Option<usize>,
    since_secs: Option<u64>,
    by: Option<String>,
    account: Option<String>,
    level: Option<String>,
}

fn handle_stats(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: StatsParams = if params.is_null() {
        StatsParams::default()
    } else {
        parse_params(params)?
    };
    if params.by.as_deref() == Some("apikey") {
        if !params.detail {
            return Err(RpcError::bad_params("stats grouping requires detail=true"));
        }
        return to_value(serde_json::json!({"by_apikey":state.meter.list()}));
    }
    let cutoff = params
        .since_secs
        .map(|seconds| now_secs().saturating_sub(seconds as i64));
    let mut stats = state.stats.window(cutoff, params.recent);
    if params.account.is_some() || params.level.is_some() {
        stats.recent_requests.retain(|request| {
            params
                .account
                .as_deref()
                .is_none_or(|account| request.account_id == account)
                && params.level.as_deref().is_none_or(|level| match level {
                    "error" => request.status >= 500,
                    "warn" => request.status >= 400,
                    _ => true,
                })
        });
    }
    let percentiles = stats.percentiles();
    if !params.detail {
        return to_value(serde_json::json!({
            "summary":stats.total,
            "latency":{"p50_ms":percentiles.0,"p95_ms":percentiles.1,"p99_ms":percentiles.2}
        }));
    }
    let grouped = match params.by.as_deref() {
        Some("account") => serde_json::to_value(&stats.by_account),
        Some("endpoint") => serde_json::to_value(&stats.by_endpoint),
        Some("model") => serde_json::to_value(&stats.by_model),
        Some(other) => {
            return Err(RpcError::bad_params(format!(
                "unknown stats group: {other}"
            )))
        }
        None => serde_json::to_value(&stats.total),
    }
    .map_err(|error| RpcError::internal(error.to_string()))?;
    to_value(serde_json::json!({
        "stats":stats,
        "grouped":grouped,
        "latency":{"p50_ms":percentiles.0,"p95_ms":percentiles.1,"p99_ms":percentiles.2}
    }))
}

#[derive(serde::Deserialize)]
struct TaskRunParams {
    name: String,
}

async fn handle_task_run(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: TaskRunParams = parse_params(params)?;
    crate::tasks::run_named(state, &params.name)
        .await
        .map_err(|error| RpcError::bad_params(error.to_string()))
}

async fn handle_models(state: &Arc<AppState>) -> Handled {
    let config = state.config.current();
    if !config.models.dynamic_discovery {
        let mut models = crate::http::fallback_models(&config);
        sort_models_for_display(&mut models);
        return to_value(models);
    }
    let (mut cached, fresh) = state.models.get(config.models.cache_ttl_ms);
    if fresh {
        sort_models_for_display(&mut cached);
        return to_value(cached);
    }
    let account = state
        .pool()
        .snapshot()
        .await
        .into_iter()
        .find(|account| account.enabled)
        .ok_or_else(|| RpcError::bad_params("no enabled account for model discovery"))?;
    let models = state
        .kiro()
        .list_models(&account)
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    if let Some(runtime) = state.pool().get(&account.id).await {
        runtime
            .set_supported_models(models.iter().map(|model| model.model_id.clone()))
            .await;
    }
    state.models.finish_refresh(models.clone());
    let mut output = models;
    sort_models_for_display(&mut output);
    to_value(output)
}

fn sort_models_for_display(models: &mut [kproxy_kiro::ModelInfo]) {
    models.sort_by(|left, right| {
        compare_display_text(&left.model_id, &right.model_id)
            .then_with(|| compare_display_text(&left.model_name, &right.model_name))
    });
}

#[derive(serde::Deserialize)]
struct WebhookTestParams {
    name: Option<String>,
}

fn handle_webhook_test(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    use kproxy_notify::{WebhookEvent, WebhookEventKind};
    let params: WebhookTestParams = parse_params(params)?;
    let event = WebhookEvent::new(
        WebhookEventKind::ServiceDegraded,
        "kiro-proxy webhook test",
        "This is a test notification from kiro-proxy.",
    );
    let notifier = state.notifier();
    let queued = match params.name.as_deref() {
        Some(name) => notifier.emit_to(name, event),
        None => notifier.emit(event),
    };
    if queued == 0 {
        return Err(RpcError::bad_params(format!(
            "webhook not found, disabled, or not subscribed: {}",
            params.name.as_deref().unwrap_or("all")
        )));
    }
    to_value(
        serde_json::json!({"name":params.name.unwrap_or_else(|| "all".into()),"queued":queued}),
    )
}

#[derive(serde::Deserialize)]
struct WebhookLogsParams {
    #[serde(default = "default_webhook_tail")]
    tail: usize,
}

fn default_webhook_tail() -> usize {
    50
}

fn handle_webhook_logs(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: WebhookLogsParams = if params.is_null() {
        WebhookLogsParams {
            tail: default_webhook_tail(),
        }
    } else {
        parse_params(params)?
    };
    to_value(state.notifier().logs(params.tail))
}

async fn handle_service_list(state: &Arc<AppState>) -> Handled {
    let config = state.config.current();
    let mut services = state.proxy_services.views(&config.proxy_service).await;
    services.sort_by(|left, right| {
        compare_display_text(&left.name, &right.name).then_with(|| left.id.cmp(&right.id))
    });
    to_value(ProxyServiceListResult { services })
}

fn handle_service_apikeys(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: ProxyServiceApiKeysParams = parse_params(params)?;
    let selector = params.service.trim();
    if selector.is_empty() {
        return Err(RpcError::bad_params("service ID or name must not be empty"));
    }
    let config = state.config.current();
    let service = config
        .proxy_service
        .iter()
        .find(|service| service.id == selector || service.name == selector)
        .ok_or_else(|| RpcError::bad_params(format!("proxy service not found: {selector}")))?;
    let mut api_keys = service
        .api_key_ids
        .iter()
        .filter_map(|key_id| {
            config
                .api_key
                .iter()
                .find(|key| key.id.as_deref() == Some(key_id.as_str()))
                .map(|key| ProxyServiceApiKeyView {
                    id: key_id.clone(),
                    name: key.name.clone(),
                    format: match key.format {
                        ApiKeyFormat::Sk => "sk",
                        ApiKeyFormat::Simple => "simple",
                        ApiKeyFormat::Token => "token",
                    }
                    .to_string(),
                    enabled: key.enabled,
                    credits_limit: key.credits_limit,
                    key: params.show_secret.then(|| key.key.clone()),
                })
        })
        .collect::<Vec<_>>();
    api_keys.sort_by(|left, right| {
        compare_display_text(&left.name, &right.name).then_with(|| left.id.cmp(&right.id))
    });
    to_value(ProxyServiceApiKeysResult {
        service_id: service.id.clone(),
        service_name: service.name.clone(),
        api_keys,
    })
}

async fn handle_service_create(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: ProxyServiceCreateParams = parse_params(params)?;
    let name = params.name.trim();
    if name.is_empty() {
        return Err(RpcError::bad_params("service name must not be empty"));
    }
    let _mutation = state.lock_config_mutation().await;
    let previous = state.config.current().as_ref().clone();
    if previous
        .proxy_service
        .iter()
        .any(|service| service.name == name)
    {
        return Err(RpcError::bad_params(format!(
            "proxy service name already exists: {name}"
        )));
    }

    let format_name = params.api_key_format.as_deref().unwrap_or("sk");
    let format = match format_name {
        "sk" => ApiKeyFormat::Sk,
        "token" => ApiKeyFormat::Token,
        "simple" => ApiKeyFormat::Simple,
        other => {
            return Err(RpcError::bad_params(format!(
                "unsupported API key format: {other}"
            )))
        }
    };
    let key = generate_api_key(format);
    let key_id = api_key_id(&key);
    let key_name = params
        .api_key_name
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("{name}-default"));
    let service = ProxyServiceConfig {
        id: format!("svc_{}", uuid::Uuid::new_v4().simple()),
        name: name.to_string(),
        host: params.host.unwrap_or_else(|| previous.server.host.clone()),
        port: params.port.unwrap_or(previous.server.port),
        enabled: true,
        api_key_ids: vec![key_id.clone()],
        created_at: now_secs(),
    };
    let key_config = ApiKeyConfig {
        id: Some(key_id.clone()),
        name: key_name.clone(),
        key: key.clone(),
        format,
        enabled: true,
        credits_limit: None,
    };

    let mut next = previous.clone();
    next.api_key.push(key_config);
    next.proxy_service.push(service.clone());
    next.validate()
        .map_err(|error| RpcError::bad_params(error.to_string()))?;

    let raw = tokio::fs::read(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let output = toml::to_string_pretty(&next)
        .map_err(|error| RpcError::internal(format!("serialize config: {error}")))?;
    kproxy_store::atomic::write_bytes_atomically(
        &state.paths.config_file,
        output.as_bytes(),
        Some(0o600),
    )
    .await
    .map_err(|error| RpcError::internal(error.to_string()))?;

    state.apply_runtime_config(&next);
    state.config.replace(next.clone());
    state.mark_config_reloaded(now_secs());
    let failures = state.reconcile_proxy_services(&next).await;
    if let Some((_, error)) = failures
        .into_iter()
        .find(|(service_id, _)| service_id == &service.id)
    {
        let rollback_write = kproxy_store::atomic::write_bytes_atomically(
            &state.paths.config_file,
            &raw,
            Some(0o600),
        )
        .await;
        state.apply_runtime_config(&previous);
        state.config.replace(previous.clone());
        let _rollback_failures = state.reconcile_proxy_services(&previous).await;
        if let Err(rollback_error) = rollback_write {
            return Err(RpcError::internal(format!(
                "proxy service failed to start ({error}); config rollback failed: {rollback_error}"
            )));
        }
        return Err(RpcError::bad_params(format!(
            "proxy service failed to start: {error}"
        )));
    }

    let view = state
        .proxy_services
        .views(&next.proxy_service)
        .await
        .into_iter()
        .find(|view| view.id == service.id)
        .ok_or_else(|| RpcError::internal("created proxy service is missing"))?;
    to_value(ProxyServiceCreateResult {
        service: view,
        api_key: CreatedApiKey {
            id: key_id,
            name: key_name,
            key,
        },
    })
}

async fn handle_service_delete(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: ProxyServiceDeleteParams = parse_params(params)?;
    let selector = params.service.trim();
    if selector.is_empty() {
        return Err(RpcError::bad_params("service ID or name must not be empty"));
    }

    let _mutation = state.lock_config_mutation().await;
    let previous = state.config.current().as_ref().clone();
    let index = previous
        .proxy_service
        .iter()
        .position(|service| service.id == selector || service.name == selector)
        .ok_or_else(|| RpcError::bad_params(format!("proxy service not found: {selector}")))?;

    let mut next = previous.clone();
    let removed = next.proxy_service.remove(index);
    let (deleted_api_key_ids, retained_api_key_ids) =
        remove_unshared_service_api_keys(&mut next, &removed);
    next.validate()
        .map_err(|error| RpcError::bad_params(error.to_string()))?;
    let output = toml::to_string_pretty(&next)
        .map_err(|error| RpcError::internal(format!("serialize config: {error}")))?;
    kproxy_store::atomic::write_bytes_atomically(
        &state.paths.config_file,
        output.as_bytes(),
        Some(0o600),
    )
    .await
    .map_err(|error| RpcError::internal(error.to_string()))?;

    state.apply_runtime_config(&next);
    state.config.replace(next.clone());
    state.mark_config_reloaded(now_secs());
    let failures = state.reconcile_proxy_services(&next).await;
    for (service_id, error) in failures {
        warn!(%service_id, %error, "proxy service failed after service deletion");
    }

    to_value(ProxyServiceDeleteResult {
        service_id: removed.id,
        service_name: removed.name,
        deleted_api_key_ids,
        retained_api_key_ids,
    })
}

fn remove_unshared_service_api_keys(
    config: &mut Config,
    removed: &ProxyServiceConfig,
) -> (Vec<String>, Vec<String>) {
    let is_still_referenced = |key_id: &str| {
        config
            .proxy_service
            .iter()
            .any(|service| service.api_key_ids.iter().any(|id| id == key_id))
    };
    let retained = removed
        .api_key_ids
        .iter()
        .filter(|key_id| is_still_referenced(key_id))
        .cloned()
        .collect::<Vec<_>>();
    let deleted = removed
        .api_key_ids
        .iter()
        .filter(|key_id| !is_still_referenced(key_id))
        .cloned()
        .collect::<Vec<_>>();
    config.api_key.retain(|key| {
        key.id
            .as_ref()
            .is_none_or(|key_id| !deleted.contains(key_id))
    });
    (deleted, retained)
}

fn generate_api_key(format: ApiKeyFormat) -> String {
    let mut bytes = [0_u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let random = bytes.iter().fold(String::new(), |mut output, byte| {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
        output
    });
    match format {
        ApiKeyFormat::Sk => format!("sk-{random}"),
        ApiKeyFormat::Token => format!("token_{random}"),
        ApiKeyFormat::Simple => random,
    }
}

fn api_key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8]
        .iter()
        .fold(String::from("ak_"), |mut output, byte| {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
            output
        })
}

async fn handle_apikey_reset(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: AccountRefParams = parse_params(params)?;
    match state.meter.reset_usage(&params.id).await {
        Ok(true) => to_value(serde_json::json!({"id":params.id,"reset":true})),
        Ok(false) => Err(RpcError::bad_params(format!(
            "API key not found: {}",
            params.id
        ))),
        Err(error) => Err(RpcError::internal(error.to_string())),
    }
}

async fn persist_pool_snapshot(state: &Arc<AppState>) -> Result<(), RpcError> {
    let snapshot = state.pool().snapshot().await;
    let _transaction = state.lock_account_mutation().await;
    let mut next = state.with_accounts(Clone::clone);
    for account in snapshot {
        next.replace_if_changed(account);
    }
    next.save()
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    state.replace_accounts(next);
    Ok(())
}

#[cfg(test)]
mod tests {
    use kproxy_core::account::{Account, AuthMethod, Credentials};
    use kproxy_core::config::Config;
    use kproxy_core::paths::Paths;
    use kproxy_store::accounts::AccountStore;
    use kproxy_store::config_loader::ConfigHandle;
    use tempfile::TempDir;

    use super::*;

    fn sample_account(id: &str, email: &str, enabled: bool) -> Account {
        Account {
            id: id.into(),
            email: email.into(),
            label: None,
            enabled,
            machine_id: "a".repeat(64),
            profile_arn: None,
            credentials: Credentials {
                access_token: "at-secret".into(),
                refresh_token: Some("rt-secret".into()),
                client_id: Some("cid".into()),
                client_secret: Some("cs-secret".into()),
                region: "us-east-1".into(),
                expires_at: 1_700_000_000,
                auth_method: AuthMethod::Idc,
            },
            usage: None,
            subscription: None,
            tags: vec![],
            created_at: 0,
            credit_exhausted: false,
        }
    }

    #[test]
    fn sso_identity_matching_accepts_idc_username_variants_but_rejects_other_users() {
        assert!(sso_identities_match(
            "alice@example.com",
            "ALICE@example.com"
        ));
        assert!(sso_identities_match(
            "kiro.svc.70@patsnap.com",
            "kirosvc.70"
        ));
        assert!(!sso_identities_match(
            "kiro.svc.70@patsnap.com",
            "kirosvc.41"
        ));
        assert!(!sso_identities_match("", ""));
    }

    #[test]
    fn sso_identity_validation_fails_closed_when_upstream_omits_identity() {
        let error =
            validate_sso_identity("alice@example.com", &kproxy_kiro::UsageLimits::default())
                .expect_err("missing upstream identity must be rejected");
        assert!(error.message.contains("did not return"));
        assert!(error.message.contains("not saved"));
    }

    async fn state_with(accounts: Vec<Account>) -> (TempDir, Arc<AppState>) {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        kproxy_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("bootstrap");
        let mut store = AccountStore::load(&paths.accounts_file)
            .await
            .expect("load");
        for account in accounts {
            store.insert(account).expect("insert");
        }
        store.save().await.expect("save");
        let state = Arc::new(AppState::new(
            paths,
            ConfigHandle::new(Config::default()),
            store,
        ));
        (directory, state)
    }

    fn expect_ok(response: Response) -> serde_json::Value {
        match response {
            Response::Ok { result, .. } => result,
            Response::Err { error, .. } => {
                panic!("expected ok, got {}: {}", error.code, error.message)
            }
        }
    }

    #[tokio::test]
    async fn status_reports_counts_and_empty_hint() {
        let (_directory, state) = state_with(vec![
            sample_account("acc_00000001", "a@example.com", true),
            sample_account("acc_00000002", "b@example.com", false),
        ])
        .await;
        state.admission.set_maximum(123);
        let status: StatusResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(1, method::STATUS, serde_json::json!({})),
            )
            .await,
        ))
        .expect("status");
        assert_eq!(status.account_total, 2);
        assert_eq!(status.account_enabled, 1);
        assert_eq!(status.listen, "-");
        assert_eq!(status.proxy_service_total, 0);
        assert_eq!(status.proxy_service_running, 0);
        assert_eq!(status.max_concurrent_requests, 123);
        assert!(!status.ready);
        assert!(status
            .readiness_reasons
            .iter()
            .any(|reason| reason.contains("proxy service")));

        let (_directory, empty) = state_with(vec![]).await;
        let empty_status: StatusResult = serde_json::from_value(expect_ok(
            dispatch(
                &empty,
                Request::new(1, method::STATUS, serde_json::json!({})),
            )
            .await,
        ))
        .expect("empty status");
        assert!(empty_status.hint.is_some());
        assert!(!empty_status.ready);
    }

    #[tokio::test]
    async fn stats_default_is_compact_and_detail_restores_recent_requests() {
        let (_directory, state) = state_with(vec![]).await;
        state.stats.record(crate::stats::RequestLog {
            timestamp: now_secs(),
            trace_id: "trace_stats".into(),
            request_id: "req_stats".into(),
            path: "/v1/messages".into(),
            model: "claude-sonnet-4.6".into(),
            original_model: "claude-4.6-sonnet".into(),
            kiro_model: "claude-sonnet-4.6".into(),
            account_id: "acc_stats".into(),
            account_name: "Enterprise stats".into(),
            endpoint: "codewhisperer".into(),
            model_path: vec!["claude-4.6-sonnet".into(), "claude-sonnet-4.6".into()],
            model_mapping_rule: None,
            attempts: Vec::new(),
            duration_ms: 25,
            status: 200,
            input_tokens: 120,
            output_tokens: 30,
            credits: 0.5,
            error: None,
            diagnostics: crate::stats::RequestDiagnostics::default(),
        });

        let compact = expect_ok(
            dispatch(
                &state,
                Request::new(1, method::STATS, serde_json::json!({})),
            )
            .await,
        );
        assert_eq!(compact["summary"]["requests"], 1);
        assert_eq!(compact["latency"]["p50_ms"], 25);
        assert!(compact.get("stats").is_none());
        assert!(compact.get("grouped").is_none());

        let detail = expect_ok(
            dispatch(
                &state,
                Request::new(
                    2,
                    method::STATS,
                    serde_json::json!({"detail":true,"recent":20,"by":"model"}),
                ),
            )
            .await,
        );
        assert_eq!(
            detail["stats"]["recent_requests"][0]["account_name"],
            "Enterprise stats"
        );
        assert_eq!(detail["grouped"]["claude-sonnet-4.6"]["requests"], 1);
    }

    #[tokio::test]
    async fn creating_first_proxy_service_returns_a_scoped_api_key() {
        let (_directory, state) = state_with(vec![]).await;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
        let port = listener.local_addr().expect("address").port();
        drop(listener);

        let created: ProxyServiceCreateResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(
                    1,
                    method::SERVICE_CREATE,
                    serde_json::json!({"name":"first","port":port}),
                ),
            )
            .await,
        ))
        .expect("created service");
        assert!(created.service.running);
        assert_eq!(created.service.host, "0.0.0.0");
        assert_eq!(created.service.port, port);
        assert_eq!(
            created.service.api_key_ids,
            vec![created.api_key.id.clone()]
        );
        assert!(created.api_key.key.starts_with("sk-"));

        let listed: ProxyServiceListResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(2, method::SERVICE_LIST, serde_json::json!({})),
            )
            .await,
        ))
        .expect("service list");
        assert_eq!(listed.services.len(), 1);
        assert!(!serde_json::to_string(&listed)
            .expect("serialize list")
            .contains(&created.api_key.key));

        let hidden: ProxyServiceApiKeysResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(
                    3,
                    method::SERVICE_APIKEYS,
                    serde_json::json!({"service":"first"}),
                ),
            )
            .await,
        ))
        .expect("hidden service API keys");
        assert_eq!(hidden.service_id, created.service.id);
        assert_eq!(hidden.api_keys.len(), 1);
        assert!(hidden.api_keys[0].key.is_none());
        assert!(!serde_json::to_string(&hidden)
            .expect("serialize hidden keys")
            .contains(&created.api_key.key));

        let revealed: ProxyServiceApiKeysResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(
                    4,
                    method::SERVICE_APIKEYS,
                    serde_json::json!({
                        "service":created.service.id,
                        "show_secret":true
                    }),
                ),
            )
            .await,
        ))
        .expect("revealed service API keys");
        assert_eq!(
            revealed.api_keys[0].key.as_deref(),
            Some(created.api_key.key.as_str())
        );

        let health: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/health"))
            .await
            .expect("health request")
            .json()
            .await
            .expect("health JSON");
        assert_eq!(health["status"], "ok");
        assert_eq!(health["available_accounts"], 0);

        let deleted: ProxyServiceDeleteResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(
                    5,
                    method::SERVICE_DELETE,
                    serde_json::json!({"service":"first"}),
                ),
            )
            .await,
        ))
        .expect("deleted service");
        assert_eq!(deleted.service_id, created.service.id);
        assert_eq!(deleted.service_name, created.service.name);
        assert_eq!(deleted.deleted_api_key_ids.len(), 1);
        assert_eq!(deleted.deleted_api_key_ids[0], created.api_key.id);
        assert!(deleted.retained_api_key_ids.is_empty());

        let listed_after_delete: ProxyServiceListResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(6, method::SERVICE_LIST, serde_json::json!({})),
            )
            .await,
        ))
        .expect("service list after delete");
        assert!(listed_after_delete.services.is_empty());
        assert!(!state
            .config
            .current()
            .api_key
            .iter()
            .any(|key| key.id.as_deref() == Some(created.api_key.id.as_str())));
        assert!(state
            .meter
            .authenticate(Some(&created.api_key.key))
            .expect("empty key registry permits unauthenticated requests")
            .is_none());
        let persisted = tokio::fs::read_to_string(&state.paths.config_file)
            .await
            .expect("persisted config");
        assert!(!persisted.contains(&created.api_key.id));
        assert!(!persisted.contains(&created.api_key.key));
        state.shutdown.cancel();
    }

    #[test]
    fn service_key_cleanup_preserves_keys_shared_with_other_services() {
        let exclusive = ApiKeyConfig {
            id: Some("ak_exclusive".into()),
            name: "exclusive".into(),
            key: "sk-exclusive".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            credits_limit: None,
        };
        let shared = ApiKeyConfig {
            id: Some("ak_shared".into()),
            name: "shared".into(),
            key: "sk-shared".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            credits_limit: None,
        };
        let removed = ProxyServiceConfig {
            id: "svc_removed".into(),
            name: "removed".into(),
            host: "127.0.0.1".into(),
            port: 5580,
            enabled: true,
            api_key_ids: vec!["ak_exclusive".into(), "ak_shared".into()],
            created_at: 0,
        };
        let remaining = ProxyServiceConfig {
            id: "svc_remaining".into(),
            name: "remaining".into(),
            host: "127.0.0.1".into(),
            port: 5581,
            enabled: true,
            api_key_ids: vec!["ak_shared".into()],
            created_at: 0,
        };
        let mut config = Config {
            api_key: vec![exclusive, shared],
            proxy_service: vec![remaining],
            ..Config::default()
        };

        let (deleted, retained) = remove_unshared_service_api_keys(&mut config, &removed);

        assert_eq!(deleted, ["ak_exclusive"]);
        assert_eq!(retained, ["ak_shared"]);
        assert_eq!(config.api_key.len(), 1);
        assert_eq!(config.api_key[0].id.as_deref(), Some("ak_shared"));
    }

    #[tokio::test]
    async fn account_lifecycle_persists_without_exposing_tokens() {
        let (_directory, state) = state_with(vec![]).await;
        let account = sample_account("acc_00000001", "a@example.com", true);
        let imported: AccountImportResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(
                    1,
                    method::ACCOUNT_IMPORT,
                    serde_json::json!({"accounts": [account]}),
                ),
            )
            .await,
        ))
        .expect("import");
        assert_eq!(imported.imported, 1);

        let list = expect_ok(
            dispatch(
                &state,
                Request::new(2, method::ACCOUNT_LIST, serde_json::json!({})),
            )
            .await,
        );
        assert_eq!(list["accounts"].as_array().expect("array").len(), 1);
        assert!(!serde_json::to_string(&list)
            .expect("serialize")
            .contains("at-secret"));

        expect_ok(
            dispatch(
                &state,
                Request::new(
                    3,
                    method::ACCOUNT_TAG,
                    serde_json::json!({"id": "a@example.com", "add": ["prod"]}),
                ),
            )
            .await,
        );
        let raw = tokio::fs::read_to_string(&state.paths.accounts_file)
            .await
            .expect("read disk");
        assert!(raw.contains("prod"));
    }

    #[tokio::test]
    async fn account_lists_and_exports_default_to_email_order() {
        let (_directory, state) = state_with(vec![
            sample_account("acc_00000001", "z@example.com", true),
            sample_account("acc_00000002", "a@example.com", true),
            sample_account("acc_00000003", "B@example.com", true),
        ])
        .await;

        let list: AccountListResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(1, method::ACCOUNT_LIST, serde_json::json!({})),
            )
            .await,
        ))
        .expect("account list");
        assert_eq!(
            list.accounts
                .iter()
                .map(|account| account.email.as_str())
                .collect::<Vec<_>>(),
            ["a@example.com", "B@example.com", "z@example.com"]
        );

        let by_id: AccountListResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(2, method::ACCOUNT_LIST, serde_json::json!({"sort":"id"})),
            )
            .await,
        ))
        .expect("account list by ID");
        assert_eq!(
            by_id
                .accounts
                .iter()
                .map(|account| account.email.as_str())
                .collect::<Vec<_>>(),
            ["z@example.com", "a@example.com", "B@example.com"]
        );

        let exported = expect_ok(
            dispatch(
                &state,
                Request::new(
                    3,
                    method::ACCOUNT_EXPORT,
                    serde_json::json!({"redact":true}),
                ),
            )
            .await,
        );
        assert_eq!(
            exported
                .as_array()
                .expect("exported accounts")
                .iter()
                .map(|account| account["email"].as_str().expect("email"))
                .collect::<Vec<_>>(),
            ["a@example.com", "B@example.com", "z@example.com"]
        );

        let invalid = dispatch(
            &state,
            Request::new(
                4,
                method::ACCOUNT_LIST,
                serde_json::json!({"sort":"unknown"}),
            ),
        )
        .await;
        assert!(matches!(
            invalid,
            Response::Err { error, .. } if error.message.contains("unsupported account sort field")
        ));
    }

    #[tokio::test]
    async fn administrative_lists_use_stable_name_order() {
        let (_directory, state) = state_with(vec![]).await;
        let mut config = Config::default();
        config.webhook = ["Zulu", "alpha"]
            .map(|name| kproxy_core::config::WebhookConfig {
                name: name.into(),
                kind: "custom".into(),
                url: format!("https://example.com/{name}"),
                enabled: true,
                events: vec![],
                dingtalk_sign: None,
                telegram_chat_id: None,
                custom_template: None,
            })
            .into();
        config.api_key = [
            ("ak_zulu", "Zulu key", "secret-zulu"),
            ("ak_alpha", "alpha key", "secret-alpha"),
        ]
        .map(|(id, name, key)| ApiKeyConfig {
            id: Some(id.into()),
            name: name.into(),
            key: key.into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            credits_limit: None,
        })
        .into();
        config.proxy_service = [
            ("svc_zulu", "Zulu service", 6001),
            ("svc_alpha", "alpha service", 6002),
        ]
        .map(|(id, name, port)| ProxyServiceConfig {
            id: id.into(),
            name: name.into(),
            host: "127.0.0.1".into(),
            port,
            enabled: false,
            api_key_ids: vec!["ak_zulu".into(), "ak_alpha".into()],
            created_at: 0,
        })
        .into();
        state.config.replace(config);

        let services: ProxyServiceListResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(1, method::SERVICE_LIST, serde_json::json!({})),
            )
            .await,
        ))
        .expect("service list");
        assert_eq!(
            services
                .services
                .iter()
                .map(|service| service.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha service", "Zulu service"]
        );

        let keys: ProxyServiceApiKeysResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(
                    2,
                    method::SERVICE_APIKEYS,
                    serde_json::json!({"service":"svc_alpha","show_secret":false}),
                ),
            )
            .await,
        ))
        .expect("service API keys");
        assert_eq!(
            keys.api_keys
                .iter()
                .map(|key| key.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha key", "Zulu key"]
        );

        let webhooks = expect_ok(
            dispatch(
                &state,
                Request::new(3, method::WEBHOOK_LIST, serde_json::json!({})),
            )
            .await,
        );
        assert_eq!(
            webhooks
                .as_array()
                .expect("webhook list")
                .iter()
                .map(|target| target["name"].as_str().expect("name"))
                .collect::<Vec<_>>(),
            ["alpha", "Zulu"]
        );

        let mut models = ["z-model", "A-model", "b-model"].map(|model_id| kproxy_kiro::ModelInfo {
            model_id: model_id.into(),
            model_name: model_id.into(),
            description: String::new(),
            rate_multiplier: None,
            token_limits: None,
        });
        sort_models_for_display(&mut models);
        assert_eq!(
            models
                .iter()
                .map(|model| model.model_id.as_str())
                .collect::<Vec<_>>(),
            ["A-model", "b-model", "z-model"]
        );
    }

    #[tokio::test]
    async fn config_reload_keeps_old_value_on_error() {
        let (_directory, state) = state_with(vec![]).await;
        tokio::fs::write(&state.paths.config_file, "[server\nport = ")
            .await
            .expect("break");
        let result: ConfigReloadResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(1, method::CONFIG_RELOAD, serde_json::json!({})),
            )
            .await,
        ))
        .expect("reload");
        assert!(!result.applied);
        assert_eq!(state.config.current().server.port, 5580);
    }

    #[tokio::test]
    async fn config_reload_applies_service_defaults_but_not_socket_fields() {
        let (_directory, state) = state_with(vec![]).await;
        tokio::fs::write(
            &state.paths.config_file,
            "[server]\nport = 6100\n\n[features]\nenable_prompt_cache = true\n",
        )
        .await
        .expect("edit");
        let result: ConfigReloadResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(1, method::CONFIG_RELOAD, serde_json::json!({})),
            )
            .await,
        ))
        .expect("reload");
        assert!(result.applied);
        assert!(result.needs_restart.is_empty());
        assert_eq!(state.config.current().server.port, 6100);
        assert!(state.config.current().features.enable_prompt_cache);
    }

    #[tokio::test]
    async fn config_reload_rolls_back_when_proxy_listener_cannot_start() {
        let (_directory, state) = state_with(vec![]).await;
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("occupied port");
        let port = occupied.local_addr().expect("address").port();
        let mut next = state.config.current().as_ref().clone();
        next.api_key.push(ApiKeyConfig {
            id: Some("ak_reload".into()),
            name: "reload".into(),
            key: "sk-reload".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            credits_limit: None,
        });
        next.proxy_service.push(ProxyServiceConfig {
            id: "svc_reload".into(),
            name: "reload".into(),
            host: "127.0.0.1".into(),
            port,
            enabled: true,
            api_key_ids: vec!["ak_reload".into()],
            created_at: 0,
        });
        tokio::fs::write(
            &state.paths.config_file,
            toml::to_string_pretty(&next).expect("serialize"),
        )
        .await
        .expect("write config");

        let result: ConfigReloadResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(1, method::CONFIG_RELOAD, serde_json::json!({})),
            )
            .await,
        ))
        .expect("reload result");

        assert!(!result.applied);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("svc_reload")));
        assert!(state.config.current().proxy_service.is_empty());
        assert!(state.config.current().api_key.is_empty());
    }
}

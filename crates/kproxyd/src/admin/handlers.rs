//! 管理面各方法实现。

use std::sync::Arc;

use futures::{stream, StreamExt};
use kproxy_core::account::Account;
use kproxy_core::config::{ApiKeyConfig, ApiKeyFormat, ProxyServiceConfig};
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
    let targets = state
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
        max_concurrent_requests: config.server.max_concurrent_requests,
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
    state.apply_runtime_config(&next);
    state.config.replace(next.clone());
    state.mark_config_reloaded(now_secs());
    let failures = state.reconcile_proxy_services(&next).await;
    for (service_id, error) in failures {
        warn!(%service_id, %error, "proxy service failed after config reload");
    }
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
    AccountSummary {
        id: account.id.clone(),
        email: account.email.clone(),
        label: account.label.clone(),
        enabled: account.enabled,
        health: None,
        tags: account.tags.clone(),
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
            let left = left.credit_current.unwrap_or(f64::INFINITY);
            let right = right.credit_current.unwrap_or(f64::INFINITY);
            left.partial_cmp(&right)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        Some("email") => accounts.sort_by(|left, right| left.email.cmp(&right.email)),
        Some("id") | None => accounts.sort_by(|left, right| left.id.cmp(&right.id)),
        Some(_) => {}
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
    let account = Account {
        id: new_account_id(),
        email,
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
    let summary = summarize(&account);
    commit_account_change(state, move |store| {
        store
            .insert(account)
            .map_err(|error| RpcError::bad_params(error.to_string()))
    })
    .await?;
    to_value(summary)
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
        return to_value(crate::http::fallback_models(&config));
    }
    let (cached, fresh) = state.models.get(config.models.cache_ttl_ms);
    if fresh {
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
    to_value(models)
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
        "This is a test notification from kproxy.",
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
    to_value(ProxyServiceListResult {
        services: state.proxy_services.views(&config.proxy_service).await,
    })
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
    let api_keys = service
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
        .collect();
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
        let rollback_write =
            kproxy_store::atomic::write_bytes_atomically(&state.paths.config_file, &raw, Some(0o600))
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
    })
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

        let listed_after_delete: ProxyServiceListResult = serde_json::from_value(expect_ok(
            dispatch(
                &state,
                Request::new(6, method::SERVICE_LIST, serde_json::json!({})),
            )
            .await,
        ))
        .expect("service list after delete");
        assert!(listed_after_delete.services.is_empty());
        assert!(state
            .config
            .current()
            .api_key
            .iter()
            .any(|key| key.id.as_deref() == Some(created.api_key.id.as_str())));
        state.shutdown.cancel();
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
}

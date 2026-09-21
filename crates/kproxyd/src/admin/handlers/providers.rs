use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use kproxy_core::provider::{ProviderDescriptor, ProviderId, ALL_PROVIDERS};
use kproxy_ipc::protocol::{
    ProviderAccountListResult, ProviderAccountSummary, ProviderModelListResult, ProviderSelector,
    RpcError,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{parse_params, to_value, Handled};
use crate::state::AppState;

pub(super) async fn handle_capabilities(state: &Arc<AppState>) -> Handled {
    let descriptors = state.providers.descriptors().await;
    to_value(json!({
        "schema_version": 2,
        "rpc_versions": [1, 2],
        "provider_kinds": descriptors.iter().map(|provider| provider.kind.as_str()).collect::<BTreeSet<_>>(),
        "features": [
            "provider_registry",
            "provider_scoped_accounts",
            "provider_scoped_account_tags",
            "provider_scoped_account_probe",
            "provider_scoped_models",
            "provider_scoped_model_mapping",
            "provider_scoped_statistics",
            "copilot_device_flow",
            "copilot_native_messages",
            "copilot_native_chat_completions",
            "copilot_native_responses"
        ]
    }))
}

pub(super) async fn handle_provider_list(state: &Arc<AppState>, params: Value) -> Handled {
    let selector = parse_selector(params)?;
    let providers = filter_descriptors(state.providers.descriptors().await, &selector)?;
    to_value(json!({
        "schema_version": 2,
        "scope": selector,
        "providers": providers,
        "complete": true,
        "errors": {}
    }))
}

pub(super) async fn handle_provider_show(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: String,
    }
    let params: Params = parse_params(params)?;
    let id = parse_provider_id(&params.provider)?;
    let descriptor = state
        .providers
        .descriptors()
        .await
        .into_iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| RpcError::bad_params(format!("provider not found: {id}")))?;
    to_value(descriptor)
}

pub(super) async fn handle_account_list(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Default, Deserialize)]
    struct Params {
        #[serde(flatten)]
        selector: ProviderSelector,
        tag: Option<String>,
        #[serde(default)]
        enabled_only: bool,
        status: Option<String>,
        sort: Option<String>,
    }
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        parse_params(params)?
    };
    let descriptors = filter_descriptors(state.providers.descriptors().await, &params.selector)?;
    let mut accounts = Vec::new();
    let mut errors = BTreeMap::new();
    for descriptor in descriptors {
        match descriptor.kind.as_str() {
            "kiro" => accounts.extend(kiro_accounts(state, &descriptor).await),
            "copilot" => match state.providers.copilot(&descriptor.id).await {
                Some(runtime) => accounts.extend(
                    runtime
                        .account_summaries()
                        .await
                        .into_iter()
                        .map(|account| {
                            let (enabled, health) =
                                copilot_account_state(descriptor.enabled, &account);
                            ProviderAccountSummary {
                                provider_id: account.provider_id,
                                provider_kind: account.provider_kind,
                                id: account.id,
                                display_name: account.login,
                                email: account.email,
                                label: account.label,
                                enabled,
                                health: health.into(),
                                auth_state: account.auth_state,
                                tags: account.tags,
                                quota_current: None,
                                quota_limit: None,
                                quota_unit: None,
                                supported_models: account.supported_models,
                                details: json!({"endpoint":account.endpoint,"last_error":account.last_error,"created_at":account.created_at}),
                            }
                        }),
                ),
                None => {
                    errors.insert(descriptor.id.to_string(), "Copilot runtime is unavailable".into());
                }
            },
            _ => {
                errors.insert(
                    descriptor.id.to_string(),
                    format!("provider driver {} is unsupported", descriptor.kind),
                );
            }
        }
    }
    accounts.retain(|account| {
        params
            .tag
            .as_ref()
            .is_none_or(|tag| account.tags.contains(tag))
            && (!params.enabled_only || account.enabled)
            && params
                .status
                .as_ref()
                .is_none_or(|status| account.health == *status)
    });
    match params.sort.as_deref() {
        Some("id") => accounts.sort_by(|left, right| {
            left.provider_id
                .cmp(&right.provider_id)
                .then_with(|| left.id.cmp(&right.id))
        }),
        Some("credit") => accounts.sort_by(|left, right| {
            right
                .quota_current
                .partial_cmp(&left.quota_current)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        Some("email") | None => accounts.sort_by(|left, right| {
            left.display_name
                .to_ascii_lowercase()
                .cmp(&right.display_name.to_ascii_lowercase())
                .then_with(|| left.provider_id.cmp(&right.provider_id))
        }),
        Some(other) => {
            return Err(RpcError::bad_params(format!(
                "unknown account sort: {other}"
            )))
        }
    }
    to_value(ProviderAccountListResult {
        schema_version: 2,
        scope: params.selector,
        accounts,
        complete: errors.is_empty(),
        errors,
    })
}

pub(super) async fn handle_account_show(state: &Arc<AppState>, params: Value) -> Handled {
    let reference = parse_account_ref(state, params).await?;
    let result = account_list_for_provider(state, &reference.provider_id).await?;
    let account = result
        .accounts
        .into_iter()
        .find(|account| {
            account.id == reference.account_id
                || account.display_name == reference.account_id
                || account.email.as_deref() == Some(reference.account_id.as_str())
        })
        .ok_or_else(|| RpcError::bad_params(format!("account not found: {reference}")))?;
    to_value(account)
}

pub(super) async fn handle_account_export(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Default, Deserialize)]
    struct Params {
        #[serde(flatten)]
        selector: ProviderSelector,
        #[serde(default)]
        redact: bool,
    }
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        parse_params(params)?
    };
    let descriptors = filter_descriptors(state.providers.descriptors().await, &params.selector)?;
    let mut providers = BTreeMap::new();
    let mut errors = BTreeMap::new();
    for descriptor in descriptors {
        let value = match descriptor.kind.as_str() {
            "kiro" => super::handle_account_export(state, json!({"redact":params.redact})),
            "copilot" => match state.providers.copilot(&descriptor.id).await {
                Some(runtime) => Ok(runtime.export_accounts(params.redact).await),
                None => Err(RpcError::internal(format!(
                    "provider runtime {} is unavailable",
                    descriptor.id
                ))),
            },
            kind => Err(RpcError::bad_params(format!(
                "account export is not supported for provider kind {kind}"
            ))),
        };
        match value {
            Ok(accounts) => {
                providers.insert(descriptor.id.to_string(), accounts);
            }
            Err(error) => {
                errors.insert(descriptor.id.to_string(), error.message);
            }
        }
    }
    to_value(json!({
        "schema_version":2,
        "scope":params.selector,
        "redacted":params.redact,
        "providers":providers,
        "complete":errors.is_empty(),
        "errors":errors
    }))
}

pub(super) async fn handle_account_import_token(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: String,
        token: String,
        label: Option<String>,
    }
    let params: Params = parse_params(params)?;
    if params.token.trim().is_empty() {
        return Err(RpcError::bad_params("token must not be empty"));
    }
    let runtime = copilot_runtime(state, &params.provider).await?;
    let result = runtime
        .import_token(params.token, params.label)
        .await
        .map_err(provider_rpc_error)?;
    to_value(result)
}

pub(super) async fn handle_login_start(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: String,
        #[serde(default = "default_device_auth")]
        auth: String,
        label: Option<String>,
    }
    fn default_device_auth() -> String {
        "device-flow".into()
    }
    let params: Params = parse_params(params)?;
    if params.auth != "device-flow" {
        return Err(RpcError::bad_params(format!(
            "provider login auth {} is unsupported",
            params.auth
        )));
    }
    let runtime = copilot_runtime(state, &params.provider).await?;
    to_value(
        runtime
            .start_device_login(params.label)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_login_status(state: &Arc<AppState>, params: Value) -> Handled {
    let (provider, task_id) = parse_login_task(params)?;
    let runtime = copilot_runtime(state, &provider).await?;
    to_value(
        runtime
            .poll_device_login(&task_id)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_login_cancel(state: &Arc<AppState>, params: Value) -> Handled {
    let (provider, task_id) = parse_login_task(params)?;
    let runtime = copilot_runtime(state, &provider).await?;
    to_value(
        runtime
            .cancel_device_login(&task_id)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_account_set_enabled(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
        enabled: bool,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_set_enabled(
            state,
            json!({"id":reference.account_id,"enabled":params.enabled}),
        )
        .await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    to_value(
        runtime
            .set_account_enabled(&reference.account_id, params.enabled)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_account_remove(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_remove(state, json!({"id":reference.account_id})).await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    to_value(
        runtime
            .remove_account(&reference.account_id)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_account_refresh(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_refresh(
            state,
            json!({"id":reference.account_id,"all":false}),
        )
        .await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    to_value(
        runtime
            .refresh_account(&reference.account_id)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_account_tag(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
        #[serde(default)]
        add: Vec<String>,
        #[serde(default)]
        remove: Vec<String>,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_tag(
            state,
            json!({"id":reference.account_id,"add":params.add,"remove":params.remove}),
        )
        .await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    to_value(
        runtime
            .update_account_tags(&reference.account_id, &params.add, &params.remove)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_account_probe(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_probe(state, json!({"id":reference.account_id,"all":false}))
            .await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    let account = runtime
        .refresh_account(&reference.account_id)
        .await
        .map_err(provider_rpc_error)?;
    to_value(json!({
        "provider_id":reference.provider_id,
        "account_id":account.id,
        "models":account.supported_models,
        "ok":true,
        "account":account
    }))
}

pub(super) async fn handle_account_reset_health(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_reset_health(
            state,
            json!({"id":reference.account_id,"all":false}),
        )
        .await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    let account = runtime
        .reset_account_health(&reference.account_id)
        .await
        .map_err(provider_rpc_error)?;
    to_value(json!({
        "provider_id":reference.provider_id,
        "reset":[account.id],
        "account":account
    }))
}

pub(super) async fn handle_models(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Default, Deserialize)]
    struct Params {
        #[serde(flatten)]
        selector: ProviderSelector,
        #[serde(default)]
        refresh: bool,
    }
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        parse_params(params)?
    };
    let mut descriptors =
        filter_descriptors(state.providers.descriptors().await, &params.selector)?;
    if params
        .selector
        .provider
        .as_deref()
        .is_none_or(|provider| provider == ALL_PROVIDERS)
    {
        descriptors.retain(|provider| provider.enabled);
    }
    if descriptors.is_empty() {
        return Err(RpcError::bad_params(
            "no enabled provider matches the requested scope",
        ));
    }
    let mut refresh_errors = BTreeMap::new();
    if params.refresh
        && descriptors
            .iter()
            .any(|provider| provider.kind == "kiro" && provider.enabled)
    {
        if let Err(error) = crate::tasks::refresh_models_for(state, Some("kiro")).await {
            tracing::warn!(%error, "provider-scoped Kiro model refresh failed");
            refresh_errors.insert("kiro".into(), error.to_string());
        }
    }
    let ids = descriptors
        .into_iter()
        .map(|provider| provider.id)
        .collect::<Vec<_>>();
    let (models, mut errors) = state.providers.models(&ids, params.refresh).await;
    errors.append(&mut refresh_errors);
    to_value(ProviderModelListResult {
        schema_version: 2,
        scope: params.selector,
        models,
        complete: errors.is_empty(),
        errors,
    })
}

fn copilot_account_state(
    provider_enabled: bool,
    account: &kproxy_copilot::CopilotAccountSummary,
) -> (bool, &'static str) {
    let enabled = provider_enabled && account.enabled;
    let health = if !enabled {
        "disabled"
    } else if account.is_available() {
        "available"
    } else {
        "unavailable"
    };
    (enabled, health)
}

async fn kiro_accounts(
    state: &Arc<AppState>,
    descriptor: &ProviderDescriptor,
) -> Vec<ProviderAccountSummary> {
    let config = state.config.current();
    let pool = state.pool();
    let mut output = Vec::new();
    for account in pool.snapshot().await {
        let health = if descriptor.enabled {
            super::effective_account_health(&pool, &account, &config.pool).await
        } else {
            "disabled".into()
        };
        let supported_models = match pool.get(&account.id).await {
            Some(runtime) if runtime.has_model_cache().await => runtime.supported_models().await,
            _ => Vec::new(),
        };
        output.push(ProviderAccountSummary {
            provider_id: descriptor.id.to_string(),
            provider_kind: "kiro".into(),
            id: account.id.clone(),
            display_name: account.display_name().to_owned(),
            email: Some(account.email.clone()),
            label: account.label.clone(),
            enabled: descriptor.enabled && account.enabled,
            health,
            auth_state: if account.credentials.expires_at > 0
                && account.credentials.expires_at <= crate::meter::now_secs()
            {
                "expired"
            } else {
                "ready"
            }
            .into(),
            tags: account.tags.clone(),
            quota_current: account.usage.as_ref().map(|usage| usage.current),
            quota_limit: account.usage.as_ref().map(|usage| usage.limit),
            quota_unit: Some("kiro_credits".into()),
            supported_models,
            details: json!({
                "region":account.credentials.region,
                "auth_method":account.credentials.auth_method,
                "machine_id":account.machine_id,
                "created_at":account.created_at
            }),
        });
    }
    output
}

fn parse_selector(params: Value) -> Result<ProviderSelector, RpcError> {
    if params.is_null() {
        Ok(ProviderSelector::default())
    } else {
        parse_params(params)
    }
}

fn filter_descriptors(
    descriptors: Vec<ProviderDescriptor>,
    selector: &ProviderSelector,
) -> Result<Vec<ProviderDescriptor>, RpcError> {
    if selector.provider.as_deref() == Some("") {
        return Err(RpcError::bad_params("provider must not be empty"));
    }
    let mut output = descriptors
        .into_iter()
        .filter(|descriptor| {
            selector.provider.as_deref().is_none_or(|selected| {
                selected == ALL_PROVIDERS || descriptor.id.as_str() == selected
            }) && selector
                .provider_kind
                .as_deref()
                .is_none_or(|kind| descriptor.kind == kind)
        })
        .collect::<Vec<_>>();
    output.sort_by(|left, right| left.id.cmp(&right.id));
    if output.is_empty() {
        return Err(RpcError::bad_params(
            "no provider matches the requested scope",
        ));
    }
    Ok(output)
}

async fn account_list_for_provider(
    state: &Arc<AppState>,
    provider_id: &ProviderId,
) -> Result<ProviderAccountListResult, RpcError> {
    let value = handle_account_list(state, json!({"provider":provider_id.as_str()})).await?;
    serde_json::from_value(value).map_err(|error| RpcError::internal(error.to_string()))
}

#[derive(Debug)]
struct ResolvedAccountRef {
    provider_id: ProviderId,
    account_id: String,
}

impl std::fmt::Display for ResolvedAccountRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.provider_id, self.account_id)
    }
}

async fn parse_account_ref(
    state: &Arc<AppState>,
    params: Value,
) -> Result<ResolvedAccountRef, RpcError> {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
    }
    let params: Params = parse_params(params)?;
    resolve_account_ref(state, params.provider.as_deref(), &params.id).await
}

async fn resolve_account_ref(
    state: &Arc<AppState>,
    explicit_provider: Option<&str>,
    id: &str,
) -> Result<ResolvedAccountRef, RpcError> {
    let (qualified_provider, account_id) = id
        .split_once('/')
        .map_or((None, id), |(provider, account)| (Some(provider), account));
    if explicit_provider.is_some()
        && qualified_provider.is_some()
        && explicit_provider != qualified_provider
    {
        return Err(RpcError::bad_params(
            "qualified account reference conflicts with --provider",
        ));
    }
    if let Some(provider) = explicit_provider.or(qualified_provider) {
        return Ok(ResolvedAccountRef {
            provider_id: parse_provider_id(provider)?,
            account_id: account_id.into(),
        });
    }
    let list: ProviderAccountListResult =
        serde_json::from_value(handle_account_list(state, Value::Null).await?)
            .map_err(|error| RpcError::internal(error.to_string()))?;
    let matches = list
        .accounts
        .into_iter()
        .filter(|account| {
            account.id == account_id
                || account.display_name == account_id
                || account.email.as_deref() == Some(account_id)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [account] => Ok(ResolvedAccountRef {
            provider_id: parse_provider_id(&account.provider_id)?,
            account_id: account.id.clone(),
        }),
        [] => Err(RpcError::bad_params(format!("account not found: {id}"))),
        _ => Err(RpcError::bad_params(format!(
            "account reference {id} is ambiguous; use provider/account_id"
        ))),
    }
}

fn parse_provider_id(value: &str) -> Result<ProviderId, RpcError> {
    ProviderId::parse(value).map_err(|error| RpcError::bad_params(error.to_string()))
}

async fn copilot_runtime(
    state: &Arc<AppState>,
    provider: &str,
) -> Result<Arc<kproxy_copilot::CopilotProvider>, RpcError> {
    let id = parse_provider_id(provider)?;
    state
        .providers
        .copilot(&id)
        .await
        .ok_or_else(|| RpcError::bad_params(format!("provider {id} is not a Copilot provider")))
}

fn parse_login_task(params: Value) -> Result<(String, String), RpcError> {
    #[derive(Deserialize)]
    struct Params {
        provider: String,
        task_id: String,
    }
    let params: Params = parse_params(params)?;
    Ok((params.provider, params.task_id))
}

fn provider_rpc_error(error: kproxy_provider::ProviderError) -> RpcError {
    RpcError {
        code: i32::from(error.status.as_u16()),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::copilot_account_state;
    use kproxy_copilot::CopilotAccountSummary;

    fn account() -> CopilotAccountSummary {
        CopilotAccountSummary {
            provider_id: "copilot".into(),
            provider_kind: "copilot".into(),
            id: "gh_1".into(),
            login: "octocat".into(),
            email: None,
            label: None,
            enabled: true,
            auth_state: "ready".into(),
            tags: Vec::new(),
            supported_models: vec!["gpt-test".into()],
            endpoint: None,
            created_at: 0,
            last_error: None,
        }
    }

    #[test]
    fn disabled_provider_makes_its_copilot_accounts_unavailable() {
        assert_eq!(
            copilot_account_state(false, &account()),
            (false, "disabled")
        );
        assert_eq!(copilot_account_state(true, &account()), (true, "available"));
    }
}

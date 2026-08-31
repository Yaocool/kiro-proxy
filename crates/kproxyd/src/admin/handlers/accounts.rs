use super::{
    account_credit_state, compare_display_text, new_account_id, new_machine_id, now_secs,
    parse_params, persist_pool_snapshot, stream, summarize, to_value, warn, Account,
    AccountCreditState, AccountDetail, AccountImportParams, AccountImportResult, AccountListParams,
    AccountListResult, AccountPool, AccountRefParams, AccountSetEnabledParams, AccountStore,
    AccountTagParams, AppState, Arc, Handled, RpcError, StreamExt,
};

pub(super) async fn effective_account_health(
    pool: &AccountPool,
    account: &Account,
    config: &kproxy_core::config::PoolConfig,
) -> String {
    if !account.enabled {
        return "disabled".into();
    }
    match account_credit_state(account, config) {
        AccountCreditState::Exhausted => return "exhausted".into(),
        AccountCreditState::Protected => return "low_credit".into(),
        AccountCreditState::Available => {}
    }
    pool.get(&account.id)
        .await
        .map(|runtime| format!("{:?}", runtime.health()).to_ascii_lowercase())
        .unwrap_or_else(|| "unavailable".into())
}

pub(super) fn compare_account_identity(
    left_email: &str,
    left_id: &str,
    right_email: &str,
    right_id: &str,
) -> std::cmp::Ordering {
    compare_display_text(left_email, right_email).then_with(|| left_id.cmp(right_id))
}

pub(super) async fn handle_account_list(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
    let params: AccountListParams = if params.is_null() {
        AccountListParams::default()
    } else {
        parse_params(params)?
    };
    let source = state.with_accounts(|store| store.all().to_vec());
    let pool = state.pool();
    let pool_config = state.runtime_config_snapshot().pool;
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
        summary.health = Some(effective_account_health(&pool, account, &pool_config).await);
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

pub(super) async fn handle_account_show(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
    let params: AccountRefParams = parse_params(params)?;
    let account = state
        .with_accounts(|store| store.find(&params.id).cloned())
        .ok_or_else(|| RpcError::bad_params(format!("account not found: {}", params.id)))?;
    let pool = state.pool();
    let runtime = pool.get(&account.id).await;
    let mut summary = summarize(&account);
    summary.health = Some(
        effective_account_health(&pool, &account, &state.runtime_config_snapshot().pool).await,
    );
    let (supported_models, active_requests) = if let Some(runtime) = runtime {
        (runtime.supported_models().await, runtime.active())
    } else {
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

pub(super) async fn commit_account_change<F, T>(
    state: &Arc<AppState>,
    mutate: F,
) -> Result<T, RpcError>
where
    F: FnOnce(&mut AccountStore) -> Result<T, RpcError>,
{
    let transaction = state
        .lock_account_storage()
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let mut next = AccountStore::load(&state.paths.accounts_file)
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    state.configure_account_store(&mut next);
    let result = mutate(&mut next)?;
    next.save()
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let pool_accounts = next.all().to_vec();
    state.replace_accounts(next);
    state.pool().replace_accounts(pool_accounts).await;
    drop(transaction);
    state.request_model_refresh();
    crate::alerts::sync_quota_incidents(state).await;
    Ok(result)
}

pub(super) async fn handle_account_import(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
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

pub(super) fn validate_account_input(account: &Account) -> Result<(), RpcError> {
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

pub(super) fn default_sso_region() -> String {
    "us-east-1".into()
}

pub(super) async fn handle_account_add_sso(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
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
        upstream_user_id: None,
        credentials,
        usage: None,
        subscription: None,
        tags: Vec::new(),
        created_at: now_secs(),
        credit_exhausted: false,
    };
    match state.kiro().resolve_profile_arn(&account).await {
        Ok(profile_arn) => account.profile_arn = Some(profile_arn),
        Err(error) => {
            let safe_error = kproxy_translate::sanitize_error_message(&error.to_string());
            warn!(
                requested_email = %email,
                validation_step = "profile discovery",
                upstream_endpoint = %error.endpoint,
                upstream_status = ?error.status,
                error = %safe_error,
                "Kiro profile discovery failed; continuing with token validation without a profile ARN"
            );
        }
    }
    let limits = state
        .kiro()
        .get_usage_limits(&account)
        .await
        .map_err(|error| sso_account_validation_error(&email, "usage limits", &error))?;
    let upstream_user_id = authenticated_sso_user_id(&limits)?;
    if let Some(actual_identity) = limits
        .user_info
        .as_ref()
        .map(|identity| identity.email.trim())
        .filter(|identity| !identity.is_empty())
        .filter(|identity| !sso_identities_match(&email, identity))
    {
        warn!(
            requested_email = %email,
            kiro_identity = %actual_identity,
            "Kiro identity name differs from the requested email; using the stable user ID"
        );
    }
    account.upstream_user_id = Some(upstream_user_id);
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

pub(super) fn sso_account_validation_error(
    email: &str,
    step: &'static str,
    error: &kproxy_kiro::KiroError,
) -> RpcError {
    let safe_error = kproxy_translate::sanitize_error_message(&error.to_string());
    warn!(
        requested_email = %email,
        validation_step = step,
        upstream_endpoint = %error.endpoint,
        upstream_status = ?error.status,
        error = %safe_error,
        "SSO login succeeded but Kiro account validation failed"
    );
    RpcError::internal(format!(
        "SSO login succeeded, but Kiro account validation failed during {step}; account was not saved: {safe_error}"
    ))
}

pub(super) fn authenticated_sso_user_id(
    limits: &kproxy_kiro::UsageLimits,
) -> Result<String, RpcError> {
    limits
        .user_info
        .as_ref()
        .map(|identity| identity.user_id.trim())
        .filter(|user_id| !user_id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            RpcError::internal(
                "Kiro account validation did not return a stable user ID; account was not saved",
            )
        })
}

pub(super) fn sso_identities_match(expected_email: &str, actual_identity: &str) -> bool {
    let expected_email = expected_email.trim();
    let actual_identity = actual_identity.trim();
    if expected_email.is_empty() || actual_identity.is_empty() {
        return false;
    }
    let expected = canonical_sso_identity(expected_email);
    !expected.is_empty()
        && actual_identity
            .split_whitespace()
            .any(|candidate| expected == canonical_sso_identity(candidate))
}

pub(super) fn canonical_sso_identity(value: &str) -> String {
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

pub(super) async fn handle_account_remove(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
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
    state.notifier().resolve_incident(
        kproxy_notify::WebhookEventKind::AccountQuotaExhausted,
        Some(&removed),
    );
    state.notifier().resolve_incident(
        kproxy_notify::WebhookEventKind::AccountCreditProtected,
        Some(&removed),
    );
    crate::alerts::resolve_token_refresh_failure(state, &removed);
    to_value(serde_json::json!({"removed": removed}))
}

pub(super) async fn handle_account_set_enabled(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
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
    if !enabled {
        crate::alerts::resolve_token_refresh_failure(state, &id);
    }
    to_value(serde_json::json!({"id": id, "enabled": enabled}))
}

pub(super) async fn handle_account_tag(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
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

pub(super) async fn handle_regenerate_machine_id(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
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

pub(super) async fn handle_account_refresh(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
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
            Ok(outcome) => {
                let usage_refreshed = crate::tasks::refresh_account_usage(state, &pool, &id)
                    .await
                    .unwrap_or(false);
                refreshed.push(serde_json::json!({
                    "id":id,"ok":true,"refreshed":outcome.changed,
                    "persisted":outcome.persisted(),
                    "persistence_error":outcome.persistence_error
                        .as_deref()
                        .map(kproxy_translate::sanitize_error_message),
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

pub(super) async fn handle_account_probe(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
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

pub(super) async fn probe_account(
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

pub(super) async fn probe_account_inner(
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

pub(super) async fn handle_account_reset_health(
    state: &Arc<AppState>,
    params: serde_json::Value,
) -> Handled {
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

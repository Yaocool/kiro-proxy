use super::{
    attempt_diagnostics, build_model_path, check_context_limit, dispatch_error,
    find_model_fallback, map_model, now_secs, push_model_path, resolve_static_model,
    retry_attempt_count, sanitize_error_message, set_payload_model, upstream_attempt_log, AppState,
    Arc, ExecuteError, HashSet, KiroError, PoolError, PreparedUpstream, Rng, UpstreamAttemptLog,
    UpstreamExecution,
};

enum DispatchOutcome {
    Prepared(Box<PreparedUpstream>),
    Generated(Box<UpstreamExecution>),
}

#[allow(clippy::too_many_arguments)]
pub(in crate::http::handlers) async fn prepare_upstream(
    state: &Arc<AppState>,
    trace_id: &str,
    model: &str,
    requested_model: &str,
    key_id: Option<&str>,
    default_model: &str,
    payload: &kproxy_translate::KiroPayload,
) -> Result<PreparedUpstream, ExecuteError> {
    match dispatch_upstream(
        state,
        trace_id,
        model,
        requested_model,
        key_id,
        default_model,
        0.0,
        0,
        false,
        payload,
        None,
        true,
    )
    .await?
    {
        DispatchOutcome::Prepared(prepared) => Ok(*prepared),
        DispatchOutcome::Generated(_) => unreachable!("preflight must not generate"),
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::http::handlers) async fn execute_upstream(
    state: &Arc<AppState>,
    trace_id: &str,
    model: &str,
    requested_model: &str,
    key_id: Option<&str>,
    default_model: &str,
    estimate: f64,
    input_tokens: u64,
    compact: bool,
    payload: &kproxy_translate::KiroPayload,
    prepared: Option<PreparedUpstream>,
) -> Result<UpstreamExecution, ExecuteError> {
    match dispatch_upstream(
        state,
        trace_id,
        model,
        requested_model,
        key_id,
        default_model,
        estimate,
        input_tokens,
        compact,
        payload,
        prepared,
        false,
    )
    .await?
    {
        DispatchOutcome::Generated(execution) => Ok(*execution),
        DispatchOutcome::Prepared(_) => unreachable!("generation must not stop at preflight"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_upstream(
    state: &Arc<AppState>,
    trace_id: &str,
    model: &str,
    requested_model: &str,
    key_id: Option<&str>,
    default_model: &str,
    estimate: f64,
    input_tokens: u64,
    compact: bool,
    payload: &kproxy_translate::KiroPayload,
    prepared: Option<PreparedUpstream>,
    preflight_only: bool,
) -> Result<DispatchOutcome, ExecuteError> {
    let config = state.config.current();
    let pool = state.pool();
    let account_count = pool.snapshot().await.len() as u32;
    let attempts = retry_attempt_count(config.upstream.max_retries, account_count);
    let mut last_error = None;
    // Reuse the prepared state without cloning the full conversation again or
    // drawing another weighted mapping choice. Preserve preflight exclusions
    // and attempt numbers when generation subsequently needs a retry.
    let (
        mut actual_model,
        mut mapped_model,
        mut request_payload,
        mut model_mapping_rule,
        mut model_path,
        mut attempt_logs,
        mut prepared_lease,
    ) = if let Some(selected) = prepared {
        (
            selected.kiro_model,
            selected.mapped_model,
            selected.payload,
            selected.model_mapping_rule,
            selected.model_path,
            selected.attempts,
            Some(selected.lease),
        )
    } else {
        (
            model.to_owned(),
            model.to_owned(),
            payload.clone(),
            map_model(requested_model, &config.model_mapping, key_id, None, "").rule,
            build_model_path(requested_model, model, ""),
            Vec::new(),
            None,
        )
    };
    let mut fallback_model = None::<String>;
    let mut attempted_accounts = attempt_logs
        .iter()
        .map(|attempt| attempt.account_id.clone())
        .collect::<HashSet<_>>();
    let preflight_attempts = attempt_logs.len() as u32;
    tracing::info!(
        event = "upstream.dispatch.started",
        trace_id,
        requested_model,
        initial_model = model,
        max_attempts = attempts,
        account_count,
        preflight_only,
        "upstream dispatch started"
    );
    for attempt in preflight_attempts..attempts {
        let lease = if let Some(lease) = prepared_lease.take() {
            lease
        } else {
            let lease = match pool
                .acquire_excluding(&actual_model, estimate, &attempted_accounts)
                .await
            {
                Ok(lease) => lease,
                Err(PoolError::NoAvailableAccount(_)) if last_error.is_some() => break,
                Err(PoolError::NoAvailableAccount(_))
                    if pool.all_matching_credit_exhausted(&actual_model).await =>
                {
                    crate::alerts::sync_service_quota(state).await;
                    return Err(ExecuteError::Pool(PoolError::CreditsExhausted));
                }
                Err(PoolError::NoAvailableAccount(_))
                    if !default_model.trim().is_empty() && actual_model != default_model =>
                {
                    actual_model = default_model.to_string();
                    set_payload_model(&mut request_payload, &actual_model);
                    match pool
                        .acquire_excluding(&actual_model, estimate, &attempted_accounts)
                        .await
                    {
                        Ok(lease) => lease,
                        Err(PoolError::NoAvailableAccount(_)) if last_error.is_some() => break,
                        Err(PoolError::NoAvailableAccount(_))
                            if pool.all_matching_credit_exhausted(&actual_model).await =>
                        {
                            crate::alerts::sync_service_quota(state).await;
                            return Err(ExecuteError::Pool(PoolError::CreditsExhausted));
                        }
                        Err(error) => return Err(ExecuteError::Pool(error)),
                    }
                }
                Err(PoolError::NoAvailableAccount(_)) => match pool
                    .acquire_excluding("", estimate, &attempted_accounts)
                    .await
                {
                    Ok(lease) => lease,
                    Err(PoolError::NoAvailableAccount(_)) if last_error.is_some() => break,
                    Err(PoolError::NoAvailableAccount(_))
                        if pool.all_matching_credit_exhausted("").await =>
                    {
                        crate::alerts::sync_service_quota(state).await;
                        return Err(ExecuteError::Pool(PoolError::CreditsExhausted));
                    }
                    Err(error) => return Err(ExecuteError::Pool(error)),
                },
                Err(error) => return Err(ExecuteError::Pool(error)),
            };
            let account = lease.account().await;
            let account_name = account.display_name().to_owned();
            tracing::debug!(
                event = "upstream.account.selected",
                trace_id,
                attempt = attempt + 1,
                max_attempts = attempts,
                account_id = %account.id,
                account_name,
                candidate_model = %actual_model,
                "upstream account selected"
            );
            let mut account_model_incompatible = false;
            let mut available_models = Vec::new();
            if let Some(runtime) = pool.get(&account.id).await {
                let remaining = account
                    .usage
                    .as_ref()
                    .filter(|usage| usage.limit > 0.0)
                    .map(|usage| {
                        ((usage.limit - usage.current) / usage.limit * 100.0).clamp(0.0, 100.0)
                    });
                if let Some(fallback) = fallback_model.clone() {
                    mapped_model = fallback;
                } else {
                    let route = map_model(
                        requested_model,
                        &config.model_mapping,
                        key_id,
                        remaining,
                        "",
                    );
                    mapped_model = route.mapped;
                    model_mapping_rule = route.rule;
                }
                model_path = build_model_path(requested_model, &mapped_model, "");
                actual_model.clone_from(&mapped_model);
                if let Some(resolved) = runtime.resolve_model(&actual_model).await {
                    actual_model = resolved;
                    push_model_path(&mut model_path, &actual_model);
                    set_payload_model(&mut request_payload, &actual_model);
                } else if runtime.has_model_cache().await {
                    if !default_model.trim().is_empty() {
                        if let Some(resolved) = runtime.resolve_model(default_model).await {
                            actual_model = resolved;
                            push_model_path(&mut model_path, default_model);
                            push_model_path(&mut model_path, &actual_model);
                            set_payload_model(&mut request_payload, &actual_model);
                        } else {
                            account_model_incompatible = true;
                            available_models = runtime.supported_models().await;
                        }
                    } else {
                        account_model_incompatible = true;
                        available_models = runtime.supported_models().await;
                    }
                } else {
                    if let Some(resolved) = resolve_static_model(&account, &actual_model) {
                        actual_model = resolved;
                        push_model_path(&mut model_path, &actual_model);
                    }
                    set_payload_model(&mut request_payload, &actual_model);
                }
            }
            if account_model_incompatible {
                let reason = if default_model.trim().is_empty() {
                    format!(
                    "model '{}' is not present in this account's model cache and no default model is configured",
                    actual_model
                )
                } else {
                    format!(
                    "model '{}' and default model '{}' are not present in this account's model cache",
                    actual_model, default_model
                )
                };
                attempt_logs.push(UpstreamAttemptLog {
                    attempt: attempt + 1,
                    account_id: account.id.clone(),
                    account_name: account_name.clone(),
                    model: actual_model.clone(),
                    available_models: available_models.clone(),
                    endpoint: "model-resolution".into(),
                    status: None,
                    error: reason.clone(),
                });
                tracing::warn!(
                    trace_id,
                    attempt = attempt + 1,
                    account_id = %account.id,
                    account_name,
                    model = %actual_model,
                    model_path = %model_path.join(" -> "),
                    available_models = %available_models.join(","),
                    reason,
                    "account cannot serve resolved model"
                );
                attempted_accounts.insert(account.id);
                drop(lease);
                continue;
            }
            lease
        };
        let account = lease.account().await;
        let account_name = account.display_name().to_owned();
        state.prepare_model_request(&mut request_payload);
        if preflight_only {
            return Ok(DispatchOutcome::Prepared(Box::new(PreparedUpstream {
                lease,
                mapped_model,
                kiro_model: actual_model,
                model_path,
                model_mapping_rule,
                attempts: attempt_logs,
                payload: request_payload,
            })));
        }
        if let Err(limit) = check_context_limit(state, input_tokens, compact, &actual_model) {
            return Err(ExecuteError::ContextLimit(limit));
        }
        request_payload.profile_arn.clone_from(&account.profile_arn);
        if account.is_token_expiring(
            now_secs(),
            state
                .config
                .current()
                .effective_token_refresh_before_expiry(),
        ) && state
            .refresh_account_token(&pool, &account.id, false)
            .await
            .is_ok_and(|outcome| outcome.changed)
        {
            tracing::info!(
                event = "upstream.token.refreshed",
                trace_id,
                account_id = %account.id,
                "expiring account token refreshed before upstream call"
            );
        }
        let account = lease.account().await;
        match state.generate(&account, &request_payload).await {
            Ok(response) => {
                tracing::info!(
                    event = "upstream.response.accepted",
                    trace_id,
                    attempt = attempt + 1,
                    account_id = %account.id,
                    account_name,
                    mapped_model = %mapped_model,
                    kiro_model = %actual_model,
                    model_path = %model_path.join(" -> "),
                    mapping_rule = model_mapping_rule.as_deref().unwrap_or("none"),
                    endpoint = %response.endpoint.name,
                    "upstream response accepted"
                );
                pool.record_success(&account.id).await;
                return Ok(DispatchOutcome::Generated(Box::new(UpstreamExecution {
                    lease,
                    response,
                    upstream_access_token: account.credentials.access_token.clone(),
                    mapped_model,
                    kiro_model: actual_model,
                    model_path,
                    model_mapping_rule,
                    attempts: attempt_logs,
                    payload: request_payload,
                })));
            }
            Err(error) if error.is_auth() => {
                attempt_logs.push(upstream_attempt_log(
                    attempt + 1,
                    &account,
                    &actual_model,
                    &error,
                ));
                tracing::warn!(
                    event = "upstream.request.rejected",
                    trace_id,
                    attempt = attempt + 1,
                    account_id = %account.id,
                    account_name,
                    endpoint = %error.endpoint,
                    upstream_status = error.status.unwrap_or_default(),
                    error = %sanitize_error_message(&error.message),
                    "upstream authentication failed; refreshing token"
                );
                let mut ban_account = true;
                if state
                    .refresh_account_token_after_auth_failure(
                        &pool,
                        &account.id,
                        &account.credentials.access_token,
                    )
                    .await
                    .is_ok()
                {
                    let refreshed = lease.account().await;
                    request_payload
                        .profile_arn
                        .clone_from(&refreshed.profile_arn);
                    match state.generate(&refreshed, &request_payload).await {
                        Ok(response) => {
                            let refreshed_name = refreshed.display_name().to_owned();
                            tracing::info!(
                                event = "upstream.authentication_retry.succeeded",
                                trace_id,
                                attempt = attempt + 1,
                                account_id = %refreshed.id,
                                account_name = refreshed_name,
                                endpoint = %response.endpoint.name,
                                "upstream authentication retry succeeded"
                            );
                            pool.record_success(&refreshed.id).await;
                            return Ok(DispatchOutcome::Generated(Box::new(UpstreamExecution {
                                lease,
                                upstream_access_token: refreshed.credentials.access_token.clone(),
                                mapped_model,
                                kiro_model: actual_model,
                                model_path,
                                model_mapping_rule,
                                attempts: attempt_logs,
                                payload: request_payload,
                                response,
                            })));
                        }
                        Err(retry_error) => {
                            attempt_logs.push(upstream_attempt_log(
                                attempt + 1,
                                &refreshed,
                                &actual_model,
                                &retry_error,
                            ));
                            tracing::warn!(
                                trace_id,
                                attempt = attempt + 1,
                                account_id = %account.id,
                                account_name,
                                endpoint = %retry_error.endpoint,
                                upstream_status = retry_error.status.unwrap_or_default(),
                                error = %sanitize_error_message(&retry_error.message),
                                "upstream authentication retry failed"
                            );
                            ban_account = retry_error.is_auth();
                            last_error = Some(retry_error);
                        }
                    }
                } else {
                    last_error = Some(error);
                }
                if ban_account {
                    pool.mark_banned(&account.id).await;
                    crate::alerts::emit_token_refresh_failure(
                        state,
                        &account.id,
                        &account_name,
                        "刷新后上游认证仍然失败",
                    );
                } else {
                    pool.record_error(&account.id).await;
                }
            }
            Err(error) if error.is_quota() && !error.is_throttle() => {
                attempt_logs.push(upstream_attempt_log(
                    attempt + 1,
                    &account,
                    &actual_model,
                    &error,
                ));
                tracing::warn!(
                    event = "upstream.request.retryable_failure",
                    trace_id,
                    attempt = attempt + 1,
                    account_id = %account.id,
                    account_name,
                    endpoint = %error.endpoint,
                    upstream_status = error.status.unwrap_or_default(),
                    error = %sanitize_error_message(&error.message),
                    "upstream quota error"
                );
                pool.record_quota_error(&account.id).await;
                if pool.get(&account.id).await.is_some_and(|runtime| {
                    runtime.health() == kproxy_pool::AccountHealth::Exhausted
                }) {
                    if let Err(persist_error) = crate::tasks::persist_pool_accounts(state).await {
                        tracing::error!(
                            trace_id,
                            %persist_error,
                            "failed to persist exhausted account state"
                        );
                    }
                    crate::alerts::sync_account_quota(state, &account.id).await;
                    crate::alerts::sync_service_quota(state).await;
                }
                if !config.pool.auto_switch_on_quota_exhausted {
                    return Err(dispatch_error(
                        error,
                        &mapped_model,
                        &actual_model,
                        &model_path,
                        model_mapping_rule,
                        attempt_logs,
                    ));
                }
                last_error = Some(error);
            }
            Err(error) if error.is_model_capacity_error() => {
                let is_throttle = error.is_throttle();
                let is_model_unavailable = error.is_model_temporarily_unavailable();
                attempt_logs.push(upstream_attempt_log(
                    attempt + 1,
                    &account,
                    &actual_model,
                    &error,
                ));
                tracing::warn!(
                    trace_id,
                    attempt = attempt + 1,
                    account_id = %account.id,
                    account_name,
                    endpoint = %error.endpoint,
                    upstream_status = error.status.unwrap_or_default(),
                    throttle_error = is_throttle,
                    model_unavailable = is_model_unavailable,
                    error = %sanitize_error_message(&error.message),
                    "upstream model capacity unavailable"
                );
                if !is_model_unavailable {
                    pool.record_error(&account.id).await;
                }
                last_error = Some(error);
                if config.features.enable_model_fallback && fallback_model.is_none() {
                    let (models, _) = state.models.get(config.models.cache_ttl_ms);
                    if let Some(fallback) = find_model_fallback(&actual_model, &models) {
                        let resolved = if let Some(runtime) = pool.get(&account.id).await {
                            let resolved = runtime.resolve_model(&fallback).await;
                            if resolved.is_none() && !runtime.has_model_cache().await {
                                Some(fallback.clone())
                            } else {
                                resolved
                            }
                        } else {
                            Some(fallback.clone())
                        };
                        if let Some(resolved) = resolved {
                            let fits_context =
                                check_context_limit(state, input_tokens, compact, &resolved)
                                    .is_ok();
                            if !fits_context {
                                tracing::warn!(
                                    trace_id,
                                    attempt = attempt + 1,
                                    account_id = %account.id,
                                    account_name,
                                    fallback_model = %resolved,
                                    input_tokens,
                                    "skipping model fallback because its context window is too small"
                                );
                            } else {
                                fallback_model = Some(fallback.clone());
                                mapped_model = fallback;
                                actual_model = resolved;
                                push_model_path(&mut model_path, &mapped_model);
                                push_model_path(&mut model_path, &actual_model);
                                set_payload_model(&mut request_payload, &actual_model);
                                state.prepare_model_request(&mut request_payload);
                                match state.generate(&account, &request_payload).await {
                                    Ok(response) => {
                                        tracing::info!(
                                            event = "upstream.model_fallback.succeeded",
                                            trace_id,
                                            attempt = attempt + 1,
                                            account_id = %account.id,
                                            account_name,
                                            fallback_model = %actual_model,
                                            model_path = %model_path.join(" -> "),
                                            endpoint = %response.endpoint.name,
                                            "upstream model fallback succeeded"
                                        );
                                        pool.record_success(&account.id).await;
                                        return Ok(DispatchOutcome::Generated(Box::new(
                                            UpstreamExecution {
                                                lease,
                                                upstream_access_token: account
                                                    .credentials
                                                    .access_token
                                                    .clone(),
                                                mapped_model,
                                                kiro_model: actual_model,
                                                model_path,
                                                model_mapping_rule,
                                                attempts: attempt_logs,
                                                payload: request_payload,
                                                response,
                                            },
                                        )));
                                    }
                                    Err(fallback_error) => {
                                        attempt_logs.push(upstream_attempt_log(
                                            attempt + 1,
                                            &account,
                                            &actual_model,
                                            &fallback_error,
                                        ));
                                        if fallback_error.is_request_rejection() {
                                            return Err(dispatch_error(
                                                fallback_error,
                                                &mapped_model,
                                                &actual_model,
                                                &model_path,
                                                model_mapping_rule,
                                                attempt_logs,
                                            ));
                                        }
                                        last_error = Some(fallback_error);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(error) if error.is_request_rejection() => {
                attempt_logs.push(upstream_attempt_log(
                    attempt + 1,
                    &account,
                    &actual_model,
                    &error,
                ));
                tracing::warn!(
                    trace_id,
                    attempt = attempt + 1,
                    account_id = %account.id,
                    account_name,
                    endpoint = %error.endpoint,
                    upstream_status = error.status.unwrap_or_default(),
                    error = %sanitize_error_message(&error.message),
                    "upstream rejected request payload; account health unchanged"
                );
                return Err(dispatch_error(
                    error,
                    &mapped_model,
                    &actual_model,
                    &model_path,
                    model_mapping_rule,
                    attempt_logs,
                ));
            }
            Err(error) if error.is_retriable() => {
                attempt_logs.push(upstream_attempt_log(
                    attempt + 1,
                    &account,
                    &actual_model,
                    &error,
                ));
                tracing::warn!(
                    trace_id,
                    attempt = attempt + 1,
                    account_id = %account.id,
                    account_name,
                    endpoint = %error.endpoint,
                    upstream_status = error.status.unwrap_or_default(),
                    error = %sanitize_error_message(&error.message),
                    "retriable upstream request failed"
                );
                pool.record_error(&account.id).await;
                last_error = Some(error);
            }
            Err(error) => {
                attempt_logs.push(upstream_attempt_log(
                    attempt + 1,
                    &account,
                    &actual_model,
                    &error,
                ));
                tracing::error!(
                    event = "upstream.request.failed",
                    trace_id,
                    attempt = attempt + 1,
                    account_id = %account.id,
                    account_name,
                    endpoint = %error.endpoint,
                    upstream_status = error.status.unwrap_or_default(),
                    error = %sanitize_error_message(&error.message),
                    "non-retriable upstream request failed"
                );
                return Err(dispatch_error(
                    error,
                    &mapped_model,
                    &actual_model,
                    &model_path,
                    model_mapping_rule,
                    attempt_logs,
                ));
            }
        }
        attempted_accounts.insert(account.id);
        drop(lease);
        if attempt + 1 < attempts {
            let exponent = attempt.min(5);
            let base_ms = 200u64.saturating_mul(1u64 << exponent).min(5_000);
            let jitter_ms = rand::thread_rng().gen_range(0..=base_ms / 4);
            let backoff_ms = base_ms + jitter_ms;
            tracing::debug!(
                event = "upstream.retry.waiting",
                trace_id,
                attempt = attempt + 1,
                backoff_ms,
                "waiting before upstream retry"
            );
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        }
    }
    let model_resolution_failed = last_error.is_none()
        && !attempt_logs.is_empty()
        && attempt_logs
            .iter()
            .all(|attempt| attempt.endpoint == "model-resolution");
    let attempt_diagnostics = attempt_diagnostics(&attempt_logs);
    let error = last_error.unwrap_or_else(|| {
        KiroError {
            status: None,
            endpoint: if model_resolution_failed {
                "model-resolution"
            } else {
                "none"
            }
            .into(),
            message: if model_resolution_failed {
                format!(
                    "no selected account can serve resolved model '{actual_model}' ({} distinct models are available); inspect routing with 'kproxy models resolve <model-id>'",
                    attempt_diagnostics.available_model_count
                )
            } else {
                "all upstream attempts failed".into()
            },
        }
    });
    if model_resolution_failed {
        tracing::warn!(
            event = "upstream.model_resolution.exhausted",
            trace_id,
            failure_kind = "model_not_available",
            error_stage = "model_resolution",
            endpoint = %error.endpoint,
            upstream_status = error.status.unwrap_or_default(),
            error = %sanitize_error_message(&error.message),
            requested_model,
            default_model,
            max_attempts = attempts,
            attempt_count = attempt_logs.len(),
            attempted_accounts = attempted_accounts.len(),
            attempted_account_ids = %attempt_diagnostics.account_ids,
            attempted_account_names = %attempt_diagnostics.account_names,
            available_model_count = attempt_diagnostics.available_model_count,
            available_models = %attempt_diagnostics.available_models,
            attempt_errors = %attempt_diagnostics.errors,
            mapped_model,
            kiro_model = actual_model,
            model_path = %model_path.join(" -> "),
            mapping_rule = model_mapping_rule.as_deref().unwrap_or("none"),
            "model resolution exhausted selected accounts"
        );
    } else {
        tracing::warn!(
            event = "upstream.dispatch.exhausted",
            trace_id,
            failure_kind = "upstream_attempts_exhausted",
            error_stage = "upstream_dispatch",
            endpoint = %error.endpoint,
            upstream_status = error.status.unwrap_or_default(),
            error = %sanitize_error_message(&error.message),
            requested_model,
            default_model,
            max_attempts = attempts,
            attempt_count = attempt_logs.len(),
            attempted_accounts = attempted_accounts.len(),
            attempted_account_ids = %attempt_diagnostics.account_ids,
            attempted_account_names = %attempt_diagnostics.account_names,
            available_model_count = attempt_diagnostics.available_model_count,
            available_models = %attempt_diagnostics.available_models,
            attempt_errors = %attempt_diagnostics.errors,
            mapped_model,
            kiro_model = actual_model,
            model_path = %model_path.join(" -> "),
            mapping_rule = model_mapping_rule.as_deref().unwrap_or("none"),
            "all upstream attempts failed"
        );
    }
    Err(dispatch_error(
        error,
        &mapped_model,
        &actual_model,
        &model_path,
        model_mapping_rule,
        attempt_logs,
    ))
}

use super::{
    handle_claude, handle_openai, json, now_secs, request_trace_id, sanitize_error_message,
    ApiError, AppState, Arc, BTreeSet, Body, Bytes, Duration, ErrorFormat, HeaderMap, Instant,
    IntoResponse, Json, Request, RequestDiagnostics, RequestLog, Response, ServiceHttpState, State,
    StatusCode, StreamExt, UpstreamAttemptLog, Uuid, Value, MAX_ATTEMPT_LOG_SUMMARY_CHARS,
    MAX_STATS_MODEL_CHARS, UNKNOWN_STATS_MODEL,
};

pub async fn root() -> Json<Value> {
    Json(json!({"name":"kiro-proxy","status":"ok","version":env!("CARGO_PKG_VERSION")}))
}

pub async fn health(State(service): State<ServiceHttpState>) -> Json<Value> {
    let pool = service.app.pool();
    let counts = pool.scheduling_counts().await;
    let accounts = pool.snapshot().await;
    let (used_credits, total_credits) = accounts
        .iter()
        .filter_map(|account| account.usage.as_ref())
        .fold((0.0, 0.0), |(used, total), usage| {
            (used + usage.current, total + usage.limit)
        });
    Json(json!({
        "status":"ok",
        "service_id":service.service.id,
        "service_name":service.service.name,
        "total_accounts":accounts.len(),
        "available_accounts":counts.available,
        "protected_accounts":counts.protected,
        "cooling_accounts":counts.cooling,
        "exhausted_accounts":counts.exhausted,
        "banned_accounts":counts.banned,
        "refreshing_accounts":counts.refreshing,
        "disabled_accounts":counts.disabled,
        "used_credits":used_credits,
        "total_credits":total_credits,
        "uptime_secs":service.app.uptime_secs()
    }))
}

pub async fn readiness(State(service): State<ServiceHttpState>) -> Response {
    let counts = service.app.pool().scheduling_counts().await;
    let mut reasons = service.app.task_registry.readiness_issues(&service.app);
    if counts.available == 0 {
        reasons.push("no account is currently available".to_string());
    }
    if let Some(error) = service.app.meter.recovery_error() {
        reasons.push(format!("metering recovery required: {error}"));
    }
    let ready = reasons.is_empty();
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "status":if ready { "ready" } else { "not_ready" },
            "ready":ready,
            "reasons":reasons,
            "service_id":service.service.id,
            "service_name":service.service.name,
            "available_accounts":counts.available,
            "uptime_secs":service.app.uptime_secs()
        })),
    )
        .into_response()
}

pub async fn claude_messages(
    State(service): State<ServiceHttpState>,
    request: Request,
) -> Response {
    let state = Arc::clone(&service.app);
    let path = request.uri().path().to_string();
    let trace_id = request_trace_id(&request);
    let started = Instant::now();
    let connection_guard = match state.connections.try_acquire() {
        Some(guard) => guard,
        None => {
            let error = ApiError::overloaded(ErrorFormat::Claude);
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let admission_guard = match state.admission.try_acquire() {
        Some(guard) => guard,
        None => {
            let error = ApiError::overloaded(ErrorFormat::Claude);
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let (headers, body, _body_reservations) =
        match read_bounded_body(&state, request, ErrorFormat::Claude).await {
            Ok(body) => body,
            Err(error) => {
                record_failed_request(&state, &trace_id, &path, "", started, &error);
                return error.with_request_id(&trace_id).into_response();
            }
        };
    let model = request_model_hint(&body);
    tracing::debug!(
        trace_id = %trace_id,
        protocol = "claude",
        body_bytes = body.len(),
        model = %model,
        "client request body read"
    );
    match handle_claude(
        service,
        trace_id.clone(),
        path.clone(),
        headers,
        body,
        connection_guard,
        admission_guard,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            record_failed_request(&state, &trace_id, &path, &model, started, &error);
            error.with_request_id(&trace_id).into_response()
        }
    }
}

pub async fn openai_chat(State(service): State<ServiceHttpState>, request: Request) -> Response {
    let state = Arc::clone(&service.app);
    let path = request.uri().path().to_string();
    let trace_id = request_trace_id(&request);
    let started = Instant::now();
    let connection_guard = match state.connections.try_acquire() {
        Some(guard) => guard,
        None => {
            let error = ApiError::overloaded(ErrorFormat::OpenAi);
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let admission_guard = match state.admission.try_acquire() {
        Some(guard) => guard,
        None => {
            let error = ApiError::overloaded(ErrorFormat::OpenAi);
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let (headers, body, _body_reservations) =
        match read_bounded_body(&state, request, ErrorFormat::OpenAi).await {
            Ok(body) => body,
            Err(error) => {
                record_failed_request(&state, &trace_id, &path, "", started, &error);
                return error.with_request_id(&trace_id).into_response();
            }
        };
    let model = request_model_hint(&body);
    tracing::debug!(
        trace_id = %trace_id,
        protocol = "openai",
        body_bytes = body.len(),
        model = %model,
        "client request body read"
    );
    match handle_openai(
        service,
        trace_id.clone(),
        path.clone(),
        headers,
        body,
        connection_guard,
        admission_guard,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            record_failed_request(&state, &trace_id, &path, &model, started, &error);
            error.with_request_id(&trace_id).into_response()
        }
    }
}

pub(super) async fn read_bounded_body(
    state: &Arc<AppState>,
    request: Request,
    format: ErrorFormat,
) -> Result<(HeaderMap, Bytes, crate::state::BodyGuard), ApiError> {
    const MAX_BODY_BYTES: usize = 50 * 1024 * 1024;
    let (parts, body) = request.into_parts();
    let mut stream = Body::into_data_stream(body);
    let mut bytes = bytes::BytesMut::new();
    let mut reservation = state
        .body_budget
        .reserve(0)
        .ok_or_else(|| ApiError::overloaded(format))?;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(15), stream.next())
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                "request body read timed out",
                format,
            )
        })?
    {
        let chunk = chunk.map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("failed to read request body: {error}"),
                format,
            )
        })?;
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds the 50 MiB limit",
                format,
            ));
        }
        if !reservation.reserve_more(chunk.len()) {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "request body memory budget exceeded",
                format,
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((parts.headers, bytes.freeze(), reservation))
}

fn request_model_hint(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(|model| model.chars().take(MAX_STATS_MODEL_CHARS).collect())
        })
        .unwrap_or_default()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct AttemptDiagnostics {
    pub(super) account_ids: String,
    pub(super) account_names: String,
    pub(super) available_models: String,
    pub(super) available_model_count: usize,
    pub(super) errors: String,
}

pub(super) fn attempt_diagnostics(attempts: &[UpstreamAttemptLog]) -> AttemptDiagnostics {
    let account_ids = attempts
        .iter()
        .filter_map(|attempt| {
            (!attempt.account_id.is_empty()).then_some(attempt.account_id.as_str())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",");
    let account_names = attempts
        .iter()
        .filter_map(|attempt| {
            (!attempt.account_name.is_empty()).then_some(attempt.account_name.as_str())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",");
    let available_models = attempts
        .iter()
        .flat_map(|attempt| attempt.available_models.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    let available_model_count = available_models.len();
    let available_models = available_models.into_iter().collect::<Vec<_>>().join(",");
    let errors = attempts
        .iter()
        .map(|attempt| {
            format!(
                "attempt={} account={} endpoint={} status={} error={}",
                attempt.attempt,
                attempt.account_id,
                attempt.endpoint,
                attempt
                    .status
                    .map_or_else(|| "-".into(), |status| status.to_string()),
                attempt.error
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    AttemptDiagnostics {
        account_ids: bounded_log_summary(account_ids),
        account_names: bounded_log_summary(account_names),
        available_models: bounded_log_summary(available_models),
        available_model_count,
        errors: bounded_log_summary(errors),
    }
}

fn bounded_log_summary(value: String) -> String {
    if value.chars().count() <= MAX_ATTEMPT_LOG_SUMMARY_CHARS {
        return value;
    }
    let mut output = value
        .chars()
        .take(MAX_ATTEMPT_LOG_SUMMARY_CHARS)
        .collect::<String>();
    output.push('…');
    output
}

pub(super) fn record_failed_request(
    state: &Arc<AppState>,
    trace_id: &str,
    path: &str,
    model: &str,
    started: Instant,
    error: &ApiError,
) {
    let model = if error.suppress_model_stats {
        UNKNOWN_STATS_MODEL.to_owned()
    } else {
        model.chars().take(MAX_STATS_MODEL_CHARS).collect()
    };
    let safe_error = sanitize_error_message(&error.message);
    let duration_ms = started.elapsed().as_millis() as u64;
    let model_path = error.log_context.model_path.join(" -> ");
    let request_id = format!("req_{}", Uuid::new_v4().simple());
    let attempts = attempt_diagnostics(&error.log_context.attempts);
    let upstream_status = error.upstream_status.or_else(|| {
        error
            .log_context
            .attempts
            .iter()
            .rev()
            .find_map(|attempt| attempt.status)
    });
    if error.status.is_server_error() {
        tracing::error!(
            event = "proxy.request.failed",
            trace_id,
            request_id,
            http_path = path,
            model = %model,
            mapped_model = %error.log_context.mapped_model,
            kiro_model = %error.log_context.kiro_model,
            model_path,
            mapping_rule = error.log_context.model_mapping_rule.as_deref().unwrap_or("none"),
            account_id = %error.log_context.account_id,
            account_name = %error.log_context.account_name,
            endpoint = %error.log_context.endpoint,
            upstream_attempts = error.log_context.attempts.len(),
            attempted_account_ids = %attempts.account_ids,
            attempted_account_names = %attempts.account_names,
            available_model_count = attempts.available_model_count,
            available_models = %attempts.available_models,
            attempt_errors = %attempts.errors,
            http_status = error.status.as_u16(),
            upstream_status = upstream_status.unwrap_or_default(),
            error_code = error.error_code,
            error_stage = error.error_stage,
            account_error = error.account_error,
            duration_ms,
            error = %safe_error,
            "client request failed"
        );
    } else {
        tracing::warn!(
            event = "proxy.request.rejected",
            trace_id,
            request_id,
            http_path = path,
            model = %model,
            mapped_model = %error.log_context.mapped_model,
            kiro_model = %error.log_context.kiro_model,
            model_path,
            mapping_rule = error.log_context.model_mapping_rule.as_deref().unwrap_or("none"),
            account_id = %error.log_context.account_id,
            account_name = %error.log_context.account_name,
            endpoint = %error.log_context.endpoint,
            upstream_attempts = error.log_context.attempts.len(),
            attempted_account_ids = %attempts.account_ids,
            attempted_account_names = %attempts.account_names,
            available_model_count = attempts.available_model_count,
            available_models = %attempts.available_models,
            attempt_errors = %attempts.errors,
            http_status = error.status.as_u16(),
            upstream_status = upstream_status.unwrap_or_default(),
            error_code = error.error_code,
            error_stage = error.error_stage,
            account_error = error.account_error,
            duration_ms,
            error = %safe_error,
            "client request rejected"
        );
    }
    state.stats.record(RequestLog {
        timestamp: now_secs(),
        trace_id: trace_id.into(),
        request_id,
        path: path.into(),
        model: if error.log_context.mapped_model.is_empty() {
            model.clone()
        } else {
            error.log_context.mapped_model.clone()
        },
        original_model: model,
        kiro_model: error.log_context.kiro_model.clone(),
        account_id: error.log_context.account_id.clone(),
        account_name: error.log_context.account_name.clone(),
        endpoint: error.log_context.endpoint.clone(),
        model_path: error.log_context.model_path.clone(),
        model_mapping_rule: error.log_context.model_mapping_rule.clone(),
        attempts: error.log_context.attempts.clone(),
        duration_ms,
        status: error.status.as_u16(),
        input_tokens: 0,
        output_tokens: 0,
        credits: 0.0,
        error: Some(safe_error),
        diagnostics: RequestDiagnostics {
            client_status: error.status.as_u16(),
            upstream_status,
            error_code: error.error_code.to_owned(),
            error_stage: error.error_stage.to_owned(),
            account_error: error.account_error,
            ..RequestDiagnostics::default()
        },
    });
}

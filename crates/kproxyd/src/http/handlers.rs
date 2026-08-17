use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use futures::StreamExt;
use kproxy_kiro::{KiroError, KiroResponse};
use kproxy_notify::{WebhookEvent, WebhookEventKind};
use kproxy_pool::{AccountLease, PoolError};
use kproxy_translate::model::{apply_adaptive_thinking, map_model, thinking_enabled_for_model};
use kproxy_translate::{
    apply_compaction_boundary, claude_to_kiro, compact_trigger_tokens, error_envelope,
    openai_to_kiro, sanitize_error_message, validate_claude, validate_openai, ClaudeRequest,
    ErrorFormat, OpenAiRequest, TranslationOptions,
};
use rand::Rng;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

use crate::meter::{now_secs, CreditReservation, MeterError, UsageRecord};
use crate::state::AppState;
use crate::stats::{RequestLog, UpstreamAttemptLog};

use super::prompt_cache::PromptCacheProfile;
use super::request_trace_id;
use super::response::{DecodedResponse, OpenAiToolIdentity, ToolLeakFilter};
use super::stream::{self, StreamContext, StreamProtocol};
use super::usage::{fallback_credits, fill_missing_usage};
use super::ServiceHttpState;

const MAX_STATS_MODEL_CHARS: usize = 128;
const UNKNOWN_STATS_MODEL: &str = "unknown";

pub async fn root() -> Json<Value> {
    Json(json!({"name":"kiro-proxy","status":"ok","version":env!("CARGO_PKG_VERSION")}))
}

pub async fn health(State(service): State<ServiceHttpState>) -> Json<Value> {
    let counts = service.app.pool().health_counts().await;
    Json(json!({
        "status":"ok",
        "service_id":service.service.id,
        "service_name":service.service.name,
        "available_accounts":counts[0],"cooling_accounts":counts[1],
        "exhausted_accounts":counts[2],"banned_accounts":counts[3],
        "uptime_secs":service.app.uptime_secs()
    }))
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

async fn read_bounded_body(
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
        .expect("zero-byte body reservation must succeed");
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

fn record_failed_request(
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
    if error.status.is_server_error() {
        tracing::error!(
            trace_id,
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
            http_status = error.status.as_u16(),
            duration_ms,
            error = %safe_error,
            "client request failed"
        );
    } else {
        tracing::warn!(
            trace_id,
            http_path = path,
            model = %model,
            mapped_model = %error.log_context.mapped_model,
            kiro_model = %error.log_context.kiro_model,
            model_path,
            mapping_rule = error.log_context.model_mapping_rule.as_deref().unwrap_or("none"),
            http_status = error.status.as_u16(),
            duration_ms,
            error = %safe_error,
            "client request rejected"
        );
    }
    state.stats.record(RequestLog {
        timestamp: now_secs(),
        trace_id: trace_id.into(),
        request_id: format!("req_{}", Uuid::new_v4().simple()),
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
    });
}

async fn handle_claude(
    service: ServiceHttpState,
    trace_id: String,
    path: String,
    headers: HeaderMap,
    body: Bytes,
    connection_guard: crate::state::AdmissionGuard,
    admission_guard: crate::state::AdmissionGuard,
) -> Result<Response, ApiError> {
    let state = Arc::clone(&service.app);
    let started = Instant::now();
    enforce_claude_user_agent(&state, &headers)?;
    let key_id = authenticate(
        &state,
        &service.allowed_api_key_ids,
        &headers,
        ErrorFormat::Claude,
    )?;
    tracing::debug!(
        trace_id = %trace_id,
        protocol = "claude",
        api_key_id = key_id.as_deref().unwrap_or("anonymous"),
        "client authentication completed"
    );
    let mut request: ClaudeRequest = serde_json::from_slice(&body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "Invalid JSON in request body",
            ErrorFormat::Claude,
        )
    })?;
    validate_claude(&request).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            error.to_string(),
            ErrorFormat::Claude,
        )
    })?;
    tracing::info!(
        trace_id = %trace_id,
        protocol = "claude",
        model = %request.model,
        streaming = request.stream,
        message_count = request.messages.len(),
        tool_count = request.tools.len(),
        max_tokens = request.max_tokens,
        "client request validated"
    );
    let config = state.config.current();
    if config.features.disable_tools {
        request.tools.clear();
        request.tool_choice = None;
    } else if !config.features.enable_web_tools {
        request.tools.retain(|tool| {
            !tool
                .r#type
                .as_deref()
                .is_some_and(|kind| kind.starts_with("web_search") || kind.starts_with("web_fetch"))
        });
    }
    let compact_trigger = compact_trigger_tokens(request.context_management.as_ref());
    let compact_boundary_applied = apply_compaction_boundary(&mut request);
    let web_tool_names = claude_web_tool_names(&request);
    let route = map_model(
        &request.model,
        &config.model_mapping,
        key_id.as_deref(),
        None,
        "",
    );
    let mut options = TranslationOptions::new(route.mapped.clone(), "AI_EDITOR");
    options.enhance_system_prompt = config.features.enhance_system_prompt;
    options.compact_mode = compact_trigger.is_some();
    let mut payload = claude_to_kiro(&request, &options);
    let thinking_limit = model_token_limit(&state, &route.mapped, false)
        .unwrap_or(config.features.max_thinking_budget_tokens)
        .min(config.features.max_thinking_budget_tokens);
    let decision = apply_adaptive_thinking(
        &mut payload,
        request.thinking.as_ref(),
        thinking_enabled_for_model(&request.model, &config.model_thinking_mode),
        config.features.adaptive_thinking,
        thinking_limit,
    );
    tracing::debug!(
        trace_id = %trace_id,
        enabled = decision.enabled,
        reason = ?decision.reason,
        budget_tokens = decision.budget_tokens,
        "adaptive thinking decision"
    );
    let mut input_tokens = state
        .tokenizer
        .estimate_kiro_payload(&payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64;
    let mut compacted = compact_boundary_applied;
    let mut compaction_summary = None;
    let effective_compact_trigger =
        compact_trigger.map(|trigger| trigger.min(context_maximum(&state, false, &route.mapped)));
    if let Some(trigger) = effective_compact_trigger.filter(|trigger| input_tokens >= *trigger) {
        let target = compact_target_tokens(&state, &route.mapped, trigger);
        let stats = state
            .tokenizer
            .compact_kiro_payload(&mut payload, target as usize)
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error,
                    ErrorFormat::Claude,
                )
            })?;
        input_tokens = stats.compacted_tokens as u64;
        compacted |= stats.removed_messages > 0;
        compaction_summary = stats
            .summary
            .map(|summary| append_current_request_to_compaction(summary, &request));
        tracing::info!(
            trace_id = %trace_id,
            original_input_tokens = stats.original_tokens,
            compacted_input_tokens = stats.compacted_tokens,
            removed_messages = stats.removed_messages,
            target_tokens = target,
            "request context compacted"
        );
    }
    let prompt_cache = config
        .features
        .enable_prompt_cache
        .then(|| state.prompt_cache.claude_profile(&request, input_tokens))
        .flatten();
    enforce_context(
        &state,
        input_tokens,
        compacted,
        &route.mapped,
        ErrorFormat::Claude,
    )?;
    let estimate = estimated_credits(input_tokens, request.max_tokens, &config.pool);
    let reservation = reserve_credits(&state, key_id.as_deref(), estimate, ErrorFormat::Claude)?;
    tracing::info!(
        trace_id = %trace_id,
        protocol = "claude",
        requested_model = %request.model,
        mapped_model = %route.mapped,
        input_tokens,
        estimated_credits = estimate,
        "upstream request prepared"
    );
    drop(body);
    let request_id = format!("msg_{}", Uuid::new_v4().simple());
    let UpstreamExecution {
        lease,
        response: upstream,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        payload,
    } = execute_upstream(
        &state,
        &trace_id,
        &route.mapped,
        &request.model,
        key_id.as_deref(),
        &config.features.default_model_id,
        estimate,
        input_tokens,
        compacted,
        &payload,
    )
    .await
    .map_err(|error| upstream_error(error, ErrorFormat::Claude))?;
    if request.stream {
        return Ok(stream::response(
            upstream,
            StreamProtocol::Claude,
            StreamContext {
                state,
                lease,
                reservation,
                trace_id,
                request_id,
                path,
                model: request.model.clone(),
                mapped_model,
                original_model: request.model,
                api_key_id: key_id.clone(),
                kiro_model,
                model_path,
                model_mapping_rule,
                attempts,
                input_tokens,
                compact: compacted,
                compaction_summary,
                estimated_credits: estimate,
                max_tokens: request.max_tokens,
                started,
                prompt_cache,
                payload,
                auto_continue_rounds: config.features.auto_continue_rounds.min(30),
                buffer_tool_calls: config.features.buffer_tool_calls,
                tool_call_buffer_delay_ms: config.features.tool_call_buffer_delay_ms,
                enable_tool_leak_filter: config.features.enable_tool_leak_filter,
                thinking_output_format: config.features.thinking_output_format,
                include_usage_chunk: false,
                web_tool_names,
                openai_tools: std::collections::HashMap::new(),
                _connection_guard: connection_guard,
                _admission_guard: admission_guard,
            },
        ));
    }
    nonstream_claude(
        state,
        lease,
        reservation,
        upstream,
        payload,
        trace_id,
        request_id,
        path,
        request,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        input_tokens,
        estimate,
        started,
        prompt_cache,
        compaction_summary,
    )
    .await
}

async fn handle_openai(
    service: ServiceHttpState,
    trace_id: String,
    path: String,
    headers: HeaderMap,
    body: Bytes,
    connection_guard: crate::state::AdmissionGuard,
    admission_guard: crate::state::AdmissionGuard,
) -> Result<Response, ApiError> {
    let state = Arc::clone(&service.app);
    let started = Instant::now();
    let key_id = authenticate(
        &state,
        &service.allowed_api_key_ids,
        &headers,
        ErrorFormat::OpenAi,
    )?;
    tracing::debug!(
        trace_id = %trace_id,
        protocol = "openai",
        api_key_id = key_id.as_deref().unwrap_or("anonymous"),
        "client authentication completed"
    );
    let mut request: OpenAiRequest = serde_json::from_slice(&body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "Invalid JSON in request body",
            ErrorFormat::OpenAi,
        )
    })?;
    validate_openai(&request).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            error.to_string(),
            ErrorFormat::OpenAi,
        )
    })?;
    tracing::info!(
        trace_id = %trace_id,
        protocol = "openai",
        model = %request.model,
        streaming = request.stream,
        message_count = request.messages.len(),
        tool_count = request.tools.len(),
        "client request validated"
    );
    let config = state.config.current();
    if config.features.disable_tools {
        request.tools.clear();
        request.tool_choice = None;
    } else if !config.features.enable_web_tools {
        request.tools.retain(|tool| tool.r#type != "web_search");
    }
    let openai_tools = openai_tool_identities(&request);
    let image_guards = hydrate_openai_images(&state, &mut request).await?;
    let max_tokens = request
        .max_completion_tokens
        .or(request.max_tokens)
        .unwrap_or(8192);
    let route = map_model(
        &request.model,
        &config.model_mapping,
        key_id.as_deref(),
        None,
        "",
    );
    let mut options = TranslationOptions::new(route.mapped.clone(), "AI_EDITOR");
    options.enhance_system_prompt = config.features.enhance_system_prompt;
    let mut payload = openai_to_kiro(&request, &options);
    let thinking_limit = model_token_limit(&state, &route.mapped, false)
        .unwrap_or(config.features.max_thinking_budget_tokens)
        .min(config.features.max_thinking_budget_tokens);
    let decision = apply_adaptive_thinking(
        &mut payload,
        request.thinking.as_ref(),
        thinking_enabled_for_model(&request.model, &config.model_thinking_mode),
        config.features.adaptive_thinking,
        thinking_limit,
    );
    tracing::debug!(
        trace_id = %trace_id,
        enabled = decision.enabled,
        reason = ?decision.reason,
        budget_tokens = decision.budget_tokens,
        "adaptive thinking decision"
    );
    let input_tokens = state
        .tokenizer
        .estimate_kiro_payload(&payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::OpenAi,
            )
        })? as u64;
    let prompt_cache = config
        .features
        .enable_prompt_cache
        .then(|| state.prompt_cache.openai_profile(&request, input_tokens))
        .flatten();
    enforce_context(
        &state,
        input_tokens,
        false,
        &route.mapped,
        ErrorFormat::OpenAi,
    )?;
    let estimate = estimated_credits(input_tokens, max_tokens, &config.pool);
    let reservation = reserve_credits(&state, key_id.as_deref(), estimate, ErrorFormat::OpenAi)?;
    tracing::info!(
        trace_id = %trace_id,
        protocol = "openai",
        requested_model = %request.model,
        mapped_model = %route.mapped,
        input_tokens,
        max_tokens,
        estimated_credits = estimate,
        "upstream request prepared"
    );
    drop(body);
    drop(image_guards);
    let include_usage_chunk = request
        .stream_options
        .as_ref()
        .and_then(|options| options.get("include_usage"))
        .and_then(Value::as_bool)
        == Some(true);
    let request_id = format!("chatcmpl-{}", Uuid::new_v4().simple());
    let UpstreamExecution {
        lease,
        response: upstream,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        payload,
    } = execute_upstream(
        &state,
        &trace_id,
        &route.mapped,
        &request.model,
        key_id.as_deref(),
        &config.features.default_model_id,
        estimate,
        input_tokens,
        false,
        &payload,
    )
    .await
    .map_err(|error| upstream_error(error, ErrorFormat::OpenAi))?;
    if request.stream {
        return Ok(stream::response(
            upstream,
            StreamProtocol::OpenAi,
            StreamContext {
                state,
                lease,
                reservation,
                trace_id,
                request_id,
                path,
                model: request.model.clone(),
                mapped_model,
                original_model: request.model,
                api_key_id: key_id.clone(),
                kiro_model,
                model_path,
                model_mapping_rule,
                attempts,
                input_tokens,
                compact: false,
                compaction_summary: None,
                estimated_credits: estimate,
                max_tokens,
                started,
                prompt_cache,
                payload,
                auto_continue_rounds: config.features.auto_continue_rounds.min(30),
                buffer_tool_calls: config.features.buffer_tool_calls,
                tool_call_buffer_delay_ms: config.features.tool_call_buffer_delay_ms,
                enable_tool_leak_filter: config.features.enable_tool_leak_filter,
                thinking_output_format: config.features.thinking_output_format,
                include_usage_chunk,
                web_tool_names: std::collections::HashMap::new(),
                openai_tools,
                _connection_guard: connection_guard,
                _admission_guard: admission_guard,
            },
        ));
    }
    nonstream_openai(
        state,
        lease,
        reservation,
        upstream,
        payload,
        trace_id,
        request_id,
        path,
        request,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        input_tokens,
        max_tokens,
        estimate,
        started,
        prompt_cache,
        openai_tools,
    )
    .await
}

async fn hydrate_openai_images(
    state: &Arc<AppState>,
    request: &mut OpenAiRequest,
) -> Result<Vec<crate::state::BodyGuard>, ApiError> {
    let mut guards = Vec::new();
    for message in &mut request.messages {
        let Some(parts) = message.content.as_mut().and_then(Value::as_array_mut) else {
            continue;
        };
        let mut output = Vec::with_capacity(parts.len());
        for mut part in parts.drain(..) {
            let remote = part
                .pointer("/image_url/url")
                .and_then(Value::as_str)
                .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
                .map(str::to_string);
            let Some(url) = remote else {
                output.push(part);
                continue;
            };
            let guard = state.body_budget.reserve(10 * 1024 * 1024).ok_or_else(|| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "remote image memory budget exceeded",
                    ErrorFormat::OpenAi,
                )
            })?;
            let data_url = fetch_image_as_data_url(&url).await.ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "unable to fetch remote image",
                    ErrorFormat::OpenAi,
                )
            })?;
            if let Some(value) = part.pointer_mut("/image_url/url") {
                *value = Value::String(data_url);
            }
            output.push(part);
            guards.push(guard);
        }
        *parts = output;
    }
    Ok(guards)
}

async fn fetch_image_as_data_url(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .ok()?
        .collect::<Vec<_>>();
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|address| !is_public_address(address.ip()))
    {
        return None;
    }
    // Pin the validated DNS answers and prohibit redirects so a resolver race or
    // redirect cannot turn this image helper into an internal-network proxy.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addresses)
        .build()
        .ok()?;
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)?
        .to_str()
        .ok()?
        .split(';')
        .next()?
        .trim()
        .to_ascii_lowercase();
    let format = match content_type.as_str() {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return None,
    };
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if bytes.len().saturating_add(chunk.len()) > 10 * 1024 * 1024 {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:image/{format};base64,{encoded}"))
}

fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            !address.is_private()
                && !address.is_loopback()
                && !address.is_link_local()
                && !address.is_unspecified()
                && !address.is_broadcast()
                && !address.is_multicast()
                && !address.is_documentation()
                && !address.octets()[0].eq(&0)
                && !(address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1]))
                && !(address.octets()[0] == 198 && (18..=19).contains(&address.octets()[1]))
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return is_public_address(IpAddr::V4(mapped));
            }
            let segments = address.segments();
            !(address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || (segments[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (segments[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)) // documentation
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_upstream(
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
) -> Result<UpstreamExecution, ExecuteError> {
    let config = state.config.current();
    let pool = state.pool();
    let account_count = pool.snapshot().await.len() as u32;
    let attempts = retry_attempt_count(config.upstream.max_retries, account_count);
    let kiro = state.kiro();
    let mut last_error = None;
    let mut actual_model = model.to_string();
    let mut mapped_model = model.to_string();
    let mut request_payload = payload.clone();
    let mut fallback_model = None::<String>;
    let mut attempted_accounts = HashSet::new();
    let initial_route = map_model(requested_model, &config.model_mapping, key_id, None, "");
    let mut model_mapping_rule = initial_route.rule;
    let mut model_path = build_model_path(requested_model, model, "");
    let mut attempt_logs = Vec::new();
    tracing::info!(
        trace_id,
        requested_model,
        initial_model = model,
        max_attempts = attempts,
        account_count,
        "upstream dispatch started"
    );
    for attempt in 0..attempts {
        let lease = match pool
            .acquire_excluding(&actual_model, estimate, &attempted_accounts)
            .await
        {
            Ok(lease) => lease,
            Err(PoolError::NoAvailableAccount(_)) if last_error.is_some() => break,
            Err(PoolError::NoAvailableAccount(_))
                if pool.all_matching_credit_exhausted(&actual_model).await =>
            {
                trigger_quota_shutdown(
                    state,
                    "All compatible Kiro accounts have exhausted their credit allowance",
                );
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
                        trigger_quota_shutdown(
                            state,
                            "All compatible Kiro accounts have exhausted their credit allowance",
                        );
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
                    trigger_quota_shutdown(
                        state,
                        "All enabled Kiro accounts have exhausted their credit allowance",
                    );
                    return Err(ExecuteError::Pool(PoolError::CreditsExhausted));
                }
                Err(error) => return Err(ExecuteError::Pool(error)),
            },
            Err(error) => return Err(ExecuteError::Pool(error)),
        };
        let account = lease.account().await;
        let account_name = account.display_name().to_owned();
        tracing::debug!(
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
            .is_ok()
        {
            tracing::info!(
                trace_id,
                account_id = %account.id,
                "expiring account token refreshed before upstream call"
            );
            persist_refreshed_accounts(state).await?;
        }
        let account = lease.account().await;
        match kiro.generate(&account, &request_payload, None).await {
            Ok(response) => {
                tracing::info!(
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
                return Ok(UpstreamExecution {
                    lease,
                    response,
                    mapped_model,
                    kiro_model: actual_model,
                    model_path,
                    model_mapping_rule,
                    attempts: attempt_logs,
                    payload: request_payload,
                });
            }
            Err(error) if error.is_auth() => {
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
                    "upstream authentication failed; refreshing token"
                );
                let mut ban_account = true;
                if state
                    .refresh_account_token(&pool, &account.id, true)
                    .await
                    .is_ok()
                {
                    persist_refreshed_accounts(state).await?;
                    let refreshed = lease.account().await;
                    request_payload
                        .profile_arn
                        .clone_from(&refreshed.profile_arn);
                    match kiro.generate(&refreshed, &request_payload, None).await {
                        Ok(response) => {
                            let refreshed_name = refreshed.display_name().to_owned();
                            tracing::info!(
                                trace_id,
                                attempt = attempt + 1,
                                account_id = %refreshed.id,
                                account_name = refreshed_name,
                                endpoint = %response.endpoint.name,
                                "upstream authentication retry succeeded"
                            );
                            pool.record_success(&refreshed.id).await;
                            return Ok(UpstreamExecution {
                                lease,
                                mapped_model,
                                kiro_model: actual_model,
                                model_path,
                                model_mapping_rule,
                                attempts: attempt_logs,
                                payload: request_payload,
                                response,
                            });
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
                    let mut event = WebhookEvent::new(
                        WebhookEventKind::AccountBanned,
                        "Kiro account disabled",
                        "Token refresh failed after an authentication error",
                    );
                    event.account_id = Some(account.id.clone());
                    state.notifier().emit(event);
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
                    let mut event = WebhookEvent::new(
                        WebhookEventKind::QuotaExhausted,
                        "Kiro account quota exhausted",
                        "The account was removed from scheduling after repeated quota errors",
                    );
                    event.account_id = Some(account.id.clone());
                    state.notifier().emit(event);
                    if pool.all_matching_credit_exhausted(&actual_model).await {
                        trigger_quota_shutdown(
                            state,
                            "All compatible Kiro accounts have exhausted their credit allowance",
                        );
                    }
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
            Err(error) if error.is_throttle() => {
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
                    "upstream throttled request"
                );
                pool.record_error(&account.id).await;
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
                            fallback_model = Some(fallback.clone());
                            mapped_model = fallback;
                            actual_model = resolved;
                            push_model_path(&mut model_path, &mapped_model);
                            push_model_path(&mut model_path, &actual_model);
                            set_payload_model(&mut request_payload, &actual_model);
                            match kiro.generate(&account, &request_payload, None).await {
                                Ok(response) => {
                                    tracing::info!(
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
                                    return Ok(UpstreamExecution {
                                        lease,
                                        mapped_model,
                                        kiro_model: actual_model,
                                        model_path,
                                        model_mapping_rule,
                                        attempts: attempt_logs,
                                        payload: request_payload,
                                        response,
                                    });
                                }
                                Err(fallback_error) => {
                                    attempt_logs.push(upstream_attempt_log(
                                        attempt + 1,
                                        &account,
                                        &actual_model,
                                        &fallback_error,
                                    ));
                                    last_error = Some(fallback_error);
                                }
                            }
                        }
                    }
                }
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
                trace_id,
                attempt = attempt + 1,
                backoff_ms,
                "waiting before upstream retry"
            );
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        }
    }
    let error = last_error.unwrap_or_else(|| {
        let model_resolution_failed = !attempt_logs.is_empty()
            && attempt_logs
                .iter()
                .all(|attempt| attempt.endpoint == "model-resolution");
        KiroError {
            status: None,
            endpoint: if model_resolution_failed {
                "model-resolution"
            } else {
                "none"
            }
            .into(),
            message: if model_resolution_failed {
                format!("no selected account can serve resolved model '{actual_model}'")
            } else {
                "all upstream attempts failed".into()
            },
        }
    });
    tracing::error!(
        trace_id,
        endpoint = %error.endpoint,
        upstream_status = error.status.unwrap_or_default(),
        error = %sanitize_error_message(&error.message),
        attempted_accounts = attempted_accounts.len(),
        account_id = attempt_logs.last().map(|attempt| attempt.account_id.as_str()).unwrap_or(""),
        account_name = attempt_logs.last().map(|attempt| attempt.account_name.as_str()).unwrap_or(""),
        mapped_model,
        kiro_model = actual_model,
        model_path = %model_path.join(" -> "),
        mapping_rule = model_mapping_rule.as_deref().unwrap_or("none"),
        "all upstream attempts failed"
    );
    Err(dispatch_error(
        error,
        &mapped_model,
        &actual_model,
        &model_path,
        model_mapping_rule,
        attempt_logs,
    ))
}

fn build_model_path(original: &str, mapped: &str, kiro: &str) -> Vec<String> {
    let mut path = Vec::new();
    push_model_path(&mut path, original);
    push_model_path(&mut path, mapped);
    push_model_path(&mut path, kiro);
    path
}

fn push_model_path(path: &mut Vec<String>, model: &str) {
    if !model.is_empty() && path.last().is_none_or(|last| last != model) {
        path.push(model.to_owned());
    }
}

fn upstream_attempt_log(
    attempt: u32,
    account: &kproxy_core::account::Account,
    model: &str,
    error: &KiroError,
) -> UpstreamAttemptLog {
    UpstreamAttemptLog {
        attempt,
        account_id: account.id.clone(),
        account_name: account.display_name().to_owned(),
        model: model.to_owned(),
        available_models: Vec::new(),
        endpoint: error.endpoint.clone(),
        status: error.status,
        error: sanitize_error_message(&error.message),
    }
}

fn dispatch_error(
    error: KiroError,
    mapped_model: &str,
    kiro_model: &str,
    model_path: &[String],
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
) -> ExecuteError {
    let (account_id, account_name) = attempts
        .last()
        .map(|attempt| (attempt.account_id.clone(), attempt.account_name.clone()))
        .unwrap_or_default();
    ExecuteError::Dispatch(DispatchFailure {
        context: RequestLogContext {
            account_id,
            account_name,
            endpoint: error.endpoint.clone(),
            mapped_model: mapped_model.to_owned(),
            kiro_model: kiro_model.to_owned(),
            model_path: model_path.to_vec(),
            model_mapping_rule,
            attempts,
        },
        error,
    })
}

fn retry_attempt_count(max_retries: u32, account_count: u32) -> u32 {
    max_retries.saturating_add(1).min(account_count).max(1)
}

async fn persist_refreshed_accounts(state: &Arc<AppState>) -> Result<(), ExecuteError> {
    crate::tasks::persist_pool_accounts(state)
        .await
        .map_err(|error| {
            ExecuteError::Upstream(KiroError {
                status: None,
                endpoint: "credential-store".into(),
                message: format!("refreshed credentials could not be persisted: {error}"),
            })
        })
}

pub(super) fn set_payload_model(payload: &mut kproxy_translate::KiroPayload, model: &str) {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id = model.into();
    for message in &mut payload.conversation_state.history {
        if let Some(user) = &mut message.user_input_message {
            user.model_id = model.into();
        }
    }
}

pub(super) fn find_model_fallback(
    model: &str,
    models: &[kproxy_kiro::ModelInfo],
) -> Option<String> {
    let (family, version) = model_family(model)?;
    models
        .iter()
        .filter_map(|candidate| {
            let (candidate_family, candidate_version) = model_family(&candidate.model_id)?;
            (candidate_family == family && candidate_version < version)
                .then_some((candidate_version, candidate.model_id.clone()))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, model)| model)
}

fn model_family(model: &str) -> Option<(String, Vec<u32>)> {
    let lower = model.to_ascii_lowercase().replace('.', "-");
    let mut family = Vec::new();
    let mut version = Vec::new();
    let mut found_version = false;
    for part in lower.split('-').filter(|part| !part.is_empty()) {
        if let Ok(number) = part.parse::<u32>() {
            found_version = true;
            version.push(number);
        } else if !found_version {
            family.push(part);
        }
    }
    (!family.is_empty() && !version.is_empty()).then(|| (family.join("-"), version))
}

#[derive(Debug, Default)]
struct RequestLogContext {
    account_id: String,
    account_name: String,
    endpoint: String,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
}

struct DispatchFailure {
    error: KiroError,
    context: RequestLogContext,
}

struct UpstreamExecution {
    lease: AccountLease,
    response: KiroResponse,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
    payload: kproxy_translate::KiroPayload,
}

enum ExecuteError {
    Pool(PoolError),
    Upstream(KiroError),
    Dispatch(DispatchFailure),
    Meter(MeterError),
    ContextLimit(ContextLimitError),
}

pub(super) struct ContextLimitError {
    model: String,
    input_tokens: u64,
    maximum: u64,
}

async fn collect_nonstream_rounds(
    state: &Arc<AppState>,
    trace_id: &str,
    lease: &AccountLease,
    reservation: &mut CreditReservation,
    estimated_credits: f64,
    mut upstream: KiroResponse,
    mut payload: kproxy_translate::KiroPayload,
) -> Result<(DecodedResponse, String, u64), ExecuteError> {
    let config = state.config.current();
    let client_has_tools = payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .is_some_and(|context| !context.tools.is_empty());
    let mut decoded = DecodedResponse::default();
    let mut accumulated_usage = kproxy_kiro::UsageInfo::default();
    let mut accumulated_text = String::new();
    let mut accumulated_reasoning = String::new();
    let mut round = 0;
    loop {
        let (endpoint_definition, response, _upstream_permit) = upstream.into_parts();
        let endpoint = endpoint_definition.name.to_string();
        let events = state
            .kiro()
            .collect_events(response)
            .await
            .map_err(ExecuteError::Upstream)?;
        let event_count = events.len();
        let mut leak_filter = ToolLeakFilter::new(config.features.enable_tool_leak_filter);
        for event in events {
            for event in leak_filter.push(event) {
                decoded.push(event).map_err(|message| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message,
                    })
                })?;
            }
        }
        for event in leak_filter.finish() {
            decoded.push(event).map_err(|message| {
                ExecuteError::Upstream(KiroError {
                    status: None,
                    endpoint: endpoint.clone(),
                    message,
                })
            })?;
        }
        if client_has_tools
            || decoded.tools.is_empty()
            || round >= config.features.auto_continue_rounds.min(30)
        {
            decoded.validate_tool_inputs().map_err(|message| {
                ExecuteError::Upstream(KiroError {
                    status: None,
                    endpoint: endpoint.clone(),
                    message,
                })
            })?;
            fill_missing_usage(state, &mut decoded, &payload).await;
            accumulated_text.push_str(&decoded.text);
            accumulated_reasoning.push_str(&decoded.reasoning);
            decoded.text = accumulated_text;
            decoded.reasoning = accumulated_reasoning;
            let current_round_output_tokens = decoded.usage.output_tokens;
            merge_round_usage(&mut decoded.usage, &accumulated_usage);
            tracing::debug!(
                trace_id,
                account_id = %lease.account_id(),
                endpoint,
                round = round + 1,
                event_count,
                input_tokens = decoded.usage.input_tokens,
                output_tokens = decoded.usage.output_tokens,
                tool_count = decoded.tools.len(),
                "upstream non-stream round decoded"
            );
            return Ok((decoded, endpoint, current_round_output_tokens));
        }
        decoded.validate_tool_inputs().map_err(|message| {
            ExecuteError::Upstream(KiroError {
                status: None,
                endpoint: endpoint.clone(),
                message,
            })
        })?;
        fill_missing_usage(state, &mut decoded, &payload).await;
        let uses = decoded
            .tools
            .values()
            .map(|tool| kproxy_translate::KiroToolUse {
                tool_use_id: tool.id.clone(),
                name: tool.name.clone(),
                input: super::response::repair_json(&tool.input),
            })
            .collect();
        let round_text = std::mem::take(&mut decoded.text);
        accumulated_text.push_str(&round_text);
        accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
        payload = kproxy_translate::auto_continue_payload(&payload, &round_text, uses);
        tracing::info!(
            trace_id,
            account_id = %lease.account_id(),
            endpoint,
            round = round + 1,
            event_count,
            output_tokens = decoded.usage.output_tokens,
            "starting automatic non-stream continuation"
        );
        decoded.tools.clear();
        merge_round_usage(&mut accumulated_usage, &decoded.usage);
        decoded.usage = kproxy_kiro::UsageInfo::default();
        if let Err(error) = reservation.extend(estimated_credits) {
            if matches!(error, MeterError::DailyLimitExceeded) {
                trigger_quota_shutdown(
                    state,
                    "The service daily credit limit was reached during auto-continuation",
                );
            }
            return Err(ExecuteError::Meter(error));
        }
        let account = lease.account().await;
        upstream = state
            .kiro()
            .generate(&account, &payload, None)
            .await
            .map_err(ExecuteError::Upstream)?;
        round += 1;
    }
}

fn merge_round_usage(total: &mut kproxy_kiro::UsageInfo, addition: &kproxy_kiro::UsageInfo) {
    total.input_tokens = total.input_tokens.saturating_add(addition.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(addition.output_tokens);
    total.credits += addition.credits;
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(addition.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(addition.cache_write_tokens);
    total.reasoning_tokens = total
        .reasoning_tokens
        .saturating_add(addition.reasoning_tokens);
}

#[allow(clippy::too_many_arguments)]
async fn nonstream_claude(
    state: Arc<AppState>,
    mut lease: AccountLease,
    mut reservation: CreditReservation,
    upstream: KiroResponse,
    payload: kproxy_translate::KiroPayload,
    trace_id: String,
    request_id: String,
    path: String,
    request: ClaudeRequest,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
    _input_tokens: u64,
    estimated_credits: f64,
    started: Instant,
    prompt_cache: Option<PromptCacheProfile>,
    compaction_summary: Option<String>,
) -> Result<Response, ApiError> {
    let (mut decoded, endpoint, current_round_output_tokens) = collect_nonstream_rounds(
        &state,
        &trace_id,
        &lease,
        &mut reservation,
        estimated_credits,
        upstream,
        payload,
    )
    .await
    .map_err(|error| upstream_error(error, ErrorFormat::Claude))?;
    decoded.restore_tool_names(&claude_web_tool_names(&request));
    let current_round_output_tokens = if current_round_output_tokens == 0 {
        decoded.usage.output_tokens
    } else {
        current_round_output_tokens
    };
    let account_id = lease.account_id();
    let account_name = lease.account().await.display_name().to_owned();
    state
        .prompt_cache
        .apply(&account_id, prompt_cache.as_ref(), &mut decoded.usage);
    let credits = credits(&state, &kiro_model, &decoded);
    lease.settle_credits(credits).await;
    reservation
        .settle(usage_record(
            &mapped_model,
            &request.model,
            &kiro_model,
            &path,
            &decoded,
            credits,
        ))
        .await
        .map_err(|error| meter_error(error, ErrorFormat::Claude))?;
    state.stats.record(request_log(
        &trace_id,
        &request_id,
        &path,
        &mapped_model,
        &request.model,
        &kiro_model,
        &account_id,
        &account_name,
        &endpoint,
        &model_path,
        model_mapping_rule.as_deref(),
        attempts,
        started,
        &decoded,
        credits,
    ));
    tracing::info!(
        trace_id,
        request_id,
        protocol = "claude",
        account_id,
        account_name,
        endpoint,
        model_path = %model_path.join(" -> "),
        mapping_rule = model_mapping_rule.as_deref().unwrap_or("none"),
        input_tokens = decoded.usage.input_tokens,
        output_tokens = decoded.usage.output_tokens,
        credits,
        duration_ms = started.elapsed().as_millis() as u64,
        "client non-stream response completed"
    );
    Ok(Json(decoded.claude_json(
        &request_id,
        &request.model,
        request.max_tokens,
        current_round_output_tokens,
        compaction_summary.as_deref(),
    ))
    .into_response())
}

#[allow(clippy::too_many_arguments)]
async fn nonstream_openai(
    state: Arc<AppState>,
    mut lease: AccountLease,
    mut reservation: CreditReservation,
    upstream: KiroResponse,
    payload: kproxy_translate::KiroPayload,
    trace_id: String,
    request_id: String,
    path: String,
    request: OpenAiRequest,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
    _input_tokens: u64,
    max_tokens: u32,
    estimated_credits: f64,
    started: Instant,
    prompt_cache: Option<PromptCacheProfile>,
    openai_tools: std::collections::HashMap<String, OpenAiToolIdentity>,
) -> Result<Response, ApiError> {
    let (mut decoded, endpoint, current_round_output_tokens) = collect_nonstream_rounds(
        &state,
        &trace_id,
        &lease,
        &mut reservation,
        estimated_credits,
        upstream,
        payload,
    )
    .await
    .map_err(|error| upstream_error(error, ErrorFormat::OpenAi))?;
    let current_round_output_tokens = if current_round_output_tokens == 0 {
        decoded.usage.output_tokens
    } else {
        current_round_output_tokens
    };
    let account_id = lease.account_id();
    let account_name = lease.account().await.display_name().to_owned();
    state
        .prompt_cache
        .apply(&account_id, prompt_cache.as_ref(), &mut decoded.usage);
    let credits = credits(&state, &kiro_model, &decoded);
    lease.settle_credits(credits).await;
    reservation
        .settle(usage_record(
            &mapped_model,
            &request.model,
            &kiro_model,
            &path,
            &decoded,
            credits,
        ))
        .await
        .map_err(|error| meter_error(error, ErrorFormat::OpenAi))?;
    state.stats.record(request_log(
        &trace_id,
        &request_id,
        &path,
        &mapped_model,
        &request.model,
        &kiro_model,
        &account_id,
        &account_name,
        &endpoint,
        &model_path,
        model_mapping_rule.as_deref(),
        attempts,
        started,
        &decoded,
        credits,
    ));
    tracing::info!(
        trace_id,
        request_id,
        protocol = "openai",
        account_id,
        account_name,
        endpoint,
        model_path = %model_path.join(" -> "),
        mapping_rule = model_mapping_rule.as_deref().unwrap_or("none"),
        input_tokens = decoded.usage.input_tokens,
        output_tokens = decoded.usage.output_tokens,
        credits,
        duration_ms = started.elapsed().as_millis() as u64,
        "client non-stream response completed"
    );
    let thinking_format = state.config.current().features.thinking_output_format;
    Ok(Json(decoded.openai_json(
        &request_id,
        &request.model,
        now_secs(),
        max_tokens,
        current_round_output_tokens,
        thinking_format,
        &openai_tools,
    ))
    .into_response())
}

fn openai_tool_identities(
    request: &OpenAiRequest,
) -> std::collections::HashMap<String, OpenAiToolIdentity> {
    request
        .tools
        .iter()
        .filter_map(|tool| {
            let definition = tool.body.get(&tool.r#type)?;
            let name = definition.get("name")?.as_str()?;
            Some((
                kproxy_translate::tool_name(name),
                OpenAiToolIdentity {
                    kind: tool.r#type.clone(),
                    name: name.into(),
                },
            ))
        })
        .collect()
}

fn credits(state: &Arc<AppState>, model: &str, decoded: &DecodedResponse) -> f64 {
    if decoded.usage.credits > 0.0 {
        decoded.usage.credits
    } else {
        fallback_credits(
            state,
            model,
            decoded.usage.input_tokens,
            decoded.usage.output_tokens,
        )
    }
}

fn usage_record(
    model: &str,
    original_model: &str,
    kiro_model: &str,
    path: &str,
    decoded: &DecodedResponse,
    credits: f64,
) -> UsageRecord {
    UsageRecord {
        timestamp: now_secs(),
        model: model.into(),
        original_model: Some(original_model.into()),
        kiro_model: Some(kiro_model.into()),
        input_tokens: decoded.usage.input_tokens,
        output_tokens: decoded.usage.output_tokens,
        credits,
        cache_read_tokens: Some(decoded.usage.cache_read_tokens),
        cache_write_tokens: Some(decoded.usage.cache_write_tokens),
        reasoning_tokens: Some(decoded.usage.reasoning_tokens),
        token_usage_source: if decoded.usage.credits > 0.0 {
            "server"
        } else {
            "estimated"
        }
        .into(),
        path: path.into(),
    }
}

#[allow(clippy::too_many_arguments)]
fn request_log(
    trace_id: &str,
    request_id: &str,
    path: &str,
    model: &str,
    original_model: &str,
    kiro_model: &str,
    account_id: &str,
    account_name: &str,
    endpoint: &str,
    model_path: &[String],
    model_mapping_rule: Option<&str>,
    attempts: Vec<UpstreamAttemptLog>,
    started: Instant,
    decoded: &DecodedResponse,
    credits: f64,
) -> RequestLog {
    RequestLog {
        timestamp: now_secs(),
        trace_id: trace_id.into(),
        request_id: request_id.into(),
        path: path.into(),
        model: model.into(),
        original_model: original_model.into(),
        kiro_model: kiro_model.into(),
        account_id: account_id.into(),
        account_name: account_name.into(),
        endpoint: endpoint.into(),
        model_path: model_path.to_vec(),
        model_mapping_rule: model_mapping_rule.map(str::to_owned),
        attempts,
        duration_ms: started.elapsed().as_millis() as u64,
        status: 200,
        input_tokens: decoded.usage.input_tokens,
        output_tokens: decoded.usage.output_tokens,
        credits,
        error: None,
    }
}

pub async fn count_tokens(State(service): State<ServiceHttpState>, request: Request) -> Response {
    let state = Arc::clone(&service.app);
    let path = request.uri().path().to_string();
    let trace_id = request_trace_id(&request);
    let started = Instant::now();
    let _connection_guard = match state.connections.try_acquire() {
        Some(guard) => guard,
        None => {
            let error = ApiError::overloaded(ErrorFormat::Claude);
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let _admission_guard = match state.admission.try_acquire() {
        Some(guard) => guard,
        None => {
            let error = ApiError::overloaded(ErrorFormat::Claude);
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let result = async {
        let (headers, body, _body_reservations) =
            read_bounded_body(&state, request, ErrorFormat::Claude).await?;
        enforce_claude_user_agent(&state, &headers)?;
        let key_id = authenticate(
            &state,
            &service.allowed_api_key_ids,
            &headers,
            ErrorFormat::Claude,
        )?;
        tracing::debug!(
            trace_id = %trace_id,
            protocol = "claude_count_tokens",
            api_key_id = key_id.as_deref().unwrap_or("anonymous"),
            body_bytes = body.len(),
            "client authentication completed"
        );
        let mut value: Value = serde_json::from_slice(&body).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "Invalid JSON in request body",
                ErrorFormat::Claude,
            )
        })?;
        let object = value.as_object_mut().ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "request body must be a JSON object",
                ErrorFormat::Claude,
            )
        })?;
        object.entry("max_tokens").or_insert_with(|| json!(1));
        let mut request: ClaudeRequest = serde_json::from_value(value).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("Invalid request body: {error}"),
                ErrorFormat::Claude,
            )
        })?;
        validate_claude(&request).map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                error.to_string(),
                ErrorFormat::Claude,
            )
        })?;
        tracing::info!(
            trace_id = %trace_id,
            protocol = "claude_count_tokens",
            model = %request.model,
            message_count = request.messages.len(),
            tool_count = request.tools.len(),
            "client token-count request validated"
        );
        let config = state.config.current();
        if config.features.disable_tools {
            request.tools.clear();
            request.tool_choice = None;
        } else if !config.features.enable_web_tools {
            request.tools.retain(|tool| {
                !tool.r#type.as_deref().is_some_and(|kind| {
                    kind.starts_with("web_search") || kind.starts_with("web_fetch")
                })
            });
        }
        let compact_trigger = compact_trigger_tokens(request.context_management.as_ref());
        let route = map_model(
            &request.model,
            &config.model_mapping,
            key_id.as_deref(),
            None,
            "",
        );
        let mut normal = TranslationOptions::new(route.mapped.clone(), "AI_EDITOR");
        normal.enhance_system_prompt = config.features.enhance_system_prompt;
        normal.compact_mode = compact_trigger.is_some();
        let mut original_payload = claude_to_kiro(&request, &normal);
        let thinking_limit = model_token_limit(&state, &route.mapped, false)
            .unwrap_or(config.features.max_thinking_budget_tokens)
            .min(config.features.max_thinking_budget_tokens);
        apply_adaptive_thinking(
            &mut original_payload,
            request.thinking.as_ref(),
            thinking_enabled_for_model(&request.model, &config.model_thinking_mode),
            config.features.adaptive_thinking,
            thinking_limit,
        );
        let original_input_tokens = state
            .tokenizer
            .estimate_kiro_payload(&original_payload)
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error,
                    ErrorFormat::Claude,
                )
            })?;
        let boundary_applied = apply_compaction_boundary(&mut request);
        let input_tokens = if boundary_applied {
            let mut effective_payload = claude_to_kiro(&request, &normal);
            apply_adaptive_thinking(
                &mut effective_payload,
                request.thinking.as_ref(),
                thinking_enabled_for_model(&request.model, &config.model_thinking_mode),
                config.features.adaptive_thinking,
                thinking_limit,
            );
            state
                .tokenizer
                .estimate_kiro_payload(&effective_payload)
                .await
                .map_err(|error| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error,
                        ErrorFormat::Claude,
                    )
                })?
        } else {
            original_input_tokens
        };
        let mut response = json!({"input_tokens":input_tokens});
        if compact_trigger.is_some() {
            response["context_management"] = json!({
                "original_input_tokens":original_input_tokens
            });
        }
        tracing::info!(
            trace_id = %trace_id,
            protocol = "claude_count_tokens",
            model = %request.model,
            input_tokens,
            original_input_tokens,
            compaction_boundary_applied = boundary_applied,
            duration_ms = started.elapsed().as_millis() as u64,
            "client token-count response completed"
        );
        Ok::<_, ApiError>(Json(response).into_response())
    }
    .await;
    match result {
        Ok(response) => response,
        Err(error) => {
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            error.with_request_id(&trace_id).into_response()
        }
    }
}

pub async fn models(State(service): State<ServiceHttpState>, headers: HeaderMap) -> Response {
    let state = Arc::clone(&service.app);
    let result = async {
        authenticate(
            &state,
            &service.allowed_api_key_ids,
            &headers,
            ErrorFormat::OpenAi,
        )?;
        let config = state.config.current();
        if !config.models.dynamic_discovery {
            let created = now_secs();
            return Ok::<_, ApiError>(
                Json(json!({
                    "object":"list",
                    "data":fallback_models(&config).into_iter().map(|model| json!({
                        "id":model.model_id,"object":"model","created":created,"owned_by":"kiro"
                    })).collect::<Vec<_>>()
                }))
                .into_response(),
            );
        }
        let (cached, fresh) = state.models.get(config.models.cache_ttl_ms);
        if !fresh && state.models.begin_refresh() {
            let refresh_state = Arc::clone(&state);
            tokio::spawn(async move {
                if let Err(error) = crate::tasks::refresh_models(&refresh_state).await {
                    tracing::warn!(%error, "on-demand model discovery failed");
                }
            });
        }
        let models = if cached.is_empty() {
            fallback_models(&config)
        } else {
            cached
        };
        let created = now_secs();
        Ok::<_, ApiError>(
            Json(json!({
                "object":"list",
                "data":models.into_iter().map(|model| json!({
                    "id":model.model_id,"object":"model","created":created,"owned_by":"kiro"
                })).collect::<Vec<_>>()
            }))
            .into_response(),
        )
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

pub async fn event_logging(
    State(service): State<ServiceHttpState>,
    headers: HeaderMap,
) -> Response {
    match authenticate(
        &service.app,
        &service.allowed_api_key_ids,
        &headers,
        ErrorFormat::OpenAi,
    ) {
        Ok(_) => Json(json!({"status":"ok"})).into_response(),
        Err(error) => error.into_response(),
    }
}

pub async fn claude_method_not_allowed() -> Response {
    ApiError::method_not_allowed(ErrorFormat::Claude, "POST").into_response()
}

pub async fn openai_method_not_allowed() -> Response {
    ApiError::method_not_allowed(ErrorFormat::OpenAi, "POST").into_response()
}

pub async fn openai_models_method_not_allowed() -> Response {
    ApiError::method_not_allowed(ErrorFormat::OpenAi, "GET").into_response()
}

pub async fn not_found(OriginalUri(uri): OriginalUri) -> Response {
    let format = if uri.path().contains("messages") {
        ErrorFormat::Claude
    } else {
        ErrorFormat::OpenAi
    };
    ApiError::new(StatusCode::NOT_FOUND, "not found", format).into_response()
}

pub(crate) fn fallback_models(config: &kproxy_core::config::Config) -> Vec<kproxy_kiro::ModelInfo> {
    let mut models = kproxy_kiro::static_models()
        .into_iter()
        .map(|model| (model.model_id.clone(), model))
        .collect::<std::collections::BTreeMap<_, _>>();
    let configured_ids = config
        .model_mapping
        .iter()
        .flat_map(|rule| rule.target_models.iter().cloned())
        .collect::<Vec<_>>();
    if !config.features.default_model_id.trim().is_empty() {
        models
            .entry(config.features.default_model_id.clone())
            .or_insert_with(|| configured_model_info(config.features.default_model_id.clone()));
    }
    for model_id in configured_ids {
        models
            .entry(model_id.clone())
            .or_insert_with(|| configured_model_info(model_id));
    }
    models.into_values().collect()
}

fn configured_model_info(model_id: String) -> kproxy_kiro::ModelInfo {
    kproxy_kiro::ModelInfo {
        model_name: model_id.clone(),
        model_id,
        description: "Configured Kiro upstream model".into(),
        rate_multiplier: None,
        token_limits: None,
    }
}

fn claude_web_tool_names(request: &ClaudeRequest) -> std::collections::HashMap<String, String> {
    request
        .tools
        .iter()
        .filter_map(|tool| {
            let kind = tool.r#type.as_deref()?;
            if kind.starts_with("web_search") {
                Some(("web_search".into(), kind.into()))
            } else if kind.starts_with("web_fetch") {
                Some(("web_fetch".into(), kind.into()))
            } else {
                None
            }
        })
        .collect()
}

fn authenticate(
    state: &Arc<AppState>,
    allowed_api_key_ids: &HashSet<String>,
    headers: &HeaderMap,
    format: ErrorFormat,
) -> Result<Option<String>, ApiError> {
    let presented = headers
        .get("x-api-key")
        .or_else(|| headers.get("anthropic-api-key"))
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
        });
    let key_id = state.meter.authenticate(presented).map_err(|_| {
        let mut error = ApiError::new(StatusCode::UNAUTHORIZED, "invalid API key", format);
        error.authenticate = true;
        error.suppress_model_stats = true;
        error
    })?;
    if !allowed_api_key_ids.is_empty()
        && key_id
            .as_ref()
            .is_none_or(|id| !allowed_api_key_ids.contains(id))
    {
        let mut error = ApiError::new(StatusCode::UNAUTHORIZED, "invalid API key", format);
        error.authenticate = true;
        error.suppress_model_stats = true;
        return Err(error);
    }
    Ok(key_id)
}

fn enforce_claude_user_agent(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), ApiError> {
    if !state.config.current().server.enforce_user_agent_check {
        return Ok(());
    }
    let value = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let valid =
        value.starts_with("claude-cli/") && value.contains(" (external,") && value.ends_with(')');
    if valid {
        Ok(())
    } else {
        let mut error = ApiError::new(
            StatusCode::BAD_REQUEST,
            "Access denied. 本服务仅限 Claude Code 客户端使用，禁止通过其他方式接入。",
            ErrorFormat::Claude,
        );
        error.suppress_model_stats = true;
        Err(error)
    }
}

fn enforce_context(
    state: &Arc<AppState>,
    input_tokens: u64,
    compact: bool,
    model: &str,
    format: ErrorFormat,
) -> Result<(), ApiError> {
    if let Err(limit) = check_context_limit(state, input_tokens, compact, model) {
        let mut error = ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "prompt is too long: {} tokens > {}",
                limit.input_tokens, limit.maximum
            ),
            format,
        );
        error.log_context.mapped_model = limit.model.clone();
        error.log_context.kiro_model = limit.model.clone();
        error.log_context.model_path = vec![limit.model];
        Err(error)
    } else {
        Ok(())
    }
}

pub(super) fn check_context_limit(
    state: &Arc<AppState>,
    input_tokens: u64,
    compact: bool,
    model: &str,
) -> Result<(), ContextLimitError> {
    let maximum = context_maximum(state, compact, model);
    if input_tokens > maximum {
        Err(ContextLimitError {
            model: model.to_owned(),
            input_tokens,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn context_maximum(state: &Arc<AppState>, compact: bool, model: &str) -> u64 {
    let context = &state.config.current().context;
    let ratio = if compact {
        context.compact_safe_input_ratio
    } else {
        context.safe_input_ratio
    };
    let model_maximum = model_token_limit(state, model, true).unwrap_or(context.max_input_tokens);
    (f64::from(model_maximum) * ratio) as u64
}

fn compact_target_tokens(state: &Arc<AppState>, model: &str, trigger: u64) -> u64 {
    // Leave meaningful room for subsequent tool turns rather than compacting
    // to just below the trigger and immediately repeating the operation.
    trigger
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(trigger)
        .min(context_maximum(state, true, model))
        .max(1)
}

fn append_current_request_to_compaction(mut summary: String, request: &ClaudeRequest) -> String {
    let Some(content) = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| &message.content)
    else {
        return summary;
    };
    let text = match content {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let characters = text.chars().collect::<Vec<_>>();
    let text = if characters.len() > 4_096 {
        format!(
            "{} … [current message compressed] … {}",
            characters[..3_000].iter().collect::<String>(),
            characters[characters.len() - 1_000..]
                .iter()
                .collect::<String>()
        )
    } else {
        text
    };
    summary.push_str("\n\n[Current user message]\n");
    summary.push_str(&text);
    summary
}

fn model_token_limit(state: &Arc<AppState>, model: &str, input: bool) -> Option<u32> {
    state
        .resolved_model_info(model)
        .and_then(|model| model.token_limits)
        .and_then(|limits| {
            if input {
                limits.max_input_tokens
            } else {
                limits.max_output_tokens
            }
        })
}

fn estimated_credits(
    input_tokens: u64,
    max_tokens: u32,
    config: &kproxy_core::config::PoolConfig,
) -> f64 {
    // Kiro does not publish a deterministic token-to-credit formula. This is
    // intentionally a *reservation* heuristic; settlement replaces it with
    // server-reported credits whenever available. Operators can tune the
    // coefficient/cap if upstream pricing or their traffic mix changes.
    let estimated_tokens = input_tokens as f64
        + f64::from(max_tokens).min(f64::from(config.credit_estimate_output_token_cap));
    estimated_tokens / 1_000.0 * config.credit_estimate_per_1k_tokens
}

fn meter_error(error: MeterError, format: ErrorFormat) -> ApiError {
    match error {
        MeterError::Unauthorized => {
            ApiError::new(StatusCode::UNAUTHORIZED, error.to_string(), format)
        }
        MeterError::LimitExceeded => {
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, error.to_string(), format)
        }
        MeterError::DailyLimitExceeded => {
            ApiError::new(StatusCode::UNAUTHORIZED, error.to_string(), format)
        }
        MeterError::Persist(_) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string(), format)
        }
    }
}

fn reserve_credits(
    state: &Arc<AppState>,
    key_id: Option<&str>,
    estimate: f64,
    format: ErrorFormat,
) -> Result<CreditReservation, ApiError> {
    match state.meter.reserve(key_id, estimate) {
        Ok(reservation) => Ok(reservation),
        Err(MeterError::DailyLimitExceeded) => {
            trigger_quota_shutdown(
                state,
                "The configured service daily credit limit has been reached",
            );
            Err(meter_error(MeterError::DailyLimitExceeded, format))
        }
        Err(error) => Err(meter_error(error, format)),
    }
}

pub(super) fn trigger_quota_shutdown(state: &Arc<AppState>, reason: &str) {
    if !state.begin_quota_shutdown() {
        return;
    }
    state.notifier().emit(WebhookEvent::new(
        WebhookEventKind::QuotaExhausted,
        "Proxy credit quota exhausted",
        reason,
    ));
    state.notifier().emit(WebhookEvent::new(
        WebhookEventKind::ServiceDegraded,
        "Proxy service stopping",
        "The service stopped accepting requests after quota exhaustion",
    ));
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        // Let the current error response and queued webhook deliveries leave the process.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        shutdown.cancel();
    });
}

fn upstream_error(error: ExecuteError, format: ErrorFormat) -> ApiError {
    match error {
        ExecuteError::Pool(PoolError::QueueFull | PoolError::QueueTimeout) => {
            ApiError::retryable("Service busy, please retry", format)
        }
        ExecuteError::Pool(PoolError::NoAvailableAccount(_)) => {
            ApiError::retryable("Service temporarily unavailable, please retry", format)
        }
        ExecuteError::Pool(PoolError::CreditsExhausted) => ApiError::new(
            StatusCode::UNAUTHORIZED,
            "Credit quota exhausted; service is paused until credits reset",
            format,
        ),
        ExecuteError::Meter(error) => meter_error(error, format),
        ExecuteError::ContextLimit(limit) => {
            let mut error = ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "prompt is too long: {} tokens > {}",
                    limit.input_tokens, limit.maximum
                ),
                format,
            );
            error.log_context.mapped_model = limit.model.clone();
            error.log_context.kiro_model = limit.model.clone();
            error.log_context.model_path = vec![limit.model];
            error
        }
        ExecuteError::Upstream(error) => {
            upstream_api_error(error, RequestLogContext::default(), format)
        }
        ExecuteError::Dispatch(failure) => {
            upstream_api_error(failure.error, failure.context, format)
        }
    }
}

fn upstream_api_error(
    error: KiroError,
    context: RequestLogContext,
    format: ErrorFormat,
) -> ApiError {
    let status = match error.status {
        Some(413) => StatusCode::PAYLOAD_TOO_LARGE,
        Some(400) if upstream_bad_request_is_actionable(&error.message) => StatusCode::BAD_REQUEST,
        // kproxy already validates the public request and enforces its context
        // window before dispatch. A remaining opaque upstream 400 (including
        // an empty body or "Internal Server Error") is an integration/upstream
        // failure, not an actionable Claude Code request error.
        Some(400) => StatusCode::BAD_GATEWAY,
        Some(401 | 403) => StatusCode::SERVICE_UNAVAILABLE,
        Some(402 | 429) => StatusCode::TOO_MANY_REQUESTS,
        _ => StatusCode::BAD_GATEWAY,
    };
    let message = if error.message.trim().is_empty() {
        "Upstream service error, please retry later".to_owned()
    } else {
        error.message
    };
    let mut output = ApiError::new(status, message, format);
    output.retry_after = matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS
    );
    output.log_context = Box::new(context);
    output
}

fn upstream_bad_request_is_actionable(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "prompt is too long",
        "context length exceeded",
        "input is too long",
        "maximum context",
        "invalid request",
        "validationexception",
        "validation error",
        "malformed request",
        "unsupported model",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

struct ApiError {
    status: StatusCode,
    message: String,
    format: ErrorFormat,
    allow: Option<&'static str>,
    authenticate: bool,
    retry_after: bool,
    suppress_model_stats: bool,
    request_id: Option<String>,
    log_context: Box<RequestLogContext>,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>, format: ErrorFormat) -> Self {
        Self {
            status,
            message: message.into(),
            format,
            allow: None,
            authenticate: false,
            retry_after: false,
            suppress_model_stats: false,
            request_id: None,
            log_context: Box::default(),
        }
    }

    fn overloaded(format: ErrorFormat) -> Self {
        let mut error = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server is overloaded, please retry",
            format,
        );
        error.retry_after = true;
        error
    }

    fn retryable(message: impl Into<String>, format: ErrorFormat) -> Self {
        let mut error = Self::new(StatusCode::SERVICE_UNAVAILABLE, message, format);
        error.retry_after = true;
        error
    }

    fn method_not_allowed(format: ErrorFormat, allow: &'static str) -> Self {
        let mut error = Self::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", format);
        error.allow = Some(allow);
        error
    }

    fn with_request_id(mut self, request_id: &str) -> Self {
        self.request_id = Some(request_id.to_owned());
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        let mut response = (
            status,
            Json(error_envelope(
                self.format,
                status.as_u16(),
                &self.message,
                self.request_id.as_deref(),
            )),
        )
            .into_response();
        if let Some(request_id) = self.request_id.as_deref() {
            if let Ok(value) = HeaderValue::from_str(request_id) {
                response
                    .headers_mut()
                    .insert(header::HeaderName::from_static("request-id"), value);
            }
        }
        if let Some(allow) = self.allow {
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static(allow));
        }
        if self.authenticate {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

#[cfg(test)]
mod model_tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[tokio::test]
    async fn context_limits_use_the_resolved_kiro_model_alias() {
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
        let accounts = kproxy_store::accounts::AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let state = Arc::new(AppState::new(
            paths,
            kproxy_store::config_loader::ConfigHandle::new(kproxy_core::config::Config::default()),
            accounts,
        ));
        state.models.finish_refresh(vec![kproxy_kiro::ModelInfo {
            model_id: "claude-sonnet-4.6".into(),
            model_name: String::new(),
            description: String::new(),
            rate_multiplier: None,
            token_limits: Some(kproxy_kiro::client::TokenLimits {
                max_input_tokens: Some(100_000),
                max_output_tokens: Some(16_384),
            }),
        }]);

        assert_eq!(
            model_token_limit(&state, "claude-4.6-sonnet", true),
            Some(100_000)
        );
        assert!(check_context_limit(&state, 96_000, false, "claude-4.6-sonnet").is_err());
    }

    #[test]
    fn fallback_chooses_the_highest_lower_model_in_the_same_family() {
        let models = ["claude-opus-4-5", "claude-opus-4-6", "claude-sonnet-4-9"]
            .into_iter()
            .map(|model_id| kproxy_kiro::ModelInfo {
                model_id: model_id.into(),
                model_name: String::new(),
                description: String::new(),
                rate_multiplier: None,
                token_limits: None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            find_model_fallback("claude-opus-4-7", &models).as_deref(),
            Some("claude-opus-4-6")
        );
    }

    #[test]
    fn remote_image_addresses_must_be_public() {
        for address in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6("fc00::1".parse().expect("valid IPv6")),
            IpAddr::V6("fe80::1".parse().expect("valid IPv6")),
        ] {
            assert!(!is_public_address(address), "{address} must be rejected");
        }
        assert!(is_public_address(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }

    #[test]
    fn retry_limit_is_never_expanded_by_account_count() {
        assert_eq!(retry_attempt_count(3, 50), 4);
        assert_eq!(retry_attempt_count(3, 2), 2);
        assert_eq!(retry_attempt_count(0, 0), 1);
    }

    #[test]
    fn opaque_upstream_bad_requests_are_reported_as_gateway_failures() {
        for message in ["Internal Server Error", "", "{}"] {
            let error = upstream_api_error(
                KiroError {
                    status: Some(400),
                    endpoint: "test".into(),
                    message: message.into(),
                },
                RequestLogContext::default(),
                ErrorFormat::Claude,
            );
            assert_eq!(error.status, StatusCode::BAD_GATEWAY, "{message:?}");
        }

        let actionable = upstream_api_error(
            KiroError {
                status: Some(400),
                endpoint: "test".into(),
                message: "prompt is too long: 200001 tokens > 200000".into(),
            },
            RequestLogContext::default(),
            ErrorFormat::Claude,
        );
        assert_eq!(actionable.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn model_path_omits_empty_and_duplicate_hops() {
        assert_eq!(
            build_model_path("client", "mapped", "kiro"),
            ["client", "mapped", "kiro"]
        );
        assert_eq!(build_model_path("same", "same", ""), ["same"]);
    }

    #[test]
    fn credit_reservation_heuristic_is_configurable_and_capped() {
        let config = kproxy_core::config::PoolConfig {
            credit_estimate_per_1k_tokens: 2.0,
            credit_estimate_output_token_cap: 100,
            ..kproxy_core::config::PoolConfig::default()
        };
        assert_eq!(estimated_credits(900, 500, &config), 2.0);
    }

    #[test]
    fn fallback_models_use_catalog_and_keep_configured_targets() {
        let mut config = kproxy_core::config::Config::default();
        config.features.default_model_id = "private-model".into();
        let models = fallback_models(&config);
        assert!(models.iter().any(|model| model.model_id == "auto"));
        assert!(models
            .iter()
            .any(|model| model.model_id == "claude-sonnet-4.6"));
        assert!(models.iter().any(|model| model.model_id == "private-model"));
    }
}

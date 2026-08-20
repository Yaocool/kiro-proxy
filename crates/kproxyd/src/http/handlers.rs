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
    apply_compaction_boundary, apply_context_management_edits, claude_loaded_tools,
    claude_pending_server_tool_uses, claude_to_kiro, compact_trigger_tokens, error_envelope,
    has_context_management_edits, matches_type_family, openai_to_kiro, resume_tool_search_payload,
    resume_web_search_payload, sanitize_error_message, tool_search_continue_payload_batch,
    validate_claude, validate_openai, web_search_continue_payload_batch, ClaudeRequest,
    ClaudeToolSearchBudget, ClaudeToolSearchCatalog, ClaudeWebSearchTrace, ErrorFormat,
    KiroToolUse, OpenAiRequest, TranslationOptions, ValidationError,
};
use rand::Rng;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

use crate::meter::{now_secs, CreditReservation, MeterError, UsageRecord};
use crate::state::AppState;
use crate::stats::{RequestDiagnostics, RequestLog, UpstreamAttemptLog};

use super::prompt_cache::PromptCacheProfile;
use super::request_trace_id;
use super::response::{ClaudeServerEvent, DecodedResponse, OpenAiToolIdentity, ToolLeakFilter};
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

pub async fn readiness(State(service): State<ServiceHttpState>) -> Response {
    let counts = service.app.pool().health_counts().await;
    let mut reasons = service.app.task_registry.readiness_issues(&service.app);
    if counts[0] == 0 {
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
            "available_accounts":counts[0],
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
        diagnostics: RequestDiagnostics {
            client_status: error.status.as_u16(),
            upstream_status: error.upstream_status.or_else(|| {
                error
                    .log_context
                    .attempts
                    .iter()
                    .rev()
                    .find_map(|attempt| attempt.status)
            }),
            error_code: error.error_code.to_owned(),
            error_stage: error.error_stage.to_owned(),
            account_error: error.account_error,
            ..RequestDiagnostics::default()
        },
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
    let compact_trigger = compact_trigger_tokens(request.context_management.as_ref());
    // Claude discards everything before the latest compaction block. Apply
    // that semantic boundary before validating references or authenticated
    // replay records so ignored history cannot reject the effective request.
    let compact_boundary_applied = apply_compaction_boundary(&mut request);
    validate_claude(&request).map_err(claude_validation_error)?;
    let context_edit_stats = apply_context_management_edits(&mut request);
    if context_edit_stats.changed() {
        tracing::info!(
            trace_id = %trace_id,
            cleared_tool_results = context_edit_stats.cleared_tool_results,
            cleared_tool_inputs = context_edit_stats.cleared_tool_inputs,
            "Claude context edits applied locally"
        );
    }
    kproxy_translate::validate_web_search_replay_content(&request, &state.web_search_replay)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, error, ErrorFormat::Claude))?;
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
            !tool.r#type.as_deref().is_some_and(|kind| {
                matches_type_family(kind, "web_search") || matches_type_family(kind, "web_fetch")
            })
        });
    }
    if !config.features.enable_tool_search
        && request.tools.iter().any(|tool| {
            tool.defer_loading
                || tool
                    .r#type
                    .as_deref()
                    .is_some_and(kproxy_translate::is_tool_search_type)
        })
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Anthropic Tool Search is disabled by proxy configuration; remove defer_loading/Tool Search or enable features.enable_tool_search",
            ErrorFormat::Claude,
        ));
    }
    // Discover pending calls only from the already validated effective
    // history, so discarded calls cannot be executed again.
    let pending_server_tools = claude_pending_server_tool_uses(&request);
    let web_search_tool = request.tools.iter().find(|tool| {
        tool.r#type
            .as_deref()
            .is_some_and(|kind| matches_type_family(kind, "web_search"))
    });
    let web_search_client_limit = web_search_tool.is_some_and(|tool| tool.max_uses.is_some());
    let web_search_proxy_limit = config.features.web_search_max_rounds.max(1);
    if let Some(requested) = web_search_tool.and_then(|tool| tool.max_uses) {
        if requested > web_search_proxy_limit {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "web_search max_uses={requested} exceeds this proxy's configured execution limit of {web_search_proxy_limit}"
                ),
                ErrorFormat::Claude,
            ));
        }
    }
    let web_search_max_rounds = web_search_tool
        .map(|tool| tool.max_uses.unwrap_or(web_search_proxy_limit))
        .unwrap_or(0);
    let max_tool_search_operations = config.features.tool_search_max_operations.clamp(1, 256);
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
    options.web_search_replay = Some(state.web_search_replay.clone());
    let mut payload = claude_to_kiro(&request, &options);
    let original_tool_count = request.tools.len();
    let catalog_bytes = request
        .tools
        .iter()
        .filter(|tool| tool.defer_loading)
        .filter_map(|tool| serde_json::to_vec(tool).ok())
        .fold(2usize, |total, tool| {
            total.saturating_add(tool.len()).saturating_add(1)
        });
    // Keep the deferred catalog proxy-side and resolve server calls that were
    // intentionally left pending by a prior mixed client/server turn.
    let (next_request, tool_search) = tokio::task::spawn_blocking(move || {
        let mut request = request;
        let catalog = ClaudeToolSearchCatalog::take_from_request(&mut request).map(Arc::new);
        (request, catalog)
    })
    .await
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Tool Search catalog worker failed: {error}"),
            ErrorFormat::Claude,
        )
    })?;
    request = next_request;
    // Pending server calls may execute an external Web Search before the
    // first Kiro generation. Reject an oversized initial working set before
    // acquiring a search account or making that external request. The same
    // budgets are checked again after pending results and internal searches
    // have changed the payload.
    let initial_tool_tokens = (state
        .tokenizer
        .estimate_kiro_tools(&payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64)
        .max(
            state
                .tokenizer
                .estimate_claude_tools(&claude_loaded_tools(&request))
                .await
                .map_err(|error| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error,
                        ErrorFormat::Claude,
                    )
                })? as u64,
        );
    enforce_payload_budget(
        &state,
        initial_tool_tokens,
        serialized_payload_bytes(&payload, ErrorFormat::Claude)?,
        loaded_tool_count(&payload),
        tool_search.is_some(),
        ErrorFormat::Claude,
    )?;
    let mut resumed_tool_searches = Vec::new();
    let mut resumed_web_searches = Vec::new();
    let mut resumed_server_events = Vec::new();
    let mut pending_web_searches = Vec::new();
    let mut loaded_names = loaded_tool_names(&payload);
    let mut tool_search_operations = 0u32;
    for tool_use in pending_server_tools {
        if tool_use.name == "web_search" {
            if web_search_max_rounds == 0 {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "pending web_search call has no matching web_search tool definition",
                    ErrorFormat::Claude,
                ));
            }
            pending_web_searches.push(tool_use);
            continue;
        }
        let Some(catalog) = tool_search
            .as_ref()
            .filter(|catalog| catalog.is_search_tool(&tool_use.name))
        else {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "pending server tool '{}' has no matching supported tool definition",
                    tool_use.name
                ),
                ErrorFormat::Claude,
            ));
        };
        let mut outcome = if tool_search_operations >= max_tool_search_operations {
            catalog.unavailable_outcome(
                &tool_use,
                format!("Tool Search operation limit of {max_tool_search_operations} was reached"),
            )
        } else {
            let budget = remaining_tool_search_budget(&state, &payload, false)
                .await
                .map_err(|message| {
                    ApiError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        message,
                        ErrorFormat::Claude,
                    )
                })?;
            tool_search_operations += 1;
            match catalog
                .search_with_budget_excluding_async(tool_use.clone(), budget, loaded_names.clone())
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => catalog.unavailable_outcome(&tool_use, error),
            }
        };
        outcome.trace.id = tool_use.tool_use_id.clone();
        outcome.trace = outcome.trace.result_only();
        loaded_names.extend(
            outcome
                .tools
                .iter()
                .map(|tool| tool.tool_specification.name.clone()),
        );
        resume_tool_search_payload(&mut payload, &tool_use, &outcome);
        resumed_server_events.push(ClaudeServerEvent::ToolSearch {
            index: resumed_tool_searches.len(),
            preceding_text: String::new(),
        });
        resumed_tool_searches.push(outcome.trace);
    }
    if !pending_web_searches.is_empty() {
        let needs_search_account = pending_web_searches
            .iter()
            .filter(|tool_use| {
                tool_use
                    .input
                    .get("query")
                    .and_then(Value::as_str)
                    .is_some_and(|query| !query.trim().is_empty())
            })
            .take(web_search_max_rounds as usize)
            .next()
            .is_some();
        let search_lease =
            if needs_search_account {
                Some(state.pool().acquire("", 0.0, &[]).await.map_err(|error| {
                    upstream_error(ExecuteError::Pool(error), ErrorFormat::Claude)
                })?)
            } else {
                None
            };
        let mut resumed_web_search_uses = 0u32;
        for tool_use in pending_web_searches {
            let query = tool_use
                .input
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            let trace = if resumed_web_search_uses >= web_search_max_rounds {
                ClaudeWebSearchTrace::error(
                    tool_use.tool_use_id.clone(),
                    &query,
                    if web_search_client_limit {
                        "max_uses_exceeded"
                    } else {
                        "unavailable"
                    },
                    format!("web search is limited to {web_search_max_rounds} uses"),
                )
            } else if query.is_empty() {
                ClaudeWebSearchTrace::error(
                    tool_use.tool_use_id.clone(),
                    &query,
                    "invalid_tool_input",
                    "web search query must not be empty".into(),
                )
            } else {
                resumed_web_search_uses += 1;
                match search_lease.as_ref() {
                    Some(lease) => match execute_kiro_web_search(&state, lease, &query).await {
                        Ok(results) => ClaudeWebSearchTrace::success(
                            tool_use.tool_use_id.clone(),
                            &query,
                            results,
                        ),
                        Err(error) => ClaudeWebSearchTrace::error(
                            tool_use.tool_use_id.clone(),
                            &query,
                            web_search_error_code(&error),
                            sanitize_error_message(&error.to_string()),
                        )
                        .executed(),
                    },
                    None => ClaudeWebSearchTrace::error(
                        tool_use.tool_use_id.clone(),
                        &query,
                        "unavailable",
                        "web search account is unavailable".into(),
                    ),
                }
            }
            .result_only();
            resume_web_search_payload(&mut payload, &tool_use, &trace);
            resumed_server_events.push(ClaudeServerEvent::WebSearch {
                index: resumed_web_searches.len(),
                preceding_text: String::new(),
            });
            resumed_web_searches.push(trace);
        }
    }
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
    let full_tool_tokens = state
        .tokenizer
        .estimate_claude_tools(&claude_loaded_tools(&request))
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64;
    let tool_tokens = (state
        .tokenizer
        .estimate_kiro_tools(&payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64)
        .max(full_tool_tokens);
    let payload_bytes = serialized_payload_bytes(&payload, ErrorFormat::Claude)?;
    let diagnostics = RequestDiagnostics {
        original_tool_count,
        loaded_tool_count: loaded_tool_count(&payload),
        deferred_tool_count: tool_search
            .as_ref()
            .map_or(0, |catalog| catalog.deferred_len()),
        loaded_tool_bytes: loaded_tool_bytes(&payload),
        catalog_bytes,
        tool_tokens,
        payload_bytes,
        ..RequestDiagnostics::default()
    };
    enforce_payload_budget(
        &state,
        tool_tokens,
        payload_bytes,
        loaded_tool_count(&payload),
        tool_search.is_some(),
        ErrorFormat::Claude,
    )?;
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
        original_tool_count = diagnostics.original_tool_count,
        tool_tokens,
        payload_bytes,
        loaded_tool_count = loaded_tool_count(&payload),
        loaded_tool_bytes = diagnostics.loaded_tool_bytes,
        deferred_tool_count = tool_search.as_ref().map_or(0, |catalog| catalog.deferred_len()),
        catalog_bytes = diagnostics.catalog_bytes,
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
                tool_search,
                max_tool_search_operations,
                tool_search_operations,
                web_search_max_rounds,
                web_search_client_limit,
                resumed_tool_searches,
                resumed_web_searches,
                resumed_server_events,
                diagnostics,
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
        compacted,
        estimate,
        started,
        prompt_cache,
        compaction_summary,
        tool_search,
        max_tool_search_operations,
        tool_search_operations,
        web_search_max_rounds,
        web_search_client_limit,
        resumed_tool_searches,
        resumed_web_searches,
        resumed_server_events,
        diagnostics,
        web_tool_names,
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
    let tool_tokens = state
        .tokenizer
        .estimate_kiro_tools(&payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::OpenAi,
            )
        })? as u64;
    let payload_bytes = serialized_payload_bytes(&payload, ErrorFormat::OpenAi)?;
    let diagnostics = RequestDiagnostics {
        original_tool_count: request.tools.len(),
        loaded_tool_count: loaded_tool_count(&payload),
        deferred_tool_count: 0,
        loaded_tool_bytes: loaded_tool_bytes(&payload),
        catalog_bytes: 0,
        tool_tokens,
        payload_bytes,
        ..RequestDiagnostics::default()
    };
    enforce_payload_budget(
        &state,
        tool_tokens,
        payload_bytes,
        loaded_tool_count(&payload),
        false,
        ErrorFormat::OpenAi,
    )?;
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
        original_tool_count = diagnostics.original_tool_count,
        tool_tokens,
        payload_bytes,
        loaded_tool_count = loaded_tool_count(&payload),
        loaded_tool_bytes = diagnostics.loaded_tool_bytes,
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
                tool_search: None,
                max_tool_search_operations: 0,
                tool_search_operations: 0,
                web_search_max_rounds: 0,
                web_search_client_limit: false,
                resumed_tool_searches: Vec::new(),
                resumed_web_searches: Vec::new(),
                resumed_server_events: Vec::new(),
                diagnostics,
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
        diagnostics,
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
                notify_quota_degradation(
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
                        notify_quota_degradation(
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
                    notify_quota_degradation(
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
        match state.generate(&account, &request_payload).await {
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
                    match state.generate(&refreshed, &request_payload).await {
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
                        notify_quota_degradation(
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
                            match state.generate(&account, &request_payload).await {
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
    pub(super) model: String,
    pub(super) input_tokens: u64,
    pub(super) maximum: u64,
}

#[allow(clippy::too_many_arguments)]
async fn collect_nonstream_rounds(
    state: &Arc<AppState>,
    trace_id: &str,
    lease: &AccountLease,
    reservation: &mut CreditReservation,
    estimated_credits: f64,
    mut upstream: KiroResponse,
    mut payload: kproxy_translate::KiroPayload,
    compact: bool,
    max_output_tokens: u32,
    tool_search: Option<&Arc<ClaudeToolSearchCatalog>>,
    max_tool_search_operations: u32,
    mut tool_search_operations: u32,
    web_search_max_rounds: u32,
    web_search_client_limit: bool,
    resumed_tool_searches: Vec<kproxy_translate::ClaudeToolSearchTrace>,
    resumed_web_searches: Vec<ClaudeWebSearchTrace>,
    resumed_server_events: Vec<ClaudeServerEvent>,
) -> Result<(DecodedResponse, String, u64), ExecuteError> {
    let config = state.config.current();
    let max_tool_search_rounds = config.features.tool_search_max_rounds.clamp(1, 8);
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
    let mut accumulated_searches = resumed_tool_searches;
    let mut accumulated_web_searches = resumed_web_searches;
    let mut accumulated_server_events = resumed_server_events;
    let mut round = 0;
    let mut search_round = 0;
    let mut web_search_round = accumulated_web_searches
        .iter()
        .filter(|search| search.executed)
        .count() as u32;
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
        decoded.validate_tool_inputs().map_err(|message| {
            ExecuteError::Upstream(KiroError {
                status: None,
                endpoint: endpoint.clone(),
                message,
            })
        })?;
        fill_missing_usage(state, &mut decoded, &payload).await;
        let output_exhausted = accumulated_usage
            .output_tokens
            .saturating_add(decoded.usage.output_tokens)
            >= u64::from(max_output_tokens);

        let search_keys = tool_search
            .map(|catalog| {
                decoded
                    .tools
                    .iter()
                    .filter(|(_, tool)| catalog.is_search_tool(&tool.name))
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(catalog) = tool_search.filter(|_| !search_keys.is_empty()) {
            let search_uses = search_keys
                .into_iter()
                .map(|key| {
                    let search = decoded
                        .tools
                        .remove(&key)
                        .expect("Tool Search buffer exists");
                    KiroToolUse {
                        tool_use_id: search.id,
                        name: search.name,
                        input: super::response::repair_json(&search.input),
                    }
                })
                .collect::<Vec<_>>();
            let parallel_web_uses = if web_search_max_rounds > 0 {
                let keys = decoded
                    .tools
                    .iter()
                    .filter(|(_, tool)| tool.name == "web_search")
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                keys.into_iter()
                    .map(|key| {
                        let search = decoded
                            .tools
                            .remove(&key)
                            .expect("web search buffer exists");
                        KiroToolUse {
                            tool_use_id: search.id,
                            name: search.name,
                            input: super::response::repair_json(&search.input),
                        }
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let round_text = std::mem::take(&mut decoded.text);
            let mut preceding_text = std::mem::take(&mut accumulated_text);
            preceding_text.push_str(&round_text);

            if search_round >= max_tool_search_rounds {
                for search_use in &search_uses {
                    let mut trace = catalog.pending_trace(search_use);
                    trace.id = format!("srvtoolu_{}", Uuid::new_v4().simple());
                    accumulated_server_events.push(ClaudeServerEvent::ToolSearch {
                        index: accumulated_searches.len(),
                        preceding_text: std::mem::take(&mut preceding_text),
                    });
                    accumulated_searches.push(trace);
                }
                for search_use in &parallel_web_uses {
                    accumulated_server_events.push(ClaudeServerEvent::WebSearch {
                        index: accumulated_web_searches.len(),
                        preceding_text: std::mem::take(&mut preceding_text),
                    });
                    accumulated_web_searches.push(ClaudeWebSearchTrace::pending(
                        format!("srvtoolu_{}", Uuid::new_v4().simple()),
                        search_use.input.clone(),
                    ));
                }
                accumulated_reasoning.push_str(&decoded.reasoning);
                decoded.reasoning = accumulated_reasoning;
                decoded.tool_searches = accumulated_searches;
                decoded.web_searches = accumulated_web_searches;
                decoded.claude_server_events = accumulated_server_events;
                if decoded.tools.is_empty() {
                    decoded.stop_reason = Some(
                        if output_exhausted {
                            "max_tokens"
                        } else {
                            "pause_turn"
                        }
                        .into(),
                    );
                }
                merge_round_usage(&mut decoded.usage, &accumulated_usage);
                let total_output_tokens = decoded.usage.output_tokens;
                return Ok((decoded, endpoint, total_output_tokens));
            }

            // Anthropic leaves server calls pending when the same assistant
            // turn also contains client tool calls. The client returns those
            // results and replays these server_tool_use blocks next turn.
            if !decoded.tools.is_empty() || output_exhausted {
                for search_use in &search_uses {
                    let mut trace = catalog.pending_trace(search_use);
                    trace.id = format!("srvtoolu_{}", Uuid::new_v4().simple());
                    accumulated_server_events.push(ClaudeServerEvent::ToolSearch {
                        index: accumulated_searches.len(),
                        preceding_text: std::mem::take(&mut preceding_text),
                    });
                    accumulated_searches.push(trace);
                }
                for search_use in &parallel_web_uses {
                    accumulated_server_events.push(ClaudeServerEvent::WebSearch {
                        index: accumulated_web_searches.len(),
                        preceding_text: std::mem::take(&mut preceding_text),
                    });
                    accumulated_web_searches.push(ClaudeWebSearchTrace::pending(
                        format!("srvtoolu_{}", Uuid::new_v4().simple()),
                        search_use.input.clone(),
                    ));
                }
                accumulated_reasoning.push_str(&decoded.reasoning);
                decoded.reasoning = accumulated_reasoning;
                decoded.tool_searches = accumulated_searches;
                decoded.web_searches = accumulated_web_searches;
                decoded.claude_server_events = accumulated_server_events;
                if output_exhausted && decoded.tools.is_empty() {
                    decoded.stop_reason = Some("max_tokens".into());
                }
                merge_round_usage(&mut decoded.usage, &accumulated_usage);
                let total_output_tokens = decoded.usage.output_tokens;
                return Ok((decoded, endpoint, total_output_tokens));
            }

            let mut budget = remaining_tool_search_budget(state, &payload, compact)
                .await
                .map_err(|message| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message,
                    })
                })?;
            let mut loaded_names = loaded_tool_names(&payload);
            let mut searches = Vec::with_capacity(search_uses.len());
            for search_use in search_uses {
                let mut outcome = if tool_search_operations >= max_tool_search_operations {
                    catalog.unavailable_outcome(
                        &search_use,
                        format!(
                            "Tool Search operation limit of {max_tool_search_operations} was reached"
                        ),
                    )
                } else {
                    tool_search_operations += 1;
                    match catalog
                        .search_with_budget_excluding_async(
                            search_use.clone(),
                            budget,
                            loaded_names.clone(),
                        )
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => catalog.unavailable_outcome(&search_use, error),
                    }
                };
                outcome.trace.id = format!("srvtoolu_{}", Uuid::new_v4().simple());
                let consumed_bytes = outcome
                    .tools
                    .iter()
                    .filter_map(|tool| serde_json::to_vec(tool).ok())
                    .map(|tool| tool.len())
                    .sum::<usize>()
                    .saturating_add(outcome.documentation.iter().map(String::len).sum());
                budget = ClaudeToolSearchBudget {
                    max_tools: budget.max_tools.saturating_sub(outcome.tools.len()),
                    max_bytes: budget.max_bytes.saturating_sub(consumed_bytes),
                };
                loaded_names.extend(
                    outcome
                        .tools
                        .iter()
                        .map(|tool| tool.tool_specification.name.clone()),
                );
                accumulated_server_events.push(ClaudeServerEvent::ToolSearch {
                    index: accumulated_searches.len(),
                    preceding_text: std::mem::take(&mut preceding_text),
                });
                accumulated_searches.push(outcome.trace.clone());
                searches.push((search_use, outcome));
            }
            let mut parallel_web_searches = Vec::with_capacity(parallel_web_uses.len());
            for search_use in parallel_web_uses {
                let query = search_use
                    .input
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let server_id = format!("srvtoolu_{}", Uuid::new_v4().simple());
                let trace = if web_search_round >= web_search_max_rounds {
                    ClaudeWebSearchTrace::error(
                        server_id,
                        &query,
                        if web_search_client_limit {
                            "max_uses_exceeded"
                        } else {
                            "unavailable"
                        },
                        format!("web search is limited to {web_search_max_rounds} uses"),
                    )
                } else if query.is_empty() {
                    ClaudeWebSearchTrace::error(
                        server_id,
                        &query,
                        "invalid_tool_input",
                        "web search query must not be empty".into(),
                    )
                } else {
                    web_search_round += 1;
                    match execute_kiro_web_search(state, lease, &query).await {
                        Ok(results) => ClaudeWebSearchTrace::success(server_id, &query, results),
                        Err(error) => ClaudeWebSearchTrace::error(
                            server_id,
                            &query,
                            web_search_error_code(&error),
                            sanitize_error_message(&error.to_string()),
                        )
                        .executed(),
                    }
                };
                accumulated_server_events.push(ClaudeServerEvent::WebSearch {
                    index: accumulated_web_searches.len(),
                    preceding_text: std::mem::take(&mut preceding_text),
                });
                accumulated_web_searches.push(trace.clone());
                parallel_web_searches.push((search_use, trace));
            }
            let documentation_tokens = state
                .tokenizer
                .count(
                    searches
                        .iter()
                        .flat_map(|(_, outcome)| outcome.documentation.iter())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                )
                .await
                .map_err(|message| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message,
                    })
                })? as u64;
            tracing::info!(
                trace_id,
                account_id = %lease.account_id(),
                endpoint,
                search_round = search_round + 1,
                search_count = searches.len(),
                matched_tools = searches.iter().map(|(_, outcome)| outcome.trace.references.len()).sum::<usize>(),
                result_truncated = searches.iter().any(|(_, outcome)| outcome.trace.budget_truncated),
                search_error = searches.iter().any(|(_, outcome)| outcome.trace.error.is_some()),
                "proxy Tool Search batch executed"
            );
            accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
            payload = tool_search_continue_payload_batch(&payload, &round_text, &searches);
            if !parallel_web_searches.is_empty() {
                if let Some(assistant) = payload
                    .conversation_state
                    .history
                    .last_mut()
                    .and_then(|message| message.assistant_response_message.as_mut())
                {
                    assistant.tool_uses.extend(
                        parallel_web_searches
                            .iter()
                            .map(|(tool_use, _)| tool_use.clone()),
                    );
                }
                for (tool_use, trace) in &parallel_web_searches {
                    resume_web_search_payload(&mut payload, tool_use, trace);
                }
            }
            merge_round_usage(&mut accumulated_usage, &decoded.usage);
            decoded = DecodedResponse::default();
            let budget_available = apply_remaining_output_budget(
                &mut payload,
                max_output_tokens,
                accumulated_usage.output_tokens,
            );
            debug_assert!(budget_available);
            let next_input_tokens = state
                .tokenizer
                .estimate_kiro_payload(&payload)
                .await
                .map_err(|message| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message,
                    })
                })? as u64;
            let next_tool_tokens = (state
                .tokenizer
                .estimate_kiro_tools(&payload)
                .await
                .map_err(|message| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message,
                    })
                })? as u64)
                .saturating_add(documentation_tokens);
            let next_payload_bytes = serde_json::to_vec(&payload)
                .map_err(|error| {
                    ExecuteError::Upstream(KiroError {
                        status: None,
                        endpoint: endpoint.clone(),
                        message: error.to_string(),
                    })
                })?
                .len();
            let next_loaded_tools = loaded_tool_count(&payload);
            let max_loaded_tools = config
                .context
                .max_loaded_tools
                .min(kproxy_translate::validate::MAX_TOOLS);
            if next_loaded_tools > max_loaded_tools {
                return Err(ExecuteError::Upstream(KiroError {
                    status: Some(400),
                    endpoint,
                    message: format!(
                        "too many loaded tools after Tool Search: {next_loaded_tools} > {max_loaded_tools}"
                    ),
                }));
            }
            if next_tool_tokens > u64::from(config.context.max_tool_input_tokens) {
                return Err(ExecuteError::Upstream(KiroError {
                    status: Some(400),
                    endpoint,
                    message: format!(
                        "loaded tool definitions are too large after Tool Search: {next_tool_tokens} estimated tokens > {}",
                        config.context.max_tool_input_tokens
                    ),
                }));
            }
            if next_payload_bytes > config.context.max_upstream_payload_bytes {
                return Err(ExecuteError::Upstream(KiroError {
                    status: Some(400),
                    endpoint,
                    message: format!(
                        "translated upstream payload is too large after Tool Search: {next_payload_bytes} bytes > {}",
                        config.context.max_upstream_payload_bytes
                    ),
                }));
            }
            let model = payload
                .conversation_state
                .current_message
                .user_input_message
                .model_id
                .clone();
            check_context_limit(state, next_input_tokens, compact, &model)
                .map_err(ExecuteError::ContextLimit)?;
            tracing::debug!(
                trace_id,
                input_tokens = next_input_tokens,
                tool_tokens = next_tool_tokens,
                payload_bytes = next_payload_bytes,
                loaded_tool_count = next_loaded_tools,
                "Tool Search continuation prepared"
            );
            if let Err(error) = reservation.extend(estimated_credits) {
                if matches!(error, MeterError::DailyLimitExceeded) {
                    notify_quota_degradation(
                        state,
                        "The service daily credit limit was reached during Tool Search",
                    );
                }
                return Err(ExecuteError::Meter(error));
            }
            let account = lease.account().await;
            upstream = state
                .generate(&account, &payload)
                .await
                .map_err(ExecuteError::Upstream)?;
            search_round += 1;
            continue;
        }

        let web_search_keys = if web_search_max_rounds > 0 {
            decoded
                .tools
                .iter()
                .filter(|(_, tool)| tool.name == "web_search")
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        if !web_search_keys.is_empty() {
            let search_uses = web_search_keys
                .into_iter()
                .map(|key| {
                    let search = decoded
                        .tools
                        .remove(&key)
                        .expect("web search buffer exists");
                    KiroToolUse {
                        tool_use_id: search.id,
                        name: search.name,
                        input: super::response::repair_json(&search.input),
                    }
                })
                .collect::<Vec<_>>();
            let round_text = std::mem::take(&mut decoded.text);
            let mut preceding_text = std::mem::take(&mut accumulated_text);
            preceding_text.push_str(&round_text);

            // Preserve every server call, but leave it unresolved until the
            // client tool results from this mixed turn are returned.
            if !decoded.tools.is_empty() || output_exhausted {
                for search_use in &search_uses {
                    accumulated_server_events.push(ClaudeServerEvent::WebSearch {
                        index: accumulated_web_searches.len(),
                        preceding_text: std::mem::take(&mut preceding_text),
                    });
                    accumulated_web_searches.push(ClaudeWebSearchTrace::pending(
                        format!("srvtoolu_{}", Uuid::new_v4().simple()),
                        search_use.input.clone(),
                    ));
                }
                accumulated_reasoning.push_str(&decoded.reasoning);
                decoded.reasoning = accumulated_reasoning;
                decoded.tool_searches = accumulated_searches;
                decoded.web_searches = accumulated_web_searches;
                decoded.claude_server_events = accumulated_server_events;
                if output_exhausted && decoded.tools.is_empty() {
                    decoded.stop_reason = Some("max_tokens".into());
                }
                merge_round_usage(&mut decoded.usage, &accumulated_usage);
                let total_output_tokens = decoded.usage.output_tokens;
                return Ok((decoded, endpoint, total_output_tokens));
            }

            let mut searches = Vec::with_capacity(search_uses.len());
            for search_use in search_uses {
                let query = search_use
                    .input
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let server_id = format!("srvtoolu_{}", Uuid::new_v4().simple());
                let trace = if web_search_round >= web_search_max_rounds {
                    ClaudeWebSearchTrace::error(
                        server_id,
                        &query,
                        if web_search_client_limit {
                            "max_uses_exceeded"
                        } else {
                            "unavailable"
                        },
                        if web_search_client_limit {
                            format!("web search is limited to max_uses={web_search_max_rounds}")
                        } else {
                            format!(
                                "web search reached the proxy safety limit of {web_search_max_rounds} uses"
                            )
                        },
                    )
                } else if query.is_empty() {
                    ClaudeWebSearchTrace::error(
                        server_id,
                        &query,
                        "invalid_tool_input",
                        "web search query must not be empty".into(),
                    )
                } else {
                    web_search_round += 1;
                    match execute_kiro_web_search(state, lease, &query).await {
                        Ok(results) => ClaudeWebSearchTrace::success(server_id, &query, results),
                        Err(error) => ClaudeWebSearchTrace::error(
                            server_id,
                            &query,
                            web_search_error_code(&error),
                            sanitize_error_message(&error.to_string()),
                        )
                        .executed(),
                    }
                };
                accumulated_server_events.push(ClaudeServerEvent::WebSearch {
                    index: accumulated_web_searches.len(),
                    preceding_text: std::mem::take(&mut preceding_text),
                });
                accumulated_web_searches.push(trace.clone());
                searches.push((search_use, trace));
            }
            tracing::info!(
                trace_id,
                account_id = %lease.account_id(),
                endpoint,
                web_search_round,
                search_count = searches.len(),
                result_count = searches.iter().map(|(_, trace)| trace.results.len()).sum::<usize>(),
                search_error = searches.iter().any(|(_, trace)| trace.error.is_some()),
                "proxy Kiro MCP web search batch executed"
            );
            accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
            payload = web_search_continue_payload_batch(&payload, &round_text, &searches);
            merge_round_usage(&mut accumulated_usage, &decoded.usage);
            decoded = DecodedResponse::default();
            let budget_available = apply_remaining_output_budget(
                &mut payload,
                max_output_tokens,
                accumulated_usage.output_tokens,
            );
            debug_assert!(budget_available);
            validate_internal_continuation(
                state,
                &payload,
                compact,
                &endpoint,
                "Web Search",
                tool_search.is_some(),
            )
            .await
            .map_err(ExecuteError::Upstream)?;
            if let Err(error) = reservation.extend(estimated_credits) {
                if matches!(error, MeterError::DailyLimitExceeded) {
                    notify_quota_degradation(
                        state,
                        "The service daily credit limit was reached during Web Search",
                    );
                }
                return Err(ExecuteError::Meter(error));
            }
            let account = lease.account().await;
            upstream = state
                .generate(&account, &payload)
                .await
                .map_err(ExecuteError::Upstream)?;
            continue;
        }

        if client_has_tools
            || decoded.tools.is_empty()
            || output_exhausted
            || round >= config.features.auto_continue_rounds.min(30)
        {
            accumulated_text.push_str(&decoded.text);
            accumulated_reasoning.push_str(&decoded.reasoning);
            decoded.text = accumulated_text;
            decoded.reasoning = accumulated_reasoning;
            decoded.tool_searches = accumulated_searches;
            decoded.web_searches = accumulated_web_searches;
            decoded.claude_server_events = accumulated_server_events;
            if output_exhausted && decoded.tools.is_empty() {
                decoded.stop_reason = Some("max_tokens".into());
            }
            merge_round_usage(&mut decoded.usage, &accumulated_usage);
            let total_output_tokens = decoded.usage.output_tokens;
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
            return Ok((decoded, endpoint, total_output_tokens));
        }
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
        let budget_available = apply_remaining_output_budget(
            &mut payload,
            max_output_tokens,
            accumulated_usage.output_tokens,
        );
        debug_assert!(budget_available);
        if let Err(error) = reservation.extend(estimated_credits) {
            if matches!(error, MeterError::DailyLimitExceeded) {
                notify_quota_degradation(
                    state,
                    "The service daily credit limit was reached during auto-continuation",
                );
            }
            return Err(ExecuteError::Meter(error));
        }
        let account = lease.account().await;
        upstream = state
            .generate(&account, &payload)
            .await
            .map_err(ExecuteError::Upstream)?;
        round += 1;
    }
}

pub(super) fn web_search_error_code(error: &KiroError) -> &'static str {
    match error.status {
        Some(429) => "too_many_requests",
        Some(413) => "request_too_large",
        Some(400) if error.message.to_ascii_lowercase().contains("too long") => "query_too_long",
        Some(400) => "invalid_tool_input",
        _ => "unavailable",
    }
}

/// Applies the remaining client output budget to the next internal Kiro
/// continuation. Returns false when no model output may be generated.
pub(super) fn apply_remaining_output_budget(
    payload: &mut kproxy_translate::KiroPayload,
    maximum: u32,
    used: u64,
) -> bool {
    let remaining = u64::from(maximum).saturating_sub(used);
    if remaining == 0 {
        return false;
    }
    if let Some(inference) = payload.inference_config.as_mut() {
        inference.max_tokens = remaining.min(u64::from(u32::MAX)) as u32;
    }
    true
}

pub(super) async fn execute_kiro_web_search(
    state: &Arc<AppState>,
    lease: &AccountLease,
    query: &str,
) -> Result<kproxy_translate::WebSearchResults, KiroError> {
    let account_id = lease.account().await.id;
    match execute_kiro_web_search_once(state, lease, query).await {
        Err(error) if error.is_auth() => {
            state
                .refresh_account_token(&state.pool(), &account_id, true)
                .await
                .map_err(|refresh| KiroError {
                    status: error.status,
                    endpoint: "MCP web_search".into(),
                    message: format!("web search authentication refresh failed: {refresh}"),
                })?;
            persist_refreshed_accounts(state)
                .await
                .map_err(|_| KiroError {
                    status: None,
                    endpoint: "MCP web_search".into(),
                    message: "failed to persist refreshed web search token".into(),
                })?;
            execute_kiro_web_search_once(state, lease, query).await
        }
        result => result,
    }
}

async fn execute_kiro_web_search_once(
    state: &Arc<AppState>,
    lease: &AccountLease,
    query: &str,
) -> Result<kproxy_translate::WebSearchResults, KiroError> {
    let account = ensure_web_search_profile_arn(state, lease).await?;
    state.kiro().web_search(&account, query).await
}

async fn ensure_web_search_profile_arn(
    state: &Arc<AppState>,
    lease: &AccountLease,
) -> Result<kproxy_core::account::Account, KiroError> {
    for _attempt in 0..2 {
        let account = lease.account().await;
        if account
            .profile_arn
            .as_deref()
            .is_some_and(|profile_arn| !profile_arn.trim().is_empty())
        {
            return Ok(account);
        }

        let profile_arn = state.kiro().resolve_profile_arn(&account).await?;
        let pool = state.pool();
        let account_state = pool.get(&account.id).await.ok_or_else(|| KiroError {
            status: None,
            endpoint: "MCP web_search".into(),
            message: "web search account disappeared during profile discovery".into(),
        })?;
        let mut current = account_state.account.write().await;
        if current
            .profile_arn
            .as_deref()
            .is_some_and(|existing| !existing.trim().is_empty())
        {
            return Ok(current.clone());
        }
        if current.credentials.access_token != account.credentials.access_token {
            continue;
        }
        current.profile_arn = Some(profile_arn);
        let resolved = current.clone();
        drop(current);

        crate::tasks::persist_pool_accounts(state)
            .await
            .map_err(|error| KiroError {
                status: None,
                endpoint: "credential-store".into(),
                message: format!("resolved profile ARN could not be persisted: {error}"),
            })?;
        return Ok(resolved);
    }

    Err(KiroError {
        status: None,
        endpoint: "MCP web_search".into(),
        message: "account credentials changed repeatedly during profile discovery".into(),
    })
}

pub(super) async fn validate_internal_continuation(
    state: &Arc<AppState>,
    payload: &kproxy_translate::KiroPayload,
    compact: bool,
    endpoint: &str,
    stage: &str,
    enforce_tool_search_budget: bool,
) -> Result<u64, KiroError> {
    let config = state.config.current();
    let input_tokens = state
        .tokenizer
        .estimate_kiro_payload(payload)
        .await
        .map_err(|message| KiroError {
            status: None,
            endpoint: endpoint.into(),
            message,
        })? as u64;
    let tool_tokens = state
        .tokenizer
        .estimate_kiro_tools(payload)
        .await
        .map_err(|message| KiroError {
            status: None,
            endpoint: endpoint.into(),
            message,
        })? as u64;
    let payload_bytes = serde_json::to_vec(payload)
        .map_err(|error| KiroError {
            status: None,
            endpoint: endpoint.into(),
            message: error.to_string(),
        })?
        .len();
    let loaded_tools = loaded_tool_count(payload);
    let max_loaded_tools = config
        .context
        .max_loaded_tools
        .min(kproxy_translate::validate::MAX_TOOLS);
    let budget_message = if loaded_tools > max_loaded_tools {
        Some(format!(
            "too many loaded tools after {stage}: {loaded_tools} > {max_loaded_tools}"
        ))
    } else if enforce_tool_search_budget
        && tool_tokens > u64::from(config.context.max_tool_input_tokens)
    {
        Some(format!(
            "loaded Tool Search working set is too large after {stage}: {tool_tokens} tokens > {}",
            config.context.max_tool_input_tokens
        ))
    } else if payload_bytes > config.context.max_upstream_payload_bytes {
        Some(format!(
            "translated payload is too large after {stage}: {payload_bytes} bytes > {}",
            config.context.max_upstream_payload_bytes
        ))
    } else {
        None
    };
    if let Some(message) = budget_message {
        return Err(KiroError {
            status: Some(400),
            endpoint: endpoint.into(),
            message,
        });
    }
    let model = &payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id;
    check_context_limit(state, input_tokens, compact, model).map_err(|limit| KiroError {
        status: Some(400),
        endpoint: endpoint.into(),
        message: format!(
            "prompt is too long after {stage}: {} tokens > {}",
            limit.input_tokens, limit.maximum
        ),
    })?;
    Ok(input_tokens)
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
    compact: bool,
    estimated_credits: f64,
    started: Instant,
    prompt_cache: Option<PromptCacheProfile>,
    compaction_summary: Option<String>,
    tool_search: Option<Arc<ClaudeToolSearchCatalog>>,
    max_tool_search_operations: u32,
    tool_search_operations: u32,
    web_search_max_rounds: u32,
    web_search_client_limit: bool,
    resumed_tool_searches: Vec<kproxy_translate::ClaudeToolSearchTrace>,
    resumed_web_searches: Vec<ClaudeWebSearchTrace>,
    resumed_server_events: Vec<ClaudeServerEvent>,
    diagnostics: RequestDiagnostics,
    web_tool_names: std::collections::HashMap<String, String>,
) -> Result<Response, ApiError> {
    let (mut decoded, endpoint, current_round_output_tokens) = collect_nonstream_rounds(
        &state,
        &trace_id,
        &lease,
        &mut reservation,
        estimated_credits,
        upstream,
        payload,
        compact,
        request.max_tokens,
        tool_search.as_ref(),
        max_tool_search_operations,
        tool_search_operations,
        web_search_max_rounds,
        web_search_client_limit,
        resumed_tool_searches,
        resumed_web_searches,
        resumed_server_events,
    )
    .await
    .map_err(|error| upstream_error(error, ErrorFormat::Claude))?;
    decoded.restore_tool_names(&web_tool_names);
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
        diagnostics,
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
        &state.web_search_replay,
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
    diagnostics: RequestDiagnostics,
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
        false,
        max_tokens,
        None,
        0,
        0,
        0,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
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
        diagnostics,
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
    mut diagnostics: RequestDiagnostics,
    decoded: &DecodedResponse,
    credits: f64,
) -> RequestLog {
    diagnostics.tool_search_rounds = decoded.tool_searches.len();
    diagnostics.tool_search_matches = decoded
        .tool_searches
        .iter()
        .map(|search| search.matched_count)
        .sum();
    diagnostics.search_requested_limit = decoded
        .tool_searches
        .iter()
        .map(|search| search.requested_limit)
        .max()
        .unwrap_or_default();
    diagnostics.search_returned_count = decoded
        .tool_searches
        .iter()
        .map(|search| search.references.len())
        .sum();
    diagnostics.search_budget_truncated = decoded
        .tool_searches
        .iter()
        .any(|search| search.budget_truncated);
    diagnostics.web_search_rounds = decoded.web_searches.len();
    diagnostics.web_search_results = decoded
        .web_searches
        .iter()
        .map(|search| search.results.len())
        .sum();
    diagnostics.client_status = 200;
    diagnostics.upstream_status = Some(200);
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
        diagnostics,
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
        let mut original_request = request.clone();
        let has_context_edits = has_context_management_edits(request.context_management.as_ref());
        let boundary_applied = apply_compaction_boundary(&mut request);
        validate_claude(&request).map_err(claude_validation_error)?;
        let context_edit_stats = apply_context_management_edits(&mut request);
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
            original_request.tools.clear();
            original_request.tool_choice = None;
        } else if !config.features.enable_web_tools {
            request.tools.retain(|tool| {
                !tool.r#type.as_deref().is_some_and(|kind| {
                    matches_type_family(kind, "web_search")
                        || matches_type_family(kind, "web_fetch")
                })
            });
            original_request.tools.retain(|tool| {
                !tool.r#type.as_deref().is_some_and(|kind| {
                    matches_type_family(kind, "web_search")
                        || matches_type_family(kind, "web_fetch")
                })
            });
        }
        if !config.features.enable_tool_search
            && request.tools.iter().any(|tool| {
                tool.defer_loading
                    || tool
                        .r#type
                        .as_deref()
                        .is_some_and(kproxy_translate::is_tool_search_type)
            })
        {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Anthropic Tool Search is disabled by proxy configuration",
                ErrorFormat::Claude,
            ));
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
        let mut original_payload = claude_to_kiro(&original_request, &normal);
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
        let input_tokens = if boundary_applied || context_edit_stats.changed() {
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
        if has_context_edits {
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
            cleared_tool_results = context_edit_stats.cleared_tool_results,
            cleared_tool_inputs = context_edit_stats.cleared_tool_inputs,
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
    let mut names = kproxy_translate::claude_tool_name_map(request);
    names.extend(request.tools.iter().filter_map(|tool| {
        let kind = tool.r#type.as_deref()?;
        if matches_type_family(kind, "web_search") {
            Some(("web_search".into(), kind.into()))
        } else if matches_type_family(kind, "web_fetch") {
            Some(("web_fetch".into(), kind.into()))
        } else {
            None
        }
    }));
    names
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

fn serialized_payload_bytes(
    payload: &kproxy_translate::KiroPayload,
    format: ErrorFormat,
) -> Result<usize, ApiError> {
    serde_json::to_vec(payload)
        .map(|payload| payload.len())
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to serialize upstream request: {error}"),
                format,
            )
        })
}

fn claude_validation_error(error: ValidationError) -> ApiError {
    // Claude Code reserves HTTP 413/request_too_large for its 32 MiB request
    // body limit and replaces the server message with a generic attachment
    // warning. Tool counts, schemas, and proxy working-set budgets are
    // semantic request validation errors, so keep their actionable messages
    // visible with HTTP 400.
    ApiError::new(
        StatusCode::BAD_REQUEST,
        error.to_string(),
        ErrorFormat::Claude,
    )
}

fn loaded_tool_count(payload: &kproxy_translate::KiroPayload) -> usize {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .map_or(0, |context| context.tools.len())
}

fn loaded_tool_bytes(payload: &kproxy_translate::KiroPayload) -> usize {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .and_then(|context| serde_json::to_vec(&context.tools).ok())
        .map_or(0, |tools| tools.len())
}

pub(super) fn loaded_tool_names(payload: &kproxy_translate::KiroPayload) -> HashSet<String> {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .into_iter()
        .flat_map(|context| context.tools.iter())
        .map(|tool| tool.tool_specification.name.clone())
        .collect()
}

pub(super) async fn remaining_tool_search_budget(
    state: &Arc<AppState>,
    payload: &kproxy_translate::KiroPayload,
    compact: bool,
) -> Result<kproxy_translate::ClaudeToolSearchBudget, String> {
    let config = state.config.current();
    let current_tools = loaded_tool_count(payload);
    let current_tool_tokens = state.tokenizer.estimate_kiro_tools(payload).await? as u64;
    let current_input_tokens = state.tokenizer.estimate_kiro_payload(payload).await? as u64;
    let current_payload_bytes = serde_json::to_vec(payload)
        .map_err(|error| format!("failed to serialize Tool Search payload: {error}"))?
        .len();
    let model = &payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id;

    // JSON/schema-heavy text generally tokenizes below three bytes per token.
    // Using three here is deliberately conservative; the exact translated
    // request is checked again before dispatch.
    let token_bytes = u64::from(config.context.max_tool_input_tokens)
        .saturating_sub(current_tool_tokens)
        .saturating_mul(3);
    let context_bytes = context_maximum(state, compact, model)
        .saturating_sub(current_input_tokens)
        .saturating_mul(3);
    let payload_bytes = config
        .context
        .max_upstream_payload_bytes
        .saturating_sub(current_payload_bytes) as u64;
    Ok(kproxy_translate::ClaudeToolSearchBudget {
        max_tools: config
            .context
            .max_loaded_tools
            .min(kproxy_translate::validate::MAX_TOOLS)
            .saturating_sub(current_tools),
        max_bytes: usize::try_from(token_bytes.min(context_bytes).min(payload_bytes))
            .unwrap_or(usize::MAX),
    })
}

fn enforce_payload_budget(
    state: &Arc<AppState>,
    tool_tokens: u64,
    payload_bytes: usize,
    loaded_tools: usize,
    enforce_tool_search_budget: bool,
    format: ErrorFormat,
) -> Result<(), ApiError> {
    let context = &state.config.current().context;
    enforce_payload_budget_limits(
        context,
        tool_tokens,
        payload_bytes,
        loaded_tools,
        enforce_tool_search_budget,
        format,
    )
}

fn enforce_payload_budget_limits(
    context: &kproxy_core::config::ContextConfig,
    tool_tokens: u64,
    payload_bytes: usize,
    loaded_tools: usize,
    enforce_tool_search_budget: bool,
    format: ErrorFormat,
) -> Result<(), ApiError> {
    let budget_status = if format == ErrorFormat::Claude {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::PAYLOAD_TOO_LARGE
    };
    let max_loaded_tools = context
        .max_loaded_tools
        .min(kproxy_translate::validate::MAX_TOOLS);
    if loaded_tools > max_loaded_tools {
        return Err(ApiError::new(
            budget_status,
            format!(
                "too many loaded tools: {loaded_tools} > {max_loaded_tools}; defer more tools or reduce Tool Search references"
            ),
            format,
        ));
    }
    // `max_tool_input_tokens` is a working-set guard for deferred Tool Search.
    // Applying it to an ordinary request creates a second, much smaller
    // context limit even though these tool definitions are already included
    // in `estimate_kiro_payload` and checked against the model input window.
    if enforce_tool_search_budget && tool_tokens > u64::from(context.max_tool_input_tokens) {
        return Err(ApiError::new(
            budget_status,
            format!(
                "loaded Tool Search working set is too large: {tool_tokens} estimated tokens > {}; reduce always-loaded tools or their schemas",
                context.max_tool_input_tokens
            ),
            format,
        ));
    }
    if payload_bytes > context.max_upstream_payload_bytes {
        return Err(ApiError::new(
            budget_status,
            format!(
                "translated upstream payload is too large: {payload_bytes} bytes > {}; reduce the conversation or loaded tool schemas",
                context.max_upstream_payload_bytes
            ),
            format,
        ));
    }
    Ok(())
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
            notify_quota_degradation(
                state,
                "The configured service daily credit limit has been reached",
            );
            Err(meter_error(MeterError::DailyLimitExceeded, format))
        }
        Err(error) => Err(meter_error(error, format)),
    }
}

pub(super) fn notify_quota_degradation(state: &Arc<AppState>, reason: &str) {
    state.notifier().emit(WebhookEvent::new(
        WebhookEventKind::QuotaExhausted,
        "Proxy credit quota exhausted",
        reason,
    ));
    state.notifier().emit(WebhookEvent::new(
        WebhookEventKind::ServiceDegraded,
        "Proxy service quota degraded",
        "Quota-bound requests are being rejected while the daemon and administration plane remain available",
    ));
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
            "Credit quota exhausted; no compatible account currently has usable credit",
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
    let upstream_status = error.status;
    let upstream_throttle = error.is_throttle();
    let request_rejection = error.is_request_rejection();
    let account_error = !request_rejection
        && (error.is_auth()
            || error.is_quota()
            || error.is_throttle()
            || matches!(error.status, Some(500..=599)));
    let status = match error.status {
        // An upstream 413 describes Kiro's translated payload, not necessarily
        // a Claude request body over 32 MiB. Preserve the upstream status in
        // diagnostics, but use 400 so Claude Code displays the real message.
        Some(413) if format == ErrorFormat::Claude => StatusCode::BAD_REQUEST,
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
    output.upstream_status = upstream_status;
    output.error_stage = "upstream_dispatch";
    output.account_error = account_error;
    if upstream_status == Some(429) || upstream_throttle {
        output.error_code = "upstream_rate_limited";
    } else if upstream_status == Some(413) {
        output.error_code = "request_payload_exceeded";
    } else if !request_rejection {
        output.error_code = "upstream_unavailable";
    }
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
        "too many tools",
        "too many loaded tools",
        "tool definitions",
        "tool schema",
        "tool search working set",
        "loaded tools are too large",
        "payload too large",
        "payload is too large",
        "request too large",
        "request entity too large",
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
    error_code: &'static str,
    error_stage: &'static str,
    upstream_status: Option<u16>,
    account_error: bool,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>, format: ErrorFormat) -> Self {
        let message = message.into();
        let (error_code, error_stage) = classify_api_error(status, &message);
        Self {
            status,
            message,
            format,
            allow: None,
            authenticate: false,
            retry_after: false,
            suppress_model_stats: false,
            request_id: None,
            log_context: Box::default(),
            error_code,
            error_stage,
            upstream_status: None,
            account_error: false,
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
        for (name, value) in [
            ("x-kproxy-error-code", self.error_code),
            ("x-kproxy-error-stage", self.error_stage),
            (
                "x-kproxy-account-error",
                if self.account_error { "true" } else { "false" },
            ),
        ] {
            if let (Ok(name), Ok(value)) = (
                header::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        if let Some(status) = self.upstream_status {
            if let Ok(value) = HeaderValue::from_str(&status.to_string()) {
                response.headers_mut().insert(
                    header::HeaderName::from_static("x-kproxy-upstream-status"),
                    value,
                );
            }
        }
        response
    }
}

fn classify_api_error(status: StatusCode, message: &str) -> (&'static str, &'static str) {
    let lower = message.to_ascii_lowercase();
    if lower.contains("prompt is too long") || lower.contains("context length") {
        return ("context_length_exceeded", "context_validation");
    }
    if (lower.contains("not supported") || lower.contains("unsupported"))
        && (lower.contains("tool")
            || lower.contains("strict")
            || lower.contains("allowed_callers")
            || lower.contains("eager"))
    {
        return ("unsupported_tool_protocol", "request_validation");
    }
    if status == StatusCode::PAYLOAD_TOO_LARGE && lower.contains("request body") {
        return ("request_body_too_large", "request_body");
    }
    let capacity_error =
        lower.contains("too many") || lower.contains("too large") || lower.contains("exceed");
    if capacity_error && (lower.contains("catalog") || lower.contains("deferred tool")) {
        return ("tool_catalog_too_large", "request_budget");
    }
    if is_tool_budget_error(&lower) {
        return ("tool_budget_exceeded", "request_budget");
    }
    if is_payload_budget_error(&lower) {
        return ("request_payload_exceeded", "request_budget");
    }
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        return ("request_payload_exceeded", "request_budget");
    }
    if status == StatusCode::BAD_REQUEST && lower.contains("tool") {
        return ("invalid_tool_protocol", "request_validation");
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ("upstream_rate_limited", "upstream_dispatch");
    }
    if matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE
    ) {
        return ("upstream_unavailable", "upstream_dispatch");
    }
    ("invalid_request", "request_validation")
}

fn is_tool_budget_error(lower: &str) -> bool {
    lower.contains("too many tools")
        || lower.contains("too many loaded tools")
        || lower.contains("tool definitions are too large")
        || lower.contains("tool definitions exceed")
        || lower.contains("tool definition exceeds")
        || lower.contains("tool documentation exceeds")
        || lower.contains("tool search working set is too large")
        || lower.contains("loaded tools are too large")
        || lower.contains("tool schema payload too large")
}

fn is_payload_budget_error(lower: &str) -> bool {
    lower.contains("payload too large")
        || lower.contains("payload is too large")
        || lower.contains("request too large")
        || lower.contains("request entity too large")
}

#[cfg(test)]
mod model_tests {
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn quota_degradation_does_not_cancel_the_daemon() {
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

        notify_quota_degradation(
            &state,
            "All compatible Kiro accounts are below the protection threshold",
        );

        let shutdown = state.shutdown.clone();
        assert!(
            tokio::time::timeout(Duration::from_millis(1_200), shutdown.cancelled())
                .await
                .is_err(),
            "quota degradation must not stop the daemon or administration plane"
        );
    }

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
                max_input_tokens: Some(1_000_000),
                max_output_tokens: Some(16_384),
            }),
        }]);

        assert_eq!(
            model_token_limit(&state, "claude-4.6-sonnet", true),
            Some(1_000_000)
        );
        assert!(check_context_limit(&state, 900_000, false, "claude-4.6-sonnet").is_ok());
        assert!(check_context_limit(&state, 960_000, false, "claude-4.6-sonnet").is_err());
    }

    #[tokio::test]
    async fn resolved_web_search_profile_is_saved_to_the_account_store() {
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
        let accounts_file = paths.accounts_file.clone();
        let mut accounts = kproxy_store::accounts::AccountStore::load(&accounts_file)
            .await
            .expect("accounts");
        accounts
            .insert(kproxy_core::account::Account {
                id: "acc_profile".into(),
                email: "profile@example.com".into(),
                label: None,
                enabled: true,
                machine_id: "a".repeat(64),
                profile_arn: None,
                credentials: kproxy_core::account::Credentials {
                    access_token: "access-token".into(),
                    refresh_token: None,
                    client_id: None,
                    client_secret: None,
                    region: "us-east-1".into(),
                    expires_at: i64::MAX,
                    auth_method: kproxy_core::account::AuthMethod::Social,
                },
                usage: None,
                subscription: None,
                tags: Vec::new(),
                created_at: 0,
                credit_exhausted: false,
            })
            .expect("insert account");
        accounts.save().await.expect("save account");
        let state = Arc::new(AppState::new(
            paths,
            kproxy_store::config_loader::ConfigHandle::new(kproxy_core::config::Config::default()),
            accounts,
        ));
        let lease = state
            .pool()
            .acquire("", 0.0, &[])
            .await
            .expect("account lease");

        let resolved = ensure_web_search_profile_arn(&state, &lease)
            .await
            .expect("resolved account");

        const SOCIAL_PROFILE_ARN: &str =
            "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";
        assert_eq!(resolved.profile_arn.as_deref(), Some(SOCIAL_PROFILE_ARN));
        drop(lease);
        let persisted = kproxy_store::accounts::AccountStore::load(&accounts_file)
            .await
            .expect("persisted accounts");
        assert_eq!(
            persisted
                .find("acc_profile")
                .and_then(|account| account.profile_arn.as_deref()),
            Some(SOCIAL_PROFILE_ARN)
        );
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
    fn upstream_request_rejections_never_poison_an_account() {
        let error = upstream_api_error(
            KiroError {
                status: Some(503),
                endpoint: "test".into(),
                message: "tool schema payload too large".into(),
            },
            RequestLogContext::default(),
            ErrorFormat::Claude,
        );
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.error_code, "tool_budget_exceeded");
        assert!(!error.account_error);
    }

    #[test]
    fn proxy_budget_errors_remain_visible_to_claude_code() {
        let error = ApiError::new(
            StatusCode::BAD_REQUEST,
            "loaded tool definitions are too large",
            ErrorFormat::Claude,
        );
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        let envelope = error_envelope(
            ErrorFormat::Claude,
            error.status.as_u16(),
            &error.message,
            None,
        );
        assert_eq!(envelope["error"]["type"], "invalid_request_error");
        let response = error.with_request_id("trace_test").into_response();
        assert_eq!(
            response.headers()["x-kproxy-error-code"],
            "tool_budget_exceeded"
        );
        assert_eq!(response.headers()["x-kproxy-error-stage"], "request_budget");
        assert_eq!(response.headers()["request-id"], "trace_test");
    }

    #[test]
    fn only_real_inbound_body_limits_use_request_too_large() {
        let error = ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body exceeds the 50 MiB limit",
            ErrorFormat::Claude,
        );
        assert_eq!(error.error_code, "request_body_too_large");
        let envelope = error_envelope(
            ErrorFormat::Claude,
            error.status.as_u16(),
            &error.message,
            None,
        );
        assert_eq!(envelope["error"]["type"], "request_too_large");
    }

    #[test]
    fn upstream_413_preserves_diagnostics_without_triggering_claude_32mb_ui() {
        let error = upstream_api_error(
            KiroError {
                status: Some(413),
                endpoint: "test".into(),
                message: "translated upstream payload is too large".into(),
            },
            RequestLogContext::default(),
            ErrorFormat::Claude,
        );
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.upstream_status, Some(413));
        assert_eq!(error.error_code, "request_payload_exceeded");
    }

    #[test]
    fn ordinary_tools_use_the_model_context_instead_of_the_tool_search_budget() {
        let context = kproxy_core::config::Config::default().context;
        let tool_tokens = 37_239;

        assert!(enforce_payload_budget_limits(
            &context,
            tool_tokens,
            512 * 1024,
            64,
            false,
            ErrorFormat::Claude,
        )
        .is_ok());

        let error = enforce_payload_budget_limits(
            &context,
            tool_tokens,
            512 * 1024,
            64,
            true,
            ErrorFormat::Claude,
        )
        .expect_err("deferred Tool Search working sets must retain their own budget");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("Tool Search working set"));
    }

    #[test]
    fn catalog_capacity_errors_are_invalid_requests_with_stable_codes() {
        for error in [
            ValidationError::TooManyDeferredTools,
            ValidationError::DeferredToolDefinitionsTooLarge,
        ] {
            let error = claude_validation_error(error);
            assert_eq!(error.status, StatusCode::BAD_REQUEST);
            assert_eq!(error.error_code, "tool_catalog_too_large");
        }

        for error in [
            ValidationError::TooManyTools,
            ValidationError::LoadedToolDefinitionsTooLarge,
            ValidationError::ToolDefinitionTooLarge,
            ValidationError::ToolDocumentationTooLarge,
        ] {
            let error = claude_validation_error(error);
            assert_eq!(error.status, StatusCode::BAD_REQUEST);
            assert_eq!(error.error_code, "tool_budget_exceeded");
        }
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
    fn internal_rounds_receive_only_the_remaining_output_budget() {
        let request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4",
            "max_tokens":100,
            "messages":[{"role":"user","content":"use a tool"}],
            "tools":[{"name":"lookup","input_schema":{"type":"object"}}]
        }))
        .expect("request");
        let mut payload = claude_to_kiro(
            &request,
            &TranslationOptions::new("dynamic-sonnet", "AI_EDITOR"),
        );
        assert_eq!(
            payload
                .inference_config
                .as_ref()
                .map(|value| value.max_tokens),
            Some(100)
        );
        assert!(apply_remaining_output_budget(&mut payload, 100, 35));
        assert_eq!(
            payload
                .inference_config
                .as_ref()
                .map(|value| value.max_tokens),
            Some(65)
        );
        assert!(!apply_remaining_output_budget(&mut payload, 100, 100));
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

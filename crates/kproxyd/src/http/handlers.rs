use std::collections::{BTreeSet, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{OriginalUri, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use bytes::BytesMut;
use futures::StreamExt;
use kproxy_kiro::{EventStreamDecoder, KiroError, KiroEvent, KiroResponse};
use kproxy_pool::{AccountLease, PoolError};
use kproxy_translate::model::{map_model, resolve_dynamic_model};
use kproxy_translate::{
    apply_context_management_edits, claude_loaded_tools, claude_pending_server_tool_uses,
    claude_to_kiro, compact_trigger_tokens, compaction_summary_payload, error_envelope,
    estimate_context_management_input_tokens, has_context_management_edits, matches_type_family,
    normalize_compaction_boundary, openai_to_kiro, resume_tool_search_payload,
    resume_web_search_payload, sanitize_error_message, sanitize_kiro_tool_history,
    tool_search_continue_payload_batch, validate_claude, validate_claude_generation,
    validate_kiro_tool_history, validate_openai, web_search_continue_payload_batch,
    ClaudeContextEditStats, ClaudeRequest, ClaudeToolSearchBudget, ClaudeToolSearchCatalog,
    ClaudeWebSearchTrace, ErrorFormat, KiroCompactionPlan, KiroPayload, OpenAiRequest,
    TranslationOptions, ValidationError,
};
use rand::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::meter::{now_secs, CreditReservation, MeterError, UsageRecord};
use crate::state::AppState;
use crate::stats::{RequestDiagnostics, RequestLog, UpstreamAttemptLog};

use super::prompt_cache::PromptCacheProfile;
use super::request_trace_id;
use super::response::{
    ClaudeServerEvent, CompactionIterationUsage, DecodedResponse, OpenAiToolIdentity,
    StopSequenceFilter, ThinkingContentFilter, ToolLeakFilter,
};
use super::stream::{self, StreamContext, StreamProtocol};
use super::usage::{fallback_credits, fill_missing_usage};
use super::ServiceHttpState;

const MAX_STATS_MODEL_CHARS: usize = 128;
const MAX_ATTEMPT_LOG_SUMMARY_CHARS: usize = 4_096;
const UNKNOWN_STATS_MODEL: &str = "unknown";
/// Credit reservation fallback only; never an implicit generation limit.
pub(super) const DEFAULT_OUTPUT_TOKEN_ESTIMATE: u32 = 8_192;
const MIN_COMPACTION_BACKGROUND_GRACE: Duration = Duration::from_millis(250);
const MAX_COMPACTION_BACKGROUND_GRACE: Duration = Duration::from_secs(5);
const COMPACTION_CLEANUP_GRACE: Duration = Duration::from_secs(5);

fn stable_conversation_id(
    headers: &HeaderMap,
    api_key_id: Option<&str>,
    explicit: Option<&str>,
    fallback: Option<&str>,
) -> Option<String> {
    let hint = explicit
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .or_else(|| {
            [
                "x-claude-code-session-id",
                "x-opencode-session",
                "x-session-affinity",
                "x-conversation-id",
            ]
            .into_iter()
            .find_map(|name| {
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::trim)
                    .filter(|hint| !hint.is_empty() && hint.len() <= 256)
            })
        })
        .or_else(|| fallback.map(str::trim).filter(|hint| !hint.is_empty()))?;
    let mut digest = Sha256::new();
    digest.update(b"kiro-proxy-conversation-v1\0");
    digest.update(api_key_id.unwrap_or("anonymous").as_bytes());
    digest.update(b"\0");
    digest.update(hint.as_bytes());
    let digest = digest.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // UUIDv8 reserves the payload bits for application-defined stable IDs.
    // Keeping the standard variant/version layout matches Kiro IDE wire IDs
    // while retaining deterministic, API-key-isolated session affinity.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Some(Uuid::from_bytes(bytes).to_string())
}

fn conversation_fingerprint<T: serde::Serialize>(messages: &[T]) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    // The first client message is immutable when later turns are appended.
    // Hashing the first two made the fallback ID change between turn one and
    // turn two because the first request contains only one message.
    let encoded = serde_json::to_vec(&messages[..1]).ok()?;
    let mut digest = Sha256::new();
    digest.update(b"kiro-proxy-history-v1\0");
    digest.update(encoded);
    Some(
        digest.finalize()[..16]
            .iter()
            .fold(String::new(), |mut output, byte| {
                use std::fmt::Write as _;
                let _ = write!(output, "{byte:02x}");
                output
            }),
    )
}

fn metadata_conversation_hint(metadata: Option<&Value>) -> Option<&str> {
    let metadata = metadata?.as_object()?;
    ["session_id", "conversation_id", "thread_id"]
        .into_iter()
        .find_map(|name| metadata.get(name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|hint| !hint.is_empty() && hint.len() <= 256)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactionReason {
    ClientTrigger,
    MappedWindowOverflow,
    ResolvedWindowOverflow,
    UpstreamWindowOverflow,
}

impl CompactionReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClientTrigger => "client_trigger",
            Self::MappedWindowOverflow => "mapped_overflow",
            Self::ResolvedWindowOverflow => "resolved_overflow",
            Self::UpstreamWindowOverflow => "upstream_overflow",
        }
    }
}

#[derive(Debug, Clone)]
struct CompactionDecision {
    reasons: Vec<CompactionReason>,
    model: String,
    trigger_tokens: u64,
    target_tokens: u64,
    maximum_tokens: u64,
}

impl CompactionDecision {
    fn reason_names(&self) -> String {
        self.reasons
            .iter()
            .map(|reason| reason.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Clone)]
enum CompactionArtifact {
    Semantic {
        source_payload: KiroPayload,
        plan: KiroCompactionPlan,
        summary: String,
        usage: CompactionIterationUsage,
    },
    Extractive {
        source_payload: KiroPayload,
        preserve_recent_turns: usize,
        usage: Option<CompactionIterationUsage>,
    },
}

struct CompactionRun {
    payload: KiroPayload,
    stats: kproxy_translate::ContextCompactionStats,
    artifact: Option<CompactionArtifact>,
    mode: &'static str,
    summary_model: Option<String>,
    summary_input_tokens: Option<u64>,
    fallback_reason: Option<&'static str>,
    iteration_usage: Option<CompactionIterationUsage>,
}

struct CompactionRequest<'a> {
    trace_id: &'a str,
    key_id: Option<&'a str>,
    source_payload: &'a KiroPayload,
    decision: &'a CompactionDecision,
    summary_model: &'a str,
    summary_timeout_ms: u64,
    preserve_recent_turns: usize,
}

mod entrypoints;

use entrypoints::{attempt_diagnostics, read_bounded_body, record_failed_request};
pub use entrypoints::{claude_messages, health, openai_chat, readiness, root};

mod request;

use request::{handle_claude, handle_openai};
#[cfg(test)]
use request::{is_public_address, validate_remote_media_type, RemoteAttachmentKind};

const COMPACTION_USAGE_PATH: &str = "/internal/compact";

struct GeneratedCompactionSummary {
    content: String,
    usage: CompactionIterationUsage,
}

struct CompactionSummaryFailure {
    message: String,
    usage: Option<CompactionIterationUsage>,
}

impl From<String> for CompactionSummaryFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            usage: None,
        }
    }
}

mod compaction;

#[cfg(test)]
use compaction::{
    await_compaction_summary_task, await_compaction_summary_task_with_policy,
    compaction_operation_target, parse_compaction_summary,
};
use compaction::{
    initial_compaction_decision, reapply_compaction, resolved_compaction_decision, run_compaction,
    upstream_overflow_compaction_decision,
};

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
    upstream_access_token: String,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
    payload: kproxy_translate::KiroPayload,
}

/// A selected account and its resolved wire model, before any generation.
/// Keep the lease only on the no-compaction path; summaries need their own slot.
struct PreparedUpstream {
    lease: AccountLease,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
    payload: KiroPayload,
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

mod execution;

pub(super) use execution::{
    apply_remaining_output_budget, execute_kiro_web_search, find_model_fallback,
    prepare_kiro_payload, resolve_static_model, set_payload_model, validate_internal_continuation,
    web_search_error_code,
};
#[cfg(test)]
use execution::{
    build_model_path, ensure_web_search_profile_arn, push_nonstream_event, retry_attempt_count,
};
use execution::{
    credits, execute_upstream, nonstream_claude, nonstream_openai, openai_tool_identities,
    prepare_upstream, prepend_attempt_logs, prepend_execute_error_attempts, request_log,
    usage_record,
};

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
        let mut original_request: ClaudeRequest =
            serde_json::from_value(value).map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("Invalid request body: {error}"),
                    ErrorFormat::Claude,
                )
            })?;
        let has_context_edits =
            has_context_management_edits(original_request.context_management.as_ref());
        // Validate the semantically effective history before touching remote
        // media. A completed compaction boundary can make stale history
        // irrelevant to protocol validation, while `original_input_tokens`
        // still needs to count that pre-boundary history.
        let mut request = original_request.clone();
        let compaction_normalization = normalize_compaction_boundary(&mut request);
        let boundary_applied = compaction_normalization.boundary_applied;
        validate_claude(&request).map_err(claude_validation_error)?;
        // Counting must include the real bytes of every attachment in the
        // pre-edit request. Generation can avoid fetching history that will
        // be cleared, but the count endpoint promises both original and
        // effective token totals.
        let _attachment_guards =
            request::hydrate_claude_attachments(&state, &mut original_request).await?;
        request.clone_from(&original_request);
        normalize_compaction_boundary(&mut request);
        validate_claude(&request).map_err(claude_validation_error)?;
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
        let route = map_model(
            &request.model,
            &config.model_mapping,
            key_id.as_deref(),
            None,
            "",
        );
        let mut normal = TranslationOptions::new(route.mapped.clone(), "AI_EDITOR");
        normal.enhance_system_prompt = config.features.enhance_system_prompt;
        normal.enable_prompt_cache = config.features.enable_prompt_cache;
        let conversation_fingerprint = conversation_fingerprint(&request.messages);
        normal.conversation_id = stable_conversation_id(
            &headers,
            key_id.as_deref(),
            request.conversation_id.as_deref(),
            metadata_conversation_hint(request.metadata.as_ref())
                .or(conversation_fingerprint.as_deref()),
        );
        normal.additional_model_request_fields_schema = state
            .resolved_model_info(&route.mapped)
            .and_then(|model| model.additional_model_request_fields_schema);
        let mut original_payload = claude_to_kiro(&original_request, &normal);
        state.prepare_model_request(&mut original_payload);
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
        let mut effective_request = request.clone();
        let context_edit_stats = apply_context_management_edits(
            &mut effective_request,
            u64::try_from(original_input_tokens).unwrap_or(u64::MAX),
        );
        let mut effective_payload = claude_to_kiro(&effective_request, &normal);
        state.prepare_model_request(&mut effective_payload);
        let input_tokens = state
            .tokenizer
            .estimate_kiro_payload(&effective_payload)
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error,
                    ErrorFormat::Claude,
                )
            })?;
        let mut response = json!({"input_tokens":input_tokens});
        if has_context_edits {
            response["context_management"] = json!({
                "original_input_tokens":original_input_tokens
            });
        }
        state.stats.record(RequestLog {
            timestamp: now_secs(),
            trace_id: trace_id.clone(),
            request_id: format!("req_{}", Uuid::new_v4().simple()),
            path: path.clone(),
            model: route.mapped.clone(),
            original_model: request.model.clone(),
            kiro_model: route.mapped.clone(),
            account_id: String::new(),
            account_name: String::new(),
            endpoint: "local-tokenizer".into(),
            model_path: vec![request.model.clone(), route.mapped.clone()],
            model_mapping_rule: route.rule.clone(),
            attempts: Vec::new(),
            duration_ms: started.elapsed().as_millis() as u64,
            status: 200,
            input_tokens: input_tokens as u64,
            output_tokens: 0,
            credits: 0.0,
            error: None,
            diagnostics: RequestDiagnostics {
                client_status: 200,
                payload_bytes: body.len(),
                ..RequestDiagnostics::default()
            },
        });
        tracing::info!(
            trace_id = %trace_id,
            protocol = "claude_count_tokens",
            model = %request.model,
            input_tokens,
            original_input_tokens,
            compaction_boundary_applied = boundary_applied,
            removed_noop_compaction_blocks = compaction_normalization.removed_noop_blocks,
            removed_noop_compaction_messages = compaction_normalization.removed_noop_messages,
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
        enforce_codex_user_agent(&state, &headers)?;
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
        additional_model_request_fields_schema: None,
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
    enforce_client_user_agent(state, headers, ErrorFormat::Claude)
}

fn enforce_codex_user_agent(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), ApiError> {
    enforce_client_user_agent(state, headers, ErrorFormat::OpenAi)
}

fn enforce_client_user_agent(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    format: ErrorFormat,
) -> Result<(), ApiError> {
    if !state.config.current().server.enforce_user_agent_check {
        return Ok(());
    }
    let value = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let (valid, client) = match format {
        ErrorFormat::Claude => (
            value.starts_with("claude-cli/")
                && value.contains(" (external,")
                && value.ends_with(')'),
            "Claude Code",
        ),
        ErrorFormat::OpenAi => (is_codex_user_agent(value), "Codex"),
    };
    if valid {
        Ok(())
    } else {
        let mut error = ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("Access denied. 本协议仅限 {client} 客户端使用，禁止通过其他方式接入。"),
            format,
        );
        error.suppress_model_stats = true;
        Err(error)
    }
}

fn is_codex_user_agent(value: &str) -> bool {
    // Match the product token, never a substring or the optional originator
    // header alone. Codex CLI, exec, editor and desktop use different products.
    let Some((product, rest)) = value.split_once('/') else {
        return false;
    };
    let branded_codex = product
        .strip_prefix("Codex ")
        .is_some_and(|suffix| !suffix.trim().is_empty());
    if !branded_codex
        && !matches!(
            product,
            "codex_cli_rs"
                | "codex_exec"
                | "codex_vscode"
                | "codex-tui"
                | "codex_cli"
                | "codex"
                | "Codex"
                | "codex_desktop"
                | "codex_app"
        )
    {
        return false;
    }
    let version = rest.split_whitespace().next().unwrap_or_default();
    version.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
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
                "prompt is too long for model '{}': {} tokens > {}",
                limit.model, limit.input_tokens, limit.maximum
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
        .map_or(0, |context| {
            context
                .tools
                .iter()
                .filter(|tool| tool.specification().is_some())
                .count()
        })
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
        .filter_map(|tool| tool.specification().map(|tool| tool.name.clone()))
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
                "translated upstream payload is too large: {payload_bytes} bytes > {}; reduce the conversation, attached documents, or loaded tool schemas",
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
    compact_target_from_maximum(trigger)
        .min(context_maximum(state, true, model))
        .max(1)
}

fn compact_target_from_maximum(maximum: u64) -> u64 {
    maximum
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(maximum)
        .max(1)
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

pub(super) fn estimated_credits(
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
            Err(meter_error(MeterError::DailyLimitExceeded, format))
        }
        Err(error) => Err(meter_error(error, format)),
    }
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
                    "prompt is too long for resolved model '{}': {} tokens > {}",
                    limit.model, limit.input_tokens, limit.maximum
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
    let model_resolution_error = error.endpoint == "model-resolution";
    let upstream_status = error.status;
    let upstream_throttle = error.is_throttle();
    let model_unavailable = error.is_model_temporarily_unavailable();
    let request_rejection = error.is_request_rejection();
    let account_error = !request_rejection
        && !model_unavailable
        && (error.is_auth()
            || error.is_quota()
            || error.is_throttle()
            || matches!(error.status, Some(500..=599)));
    let status = if model_resolution_error {
        StatusCode::BAD_REQUEST
    } else {
        match error.status {
            // An upstream 413 describes Kiro's translated payload, not necessarily
            // a Claude request body over 32 MiB. Preserve the upstream status in
            // diagnostics, but use 400 so Claude Code displays the real message.
            Some(413) if format == ErrorFormat::Claude => StatusCode::BAD_REQUEST,
            Some(413) => StatusCode::PAYLOAD_TOO_LARGE,
            Some(400) if upstream_bad_request_is_actionable(&error.message) => {
                StatusCode::BAD_REQUEST
            }
            // kproxy already validates the public request and enforces its context
            // window before dispatch. A remaining opaque upstream 400 (including
            // an empty body or "Internal Server Error") is an integration/upstream
            // failure, not an actionable Claude Code request error.
            Some(400) => StatusCode::BAD_GATEWAY,
            Some(401 | 403) => StatusCode::SERVICE_UNAVAILABLE,
            Some(402 | 429) => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::BAD_GATEWAY,
        }
    };
    let message = if error.message.trim().is_empty() {
        "Upstream service error, please retry later".to_owned()
    } else {
        error.message
    };
    let mut output = ApiError::new(status, message, format);
    output.retry_after = !model_resolution_error
        && matches!(
            status,
            StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::TOO_MANY_REQUESTS
        );
    output.log_context = Box::new(context);
    output.upstream_status = upstream_status;
    output.error_stage = "upstream_dispatch";
    output.account_error = account_error;
    if model_resolution_error {
        output.error_code = "model_not_available";
        output.error_stage = "model_resolution";
        output.account_error = false;
    } else if model_unavailable {
        output.error_code = "upstream_model_unavailable";
    } else if upstream_status == Some(429) || upstream_throttle {
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

    fn response_assembly(message: impl Into<String>, format: ErrorFormat) -> Self {
        let mut error = Self::new(StatusCode::INTERNAL_SERVER_ERROR, message, format);
        error.error_stage = "response_assembly";
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
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        return ("proxy_internal_error", "internal");
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
mod model_tests;

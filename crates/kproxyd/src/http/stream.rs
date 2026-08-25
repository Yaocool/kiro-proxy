use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use kproxy_core::config::ThinkingOutputFormat;
use kproxy_kiro::{EventStreamDecoder, KiroEvent, KiroResponse};
use kproxy_pool::AccountLease;
use kproxy_translate::{
    auto_continue_payload, tool_search_continue_payload_batch, web_search_continue_payload_batch,
    ClaudeServerToolEmission, ClaudeToolSearchBudget, ClaudeToolSearchCatalog,
    ClaudeToolSearchTrace, ClaudeWebSearchTrace, KiroPayload, KiroToolUse, WebSearchReplayCodec,
};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::codec::Decoder;

use crate::meter::{now_secs, CreditReservation, UsageRecord};
use crate::state::AppState;
use crate::stats::{RequestDiagnostics, RequestLog, UpstreamAttemptLog};

use super::prompt_cache::{PromptCachePlan, PromptCacheProfile};
use super::response::{
    repair_json, web_search_citations, CompactionIterationUsage, DecodedResponse,
    OpenAiToolIdentity, StopSequenceFilter, ThinkingContentFilter, ToolLeakFilter,
};
use super::usage::{fallback_credits, fill_missing_usage, produced_output};

#[derive(Clone)]
pub struct KeepaliveHub {
    sender: broadcast::Sender<()>,
}

impl KeepaliveHub {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(16);
        let ticker = sender.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(8));
            interval.tick().await;
            loop {
                interval.tick().await;
                let _result = ticker.send(());
            }
        });
        Self { sender }
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.sender.subscribe()
    }
}

pub enum StreamProtocol {
    Claude,
    OpenAi,
}

impl StreamProtocol {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "openai",
        }
    }
}

pub struct StreamContext {
    pub state: Arc<AppState>,
    pub lease: AccountLease,
    /// Access token used to create the currently consumed upstream response.
    pub upstream_access_token: String,
    pub reservation: CreditReservation,
    pub trace_id: String,
    pub request_id: String,
    pub path: String,
    pub model: String,
    pub mapped_model: String,
    pub original_model: String,
    pub api_key_id: Option<String>,
    pub kiro_model: String,
    pub model_path: Vec<String>,
    pub model_mapping_rule: Option<String>,
    pub attempts: Vec<UpstreamAttemptLog>,
    pub input_tokens: u64,
    pub compact: bool,
    pub compaction_summary: Option<String>,
    pub compaction_iteration: Option<CompactionIterationUsage>,
    /// Effective input size before proxy-triggered model-mapping compaction.
    pub auto_compaction_original_input_tokens: Option<u64>,
    pub estimated_credits: f64,
    pub max_tokens: u32,
    pub stop_sequences: Vec<String>,
    pub started: Instant,
    pub prompt_cache: Option<PromptCacheProfile>,
    pub payload: KiroPayload,
    pub auto_continue_rounds: u32,
    pub buffer_tool_calls: bool,
    pub tool_call_buffer_delay_ms: u64,
    pub enable_tool_leak_filter: bool,
    /// Actual per-request decision. Beta headers only advertise capability and
    /// remain present when Claude Code sends `thinking.type = "disabled"`.
    pub thinking_enabled: bool,
    pub thinking_output_format: ThinkingOutputFormat,
    pub include_usage_chunk: bool,
    /// Kiro canonical web tool name -> original Claude server tool type.
    pub web_tool_names: std::collections::HashMap<String, String>,
    /// Deferred Claude tools retained outside the Kiro payload.
    pub tool_search: Option<Arc<ClaudeToolSearchCatalog>>,
    /// Aggregate Tool Search execution budget and already consumed operations.
    pub max_tool_search_operations: u32,
    pub tool_search_operations: u32,
    /// Zero disables proxy-executed Kiro MCP web search for this request.
    pub web_search_max_rounds: u32,
    /// True when the request explicitly supplied web_search.max_uses.
    pub web_search_client_limit: bool,
    /// Result-only blocks that complete pending server calls from prior turns.
    pub resumed_tool_searches: Vec<ClaudeToolSearchTrace>,
    pub resumed_web_searches: Vec<ClaudeWebSearchTrace>,
    pub resumed_server_events: Vec<super::response::ClaudeServerEvent>,
    pub diagnostics: RequestDiagnostics,
    /// Kiro-normalized tool name -> original OpenAI tool type and name.
    pub openai_tools: std::collections::HashMap<String, OpenAiToolIdentity>,
    pub _connection_guard: crate::state::AdmissionGuard,
    pub _admission_guard: crate::state::AdmissionGuard,
}

fn claude_initial_events(
    claude: &mut ClaudeState,
    compaction_summary: Option<&str>,
    resumed_server_events: &[super::response::ClaudeServerEvent],
    searches: &[ClaudeToolSearchTrace],
    web_searches: &[ClaudeWebSearchTrace],
) -> Vec<String> {
    let mut output = Vec::new();
    if let Some(summary) = compaction_summary {
        output.extend(claude.compaction(summary));
    }
    for event in resumed_server_events {
        output.extend(match event {
            super::response::ClaudeServerEvent::ToolSearch { index, .. } => searches
                .get(*index)
                .map(|trace| claude.tool_search(trace))
                .unwrap_or_default(),
            super::response::ClaudeServerEvent::WebSearch { index, .. } => web_searches
                .get(*index)
                .map(|trace| claude.web_search(trace))
                .unwrap_or_default(),
        });
    }
    output
}

fn prepend_pending_initial(pending: &mut Vec<String>, mut output: Vec<String>) -> Vec<String> {
    if output.is_empty() || pending.is_empty() {
        return output;
    }
    let mut combined = std::mem::take(pending);
    combined.append(&mut output);
    combined
}

fn build_claude_state(context: &StreamContext, prompt_cache: &PromptCachePlan) -> ClaudeState {
    let mut claude = ClaudeState::new(
        context.request_id.clone(),
        context.model.clone(),
        context.input_tokens,
        context.state.web_search_replay.clone(),
    );
    claude.openai_include_usage = context.include_usage_chunk;
    claude.auto_compaction_original_input_tokens = context.auto_compaction_original_input_tokens;
    claude.compaction_iteration = context.compaction_iteration;
    claude.set_prompt_cache_plan(prompt_cache);
    claude
}

pub fn response(
    upstream: KiroResponse,
    protocol: StreamProtocol,
    mut context: StreamContext,
) -> Response {
    let (initial_endpoint, initial_response, mut upstream_permit) = upstream.into_parts();
    let mut source = initial_response.bytes_stream();
    let mut endpoint = initial_endpoint.name.to_string();
    let mut keepalive = context.state.keepalive.subscribe();
    let bridge_trace_id = context.trace_id.clone();
    let bridge_request_id = context.request_id.clone();
    tracing::info!(
        trace_id = %context.trace_id,
        request_id = %context.request_id,
        protocol = protocol.as_str(),
        account_id = %context.lease.account_id(),
        endpoint,
        model = %context.model,
        "client stream response started"
    );
    let stream = async_stream::stream! {
        let mut buffer = BytesMut::new();
        let mut decoder = EventStreamDecoder;
        let mut decoded = DecodedResponse::default();
        let mut stop_filter = StopSequenceFilter::new(&context.stop_sequences);
        let mut accumulated_usage = kproxy_kiro::UsageInfo::default();
        let mut prompt_cache_plan = context.state.prompt_cache.plan(
            &context.lease.account_id(),
            context.prompt_cache.as_ref(),
        );
        let mut claude = build_claude_state(&context, &prompt_cache_plan);
        let mut accumulated_searches = std::mem::take(&mut context.resumed_tool_searches);
        let mut accumulated_web_searches = std::mem::take(&mut context.resumed_web_searches);
        let mut data_started = false;
        let mut pending_initial = if matches!(protocol, StreamProtocol::Claude) {
            claude_initial_events(
                &mut claude,
                context.compaction_summary.as_deref(),
                &context.resumed_server_events,
                &accumulated_searches,
                &accumulated_web_searches,
            )
        } else {
            Vec::new()
        };
        let created = now_secs();
        let mut failed = None;
        let mut upstream_access_token = context.upstream_access_token.clone();
        let mut pre_data_retries = 0;
        let mut attempted_accounts = HashSet::new();
        let mut fallback_model = None::<String>;
        let mut accumulated_text = String::new();
        let mut accumulated_reasoning = String::new();
        let mut payload = context.payload.clone();
        // Server calls must be emitted before client tool calls in mixed
        // turns, so buffering is mandatory whenever a server tool is active.
        // A pending Claude prelude also forces buffering: a tool event is not
        // a successful semantic boundary until its complete JSON validates.
        let buffer_tool_calls = context.buffer_tool_calls
            || context.tool_search.is_some()
            || context.web_search_max_rounds > 0
            || !pending_initial.is_empty();
        let client_has_tools = payload.conversation_state.current_message.user_input_message
            .user_input_message_context.as_ref().is_some_and(|context| !context.tools.is_empty());
        let mut auto_round = 0;
        let mut search_round = 0u32;
        let mut tool_search_operations = context.tool_search_operations;
        let mut web_search_round = accumulated_web_searches
            .iter()
            .filter(|search| search.executed)
            .count() as u32;
        'rounds: loop {
            let mut leak_filter = ToolLeakFilter::new(context.enable_tool_leak_filter);
            let mut thinking_filter = ThinkingContentFilter::new(context.thinking_enabled);
            loop {
                tokio::select! {
                    chunk = source.next() => match chunk {
                        Some(Ok(chunk)) => {
                            buffer.extend_from_slice(&chunk);
                            loop {
                                match decoder.decode(&mut buffer) {
                                    Ok(Some(event)) => {
                                        tracing::trace!(
                                            trace_id = %context.trace_id,
                                            request_id = %context.request_id,
                                            event = event_kind(&event),
                                            "upstream stream event decoded"
                                        );
                                        if let KiroEvent::Error { kind, message } = &event {
                                            failed = Some(format!("{kind}: {message}"));
                                            break;
                                        }
                                        let events = leak_filter
                                            .push(event)
                                            .into_iter()
                                            .flat_map(|event| thinking_filter.push(event))
                                            .collect::<Vec<_>>();
                                        for mut event in events {
                                            let internal_search = event_is_tool_search(
                                                &event,
                                                context.tool_search.as_deref(),
                                            );
                                            let internal_web_search = event_is_web_search(
                                                &event,
                                                context.web_search_max_rounds,
                                            );
                                            if !internal_search && !internal_web_search {
                                                restore_web_tool_name(&mut event, &context.web_tool_names);
                                            }
                                            if matches!(&event, KiroEvent::Reasoning { .. } | KiroEvent::ToolUse { .. }) {
                                                let pending = stop_filter.finish();
                                                if !pending.is_empty() {
                                                    let output = stream_event(
                                                        &protocol,
                                                        &mut claude,
                                                        &KiroEvent::AssistantResponse { content: pending },
                                                        created,
                                                        &context.model,
                                                        context.thinking_output_format,
                                                        &context.openai_tools,
                                                    );
                                                    let output = prepend_pending_initial(
                                                        &mut pending_initial,
                                                        output,
                                                    );
                                                    data_started |= !output.is_empty();
                                                    for data in output {
                                                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                                    }
                                                }
                                            }
                                            let visible_event = client_visible_event(
                                                &event,
                                                &mut stop_filter,
                                            );
                                            if !internal_search && !internal_web_search {
                                                if let Some(visible_event) = visible_event.as_ref().filter(|event| {
                                                    !should_buffer_tool_event(
                                                        event,
                                                        buffer_tool_calls,
                                                        &context.openai_tools,
                                                    )
                                                }) {
                                                    let output = stream_event(
                                                        &protocol,
                                                        &mut claude,
                                                        visible_event,
                                                        created,
                                                        &context.model,
                                                        context.thinking_output_format,
                                                        &context.openai_tools,
                                                    );
                                                    let output = prepend_pending_initial(
                                                        &mut pending_initial,
                                                        output,
                                                    );
                                                    data_started |= !output.is_empty();
                                                    for data in output {
                                                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                                    }
                                                }
                                            }
                                            if let Err(error) = decoded.push(event) {
                                                failed = Some(error);
                                                break;
                                            }
                                            if stop_filter.matched().is_some() {
                                                break;
                                            }
                                        }
                                        if failed.is_some() || stop_filter.matched().is_some() {
                                            break;
                                        }
                                    }
                                    Ok(None) => break,
                                    Err(error) => {
                                        failed = Some(error.to_string());
                                        break;
                                    }
                                }
                            }
                            if failed.is_some() || stop_filter.matched().is_some() { break; }
                        }
                        Some(Err(error)) => {
                            failed = Some(error.to_string());
                            break;
                        }
                        None => {
                            if let Err(error) = decoder.decode_eof(&mut buffer) {
                                failed = Some(error.to_string());
                            }
                            break;
                        },
                    },
                    heartbeat = keepalive.recv() => {
                        if heartbeat.is_ok() {
                            // Response headers are already committed when this
                            // body is polled. A ping is transport-only, so it
                            // deliberately does not flip `data_started`; a
                            // slow upstream can still be retried before its
                            // first semantic event.
                            let ping = match protocol {
                                StreamProtocol::Claude => sse(&json!({"type":"ping"})),
                                StreamProtocol::OpenAi => ": keepalive\n\n".into(),
                            };
                            yield Ok::<Bytes, Infallible>(Bytes::from(ping));
                        }
                    }
                }
            }
            if stop_filter.matched().is_none() {
                let mut events = Vec::new();
                for event in leak_filter.finish() {
                    events.extend(thinking_filter.push(event));
                }
                events.extend(thinking_filter.finish());
                for mut event in events {
                    let internal_search =
                        event_is_tool_search(&event, context.tool_search.as_deref());
                    let internal_web_search =
                        event_is_web_search(&event, context.web_search_max_rounds);
                    if !internal_search && !internal_web_search {
                        restore_web_tool_name(&mut event, &context.web_tool_names);
                    }
                    if matches!(
                        &event,
                        KiroEvent::Reasoning { .. } | KiroEvent::ToolUse { .. }
                    ) {
                        let pending = stop_filter.finish();
                        if !pending.is_empty() {
                            let output = stream_event(
                                &protocol,
                                &mut claude,
                                &KiroEvent::AssistantResponse { content: pending },
                                created,
                                &context.model,
                                context.thinking_output_format,
                                &context.openai_tools,
                            );
                            let output = prepend_pending_initial(&mut pending_initial, output);
                            data_started |= !output.is_empty();
                            for data in output {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                    }
                    let visible_event = client_visible_event(&event, &mut stop_filter);
                    if !internal_search && !internal_web_search {
                        if let Some(visible_event) = visible_event.as_ref().filter(|event| {
                            !should_buffer_tool_event(
                                event,
                                buffer_tool_calls,
                                &context.openai_tools,
                            )
                        }) {
                            let output = stream_event(
                                &protocol,
                                &mut claude,
                                visible_event,
                                created,
                                &context.model,
                                context.thinking_output_format,
                                &context.openai_tools,
                            );
                            let output = prepend_pending_initial(&mut pending_initial, output);
                            data_started |= !output.is_empty();
                            for data in output {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                    }
                    if let Err(error) = decoded.push(event) {
                        failed = Some(error);
                        break;
                    }
                    if stop_filter.matched().is_some() {
                        break;
                    }
                }
            }
            // Buffered tool calls have not produced client-visible data yet.
            // Validate them before committing the pending message/compaction
            // prelude so malformed tool JSON can still use the pre-data retry
            // path and cannot publish a failed compaction boundary.
            if failed.is_none() && stop_filter.matched().is_none() {
                if let Err(error) = decoded.validate_tool_inputs() {
                    failed = Some(error);
                }
            }
            if failed.is_some() {
                let failure_text = failed.as_deref().unwrap_or_default().to_string();
                let failed_account_id = context.lease.account_id();
                let is_auth = kproxy_kiro::client::text_is_auth_error(&failure_text);
                let is_quota = kproxy_kiro::client::text_is_quota_error(&failure_text);
                let is_throttle = kproxy_kiro::client::text_is_throttle_error(&failure_text);
                let is_request_rejection =
                    kproxy_kiro::client::text_is_request_rejection(&failure_text);
                if is_throttle {
                    context.state.record_stream_overload();
                }
                tracing::warn!(
                    trace_id = %context.trace_id,
                    request_id = %context.request_id,
                    account_id = %failed_account_id,
                    endpoint,
                    data_started,
                    auth_error = is_auth,
                    quota_error = is_quota,
                    throttle_error = is_throttle,
                    request_rejection = is_request_rejection,
                    error = %kproxy_translate::sanitize_error_message(&failure_text),
                    "upstream stream failed"
                );
                let config = context.state.config.current();
                let retry_limit = config
                    .upstream
                    .max_retries
                    .max(context.state.pool().snapshot().await.len() as u32);
                let may_switch = (!is_quota || config.pool.auto_switch_on_quota_exhausted)
                    && !is_request_rejection;
                if !data_started && pre_data_retries < retry_limit && may_switch && is_auth {
                    let mut disable_account = true;
                    if context
                        .state
                        .refresh_account_token_after_auth_failure(
                            &context.state.pool(),
                            &failed_account_id,
                            &upstream_access_token,
                        )
                        .await
                        .is_ok()
                    {
                        tracing::info!(
                            trace_id = %context.trace_id,
                            request_id = %context.request_id,
                            account_id = %failed_account_id,
                            "stream account token refreshed"
                        );
                        let account = context.lease.account().await;
                        payload.profile_arn.clone_from(&account.profile_arn);
                        match context.state.generate(&account, &payload).await {
                            Ok(retry) => {
                                upstream_access_token = account.credentials.access_token.clone();
                                let (next_endpoint, next_response, next_permit) = retry.into_parts();
                                endpoint = next_endpoint.name.to_string();
                                source = next_response.bytes_stream();
                                upstream_permit = next_permit;
                                buffer.clear();
                                decoder = EventStreamDecoder;
                                decoded = DecodedResponse::default();
                                stop_filter = StopSequenceFilter::new(&context.stop_sequences);
                                failed = None;
                                pre_data_retries += 1;
                                tracing::info!(
                                    trace_id = %context.trace_id,
                                    request_id = %context.request_id,
                                    account_id = %failed_account_id,
                                    endpoint,
                                    retry = pre_data_retries,
                                    "retrying stream after token refresh"
                                );
                                continue 'rounds;
                            }
                            Err(error) => {
                                disable_account = error.is_auth();
                                failed = Some(error.to_string());
                            }
                        }
                    }
                    if disable_account {
                        context.state.pool().mark_banned(&failed_account_id).await;
                        let account_name = if let Some(runtime) =
                            context.state.pool().get(&failed_account_id).await
                        {
                            runtime.account.read().await.display_name().to_owned()
                        } else {
                            failed_account_id.clone()
                        };
                        crate::alerts::emit_token_refresh_failure(
                            &context.state,
                            &failed_account_id,
                            &account_name,
                            "刷新后流式请求的上游认证仍然失败",
                        );
                    } else {
                        context.state.pool().record_error(&failed_account_id).await;
                    }
                }

                if !data_started
                    && pre_data_retries < retry_limit
                    && may_switch
                    && is_throttle
                    && config.features.enable_model_fallback
                    && fallback_model.is_none()
                {
                    let (models, _) = context.state.models.get(config.models.cache_ttl_ms);
                    if let Some(fallback) =
                        super::handlers::find_model_fallback(&context.kiro_model, &models)
                    {
                        let fits_context = super::handlers::check_context_limit(
                            &context.state,
                            context.input_tokens,
                            context.compact,
                            &fallback,
                        )
                        .is_ok();
                        if !fits_context {
                            tracing::warn!(
                                trace_id = %context.trace_id,
                                request_id = %context.request_id,
                                fallback_model = %fallback,
                                input_tokens = context.input_tokens,
                                "skipping stream model fallback because its context window is too small"
                            );
                        }
                        if fits_context {
                            fallback_model = Some(fallback.clone());
                            context.mapped_model.clone_from(&fallback);
                            context.kiro_model.clone_from(&fallback);
                            super::handlers::set_payload_model(&mut payload, &fallback);
                            let account = context.lease.account().await;
                            match context.state.generate(&account, &payload).await {
                                Ok(retry) => {
                                    upstream_access_token = account.credentials.access_token.clone();
                                    let (next_endpoint, next_response, next_permit) =
                                        retry.into_parts();
                                    endpoint = next_endpoint.name.to_string();
                                    source = next_response.bytes_stream();
                                    upstream_permit = next_permit;
                                    buffer.clear();
                                    decoder = EventStreamDecoder;
                                    decoded = DecodedResponse::default();
                                    stop_filter = StopSequenceFilter::new(&context.stop_sequences);
                                    failed = None;
                                    pre_data_retries += 1;
                                    continue 'rounds;
                                }
                                Err(error) => failed = Some(error.to_string()),
                            }
                        }
                    }
                }

                if is_quota && !is_throttle {
                    context
                        .state
                        .pool()
                        .record_quota_error(&failed_account_id)
                        .await;
                    if context.state.pool().get(&failed_account_id).await.is_some_and(|runtime| {
                        runtime.health() == kproxy_pool::AccountHealth::Exhausted
                    }) {
                        if let Err(error) = crate::tasks::persist_pool_accounts(&context.state).await {
                            tracing::error!(
                                trace_id = %context.trace_id,
                                request_id = %context.request_id,
                                account_id = %failed_account_id,
                                %error,
                                "failed to persist stream quota exhaustion"
                            );
                        }
                        crate::alerts::sync_account_quota(
                            &context.state,
                            &failed_account_id,
                        )
                        .await;
                        crate::alerts::sync_service_quota(&context.state).await;
                    }
                } else if !is_auth && !is_request_rejection {
                    context.state.pool().record_error(&failed_account_id).await;
                }
                attempted_accounts.insert(failed_account_id.clone());
                if !data_started && pre_data_retries < retry_limit && may_switch {
                    let (new_lease, account) = loop {
                        let candidate_ids = context
                            .state
                            .pool()
                            .snapshot()
                            .await
                            .into_iter()
                            .filter(|account| !attempted_accounts.contains(&account.id))
                            .map(|account| account.id)
                            .collect::<Vec<_>>();
                        if candidate_ids.is_empty() {
                            if let Some(message) = failed.as_deref() {
                                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, message)));
                            }
                            break 'rounds;
                        }
                        let new_lease = match context
                            .state
                            .pool()
                            .acquire(&context.kiro_model, context.estimated_credits, &candidate_ids)
                            .await
                        {
                            Ok(lease) => lease,
                            Err(error) => {
                                if context
                                    .state
                                    .pool()
                                    .all_matching_credit_exhausted(&context.kiro_model)
                                    .await
                                {
                                    crate::alerts::sync_service_quota(&context.state).await;
                                }
                                failed = Some(error.to_string());
                                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error.to_string())));
                                break 'rounds;
                            }
                        };
                        let account = new_lease.account().await;
                        let mut incompatible = false;
                        if let Some(runtime) = context.state.pool().get(&account.id).await {
                        let remaining = account
                            .usage
                            .as_ref()
                            .filter(|usage| usage.limit > 0.0)
                            .map(|usage| {
                                ((usage.limit - usage.current) / usage.limit * 100.0)
                                    .clamp(0.0, 100.0)
                            });
                        context.mapped_model = fallback_model.clone().unwrap_or_else(|| {
                            kproxy_translate::model::map_model(
                                &context.original_model,
                                &config.model_mapping,
                                context.api_key_id.as_deref(),
                                remaining,
                                "",
                            )
                            .mapped
                        });
                        context.kiro_model.clone_from(&context.mapped_model);
                        if let Some(resolved) = runtime.resolve_model(&context.kiro_model).await {
                            context.kiro_model = resolved;
                            super::handlers::set_payload_model(&mut payload, &context.kiro_model);
                        } else if runtime.has_model_cache().await {
                            if let Some(resolved) = runtime
                                .resolve_model(&config.features.default_model_id)
                                .await
                            {
                                context.kiro_model = resolved;
                                super::handlers::set_payload_model(
                                    &mut payload,
                                    &context.kiro_model,
                                );
                            } else {
                                incompatible = true;
                            }
                        }
                        if !incompatible
                            && super::handlers::check_context_limit(
                                &context.state,
                                context.input_tokens,
                                context.compact,
                                &context.kiro_model,
                            )
                            .is_err()
                        {
                            incompatible = true;
                            tracing::warn!(
                                trace_id = %context.trace_id,
                                request_id = %context.request_id,
                                account_id = %account.id,
                                resolved_model = %context.kiro_model,
                                input_tokens = context.input_tokens,
                                "skipping stream retry account because resolved model context is too small"
                            );
                        }
                        }
                        if incompatible {
                            attempted_accounts.insert(account.id);
                            drop(new_lease);
                            continue;
                        }
                        break (new_lease, account);
                    };
                    context.lease = new_lease;
                    prompt_cache_plan = context.state.prompt_cache.plan(
                        &context.lease.account_id(),
                        context.prompt_cache.as_ref(),
                    );
                    claude = build_claude_state(&context, &prompt_cache_plan);
                    pending_initial = if matches!(protocol, StreamProtocol::Claude) {
                        claude_initial_events(
                            &mut claude,
                            context.compaction_summary.as_deref(),
                            &context.resumed_server_events,
                            &accumulated_searches,
                            &accumulated_web_searches,
                        )
                    } else {
                        Vec::new()
                    };
                    tracing::info!(
                        trace_id = %context.trace_id,
                        request_id = %context.request_id,
                        previous_account_id = %failed_account_id,
                        account_id = %account.id,
                        retry = pre_data_retries + 1,
                        "switching stream to another account"
                    );
                    payload.profile_arn.clone_from(&account.profile_arn);
                    let exponent = pre_data_retries.min(5);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        200u64.saturating_mul(1u64 << exponent).min(5_000),
                    ))
                    .await;
                    match context.state.generate(&account, &payload).await {
                        Ok(retry) => {
                            upstream_access_token = account.credentials.access_token.clone();
                            let (next_endpoint, next_response, next_permit) = retry.into_parts();
                            endpoint = next_endpoint.name.to_string();
                            source = next_response.bytes_stream();
                            upstream_permit = next_permit;
                            buffer.clear();
                            decoder = EventStreamDecoder;
                            decoded = DecodedResponse::default();
                            stop_filter = StopSequenceFilter::new(&context.stop_sequences);
                            failed = None;
                            pre_data_retries += 1;
                            continue 'rounds;
                        }
                        Err(error) => failed = Some(error.to_string()),
                    }
                }
                if let Some(message) = failed.as_deref() {
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, message)));
                }
                break 'rounds;
            }
            pre_data_retries = 0;
            if !pending_initial.is_empty() {
                data_started = true;
                for data in std::mem::take(&mut pending_initial) {
                    yield Ok::<Bytes, Infallible>(Bytes::from(data));
                }
            }
            fill_missing_usage(&context.state, &mut decoded, &payload).await;
            if stop_filter.matched().is_some() {
                break 'rounds;
            }
            let output_exhausted = accumulated_usage
                .output_tokens
                .saturating_add(decoded.usage.output_tokens)
                >= u64::from(context.max_tokens);
            let search_keys = context.tool_search.as_ref().map(|catalog| {
                decoded.tools.iter().filter(|(_, tool)| catalog.is_search_tool(&tool.name))
                    .map(|(id, _)| id.clone()).collect::<Vec<_>>()
            }).unwrap_or_default();
            if let Some(catalog) = context.tool_search.as_ref().filter(|_| !search_keys.is_empty()) {
                let max_tool_search_rounds = context
                    .state
                    .config
                    .current()
                    .features
                    .tool_search_max_rounds
                    .clamp(1, 8);
                let search_uses = search_keys.into_iter().map(|key| {
                    let search = decoded.tools.remove(&key).expect("Tool Search buffer exists");
                    KiroToolUse {
                        tool_use_id: search.id,
                        name: search.name,
                        input: repair_json(&search.input),
                    }
                }).collect::<Vec<_>>();
                let parallel_web_uses = if context.web_search_max_rounds > 0 {
                    let keys = decoded.tools.iter()
                        .filter(|(_, tool)| tool.name == "web_search")
                        .map(|(id, _)| id.clone()).collect::<Vec<_>>();
                    keys.into_iter().map(|key| {
                        let search = decoded.tools.remove(&key).expect("web search buffer exists");
                        KiroToolUse {
                            tool_use_id: search.id,
                            name: search.name,
                            input: repair_json(&search.input),
                        }
                    }).collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                if search_round >= max_tool_search_rounds {
                    for search_use in &search_uses {
                        let mut trace = catalog.pending_trace(search_use);
                        trace.id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                        if matches!(protocol, StreamProtocol::Claude) {
                            for data in claude.tool_search(&trace) {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                        accumulated_searches.push(trace);
                    }
                    for search_use in &parallel_web_uses {
                        let trace = ClaudeWebSearchTrace::pending(
                            format!("srvtoolu_{}", uuid::Uuid::new_v4().simple()),
                            search_use.input.clone(),
                        );
                        if matches!(protocol, StreamProtocol::Claude) {
                            for data in claude.web_search(&trace) {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                        accumulated_web_searches.push(trace);
                    }
                    if decoded.tools.is_empty() {
                        decoded.stop_reason = Some(
                            if output_exhausted { "max_tokens" } else { "pause_turn" }.into(),
                        );
                    }
                    break 'rounds;
                }
                if !decoded.tools.is_empty() || output_exhausted {
                    for search_use in &search_uses {
                        let mut trace = catalog.pending_trace(search_use);
                        trace.id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                        if matches!(protocol, StreamProtocol::Claude) {
                            for data in claude.tool_search(&trace) {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                        accumulated_searches.push(trace);
                    }
                    for search_use in &parallel_web_uses {
                        let trace = ClaudeWebSearchTrace::pending(
                            format!("srvtoolu_{}", uuid::Uuid::new_v4().simple()),
                            search_use.input.clone(),
                        );
                        if matches!(protocol, StreamProtocol::Claude) {
                            for data in claude.web_search(&trace) {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                        accumulated_web_searches.push(trace);
                    }
                    if output_exhausted && decoded.tools.is_empty() {
                        decoded.stop_reason = Some("max_tokens".into());
                    }
                    break 'rounds;
                }
                let mut budget = match super::handlers::remaining_tool_search_budget(
                    &context.state,
                    &payload,
                    context.compact,
                )
                .await
                {
                    Ok(budget) => budget,
                    Err(error) => {
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error)));
                        break 'rounds;
                    }
                };
                let mut loaded_names = super::handlers::loaded_tool_names(&payload);
                let mut searches = Vec::with_capacity(search_uses.len());
                for search_use in search_uses {
                    let mut outcome = if tool_search_operations
                        >= context.max_tool_search_operations
                    {
                        catalog.unavailable_outcome(
                            &search_use,
                            format!(
                                "Tool Search operation limit of {} was reached",
                                context.max_tool_search_operations
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
                    outcome.trace.id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                    let consumed_bytes = outcome.tools.iter()
                        .filter_map(|tool| serde_json::to_vec(tool).ok())
                        .map(|tool| tool.len()).sum::<usize>()
                        .saturating_add(outcome.documentation.iter().map(String::len).sum());
                    budget = ClaudeToolSearchBudget {
                        max_tools: budget.max_tools.saturating_sub(outcome.tools.len()),
                        max_bytes: budget.max_bytes.saturating_sub(consumed_bytes),
                    };
                    loaded_names.extend(outcome.tools.iter()
                        .map(|tool| tool.tool_specification.name.clone()));
                    if matches!(protocol, StreamProtocol::Claude) {
                        for data in claude.tool_search(&outcome.trace) {
                            data_started = true;
                            yield Ok::<Bytes, Infallible>(Bytes::from(data));
                        }
                    }
                    accumulated_searches.push(outcome.trace.clone());
                    searches.push((search_use, outcome));
                }
                let mut parallel_web_searches = Vec::with_capacity(parallel_web_uses.len());
                for search_use in parallel_web_uses {
                    let query = search_use.input.get("query").and_then(Value::as_str)
                        .unwrap_or_default().trim().to_owned();
                    let server_id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                    let trace = if web_search_round >= context.web_search_max_rounds {
                        ClaudeWebSearchTrace::error(
                            server_id,
                            &query,
                            if context.web_search_client_limit { "max_uses_exceeded" } else { "unavailable" },
                            format!("web search is limited to {} uses", context.web_search_max_rounds),
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
                        match super::handlers::execute_kiro_web_search(
                            &context.state,
                            &context.lease,
                            &query,
                        ).await {
                            Ok(results) => ClaudeWebSearchTrace::success(server_id, &query, results),
                            Err(error) => ClaudeWebSearchTrace::error(
                                server_id,
                                &query,
                                super::handlers::web_search_error_code(&error),
                                kproxy_translate::sanitize_error_message(&error.to_string()),
                            )
                            .executed(),
                        }
                    };
                    if matches!(protocol, StreamProtocol::Claude) {
                        for data in claude.web_search(&trace) {
                            data_started = true;
                            yield Ok::<Bytes, Infallible>(Bytes::from(data));
                        }
                    }
                    accumulated_web_searches.push(trace.clone());
                    parallel_web_searches.push((search_use, trace));
                }
                let documentation_tokens = match context
                    .state
                    .tokenizer
                    .count(searches.iter().flat_map(|(_, outcome)| outcome.documentation.iter())
                        .cloned().collect::<Vec<_>>().join("\n\n"))
                    .await
                {
                    Ok(tokens) => tokens as u64,
                    Err(error) => {
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error)));
                        break 'rounds;
                    }
                };
                tracing::info!(
                    trace_id = %context.trace_id,
                    request_id = %context.request_id,
                    account_id = %context.lease.account_id(),
                    endpoint,
                    search_round = search_round + 1,
                    search_count = searches.len(),
                    matched_tools = searches.iter().map(|(_, outcome)| outcome.trace.references.len()).sum::<usize>(),
                    result_truncated = searches.iter().any(|(_, outcome)| outcome.trace.budget_truncated),
                    search_error = searches.iter().any(|(_, outcome)| outcome.trace.error.is_some()),
                    "proxy Tool Search batch executed"
                );
                let round_text = std::mem::take(&mut decoded.text);
                accumulated_text.push_str(&round_text);
                accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
                payload = tool_search_continue_payload_batch(&payload, &round_text, &searches);
                if !parallel_web_searches.is_empty() {
                    if let Some(assistant) = payload.conversation_state.history.last_mut()
                        .and_then(|message| message.assistant_response_message.as_mut())
                    {
                        assistant.tool_uses.extend(parallel_web_searches.iter()
                            .map(|(tool_use, _)| tool_use.clone()));
                    }
                    for (tool_use, trace) in &parallel_web_searches {
                        kproxy_translate::resume_web_search_payload(&mut payload, tool_use, trace);
                    }
                }
                accumulate_usage(&mut accumulated_usage, &decoded.usage);
                decoded = DecodedResponse::default();
                let budget_available = super::handlers::apply_remaining_output_budget(
                    &mut payload,
                    context.max_tokens,
                    accumulated_usage.output_tokens,
                );
                debug_assert!(budget_available);

                let next_input_tokens = match context.state.tokenizer.estimate_kiro_payload(&payload).await {
                    Ok(tokens) => tokens as u64,
                    Err(error) => {
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error)));
                        break 'rounds;
                    }
                };
                let next_tool_tokens = match context.state.tokenizer.estimate_kiro_tools(&payload).await {
                    Ok(tokens) => (tokens as u64).saturating_add(documentation_tokens),
                    Err(error) => {
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error)));
                        break 'rounds;
                    }
                };
                let next_payload_bytes = match serde_json::to_vec(&payload) {
                    Ok(payload) => payload.len(),
                    Err(error) => {
                        failed = Some(error.to_string());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error.to_string())));
                        break 'rounds;
                    }
                };
                let config = context.state.config.current();
                let max_loaded_tools = config
                    .context
                    .max_loaded_tools
                    .min(kproxy_translate::validate::MAX_TOOLS);
                let next_loaded_tools = payload.conversation_state.current_message
                    .user_input_message.user_input_message_context.as_ref()
                    .map_or(0, |context| context.tools.len());
                let budget_error = if next_loaded_tools > max_loaded_tools {
                    Some(format!(
                        "too many loaded tools after Tool Search: {next_loaded_tools} > {max_loaded_tools}"
                    ))
                } else if next_tool_tokens > u64::from(config.context.max_tool_input_tokens) {
                    Some(format!(
                        "loaded tool definitions are too large after Tool Search: {next_tool_tokens} estimated tokens > {}",
                        config.context.max_tool_input_tokens
                    ))
                } else if next_payload_bytes > config.context.max_upstream_payload_bytes {
                    Some(format!(
                        "translated upstream payload is too large after Tool Search: {next_payload_bytes} bytes > {}",
                        config.context.max_upstream_payload_bytes
                    ))
                } else {
                    None
                };
                if let Some(error) = budget_error {
                    failed = Some(error.clone());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error)));
                    break 'rounds;
                }
                let model = payload.conversation_state.current_message.user_input_message.model_id.clone();
                if let Err(limit) = super::handlers::check_context_limit(
                    &context.state,
                    next_input_tokens,
                    context.compact,
                    &model,
                ) {
                    let error = format!(
                        "prompt is too long after Tool Search: {} tokens > {}",
                        limit.input_tokens, limit.maximum
                    );
                    failed = Some(error.clone());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error)));
                    break 'rounds;
                }
                context.input_tokens = next_input_tokens;
                if let Err(error) = context.reservation.extend(context.estimated_credits) {
                    failed = Some(error.to_string());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error.to_string())));
                    break 'rounds;
                }
                let account = context.lease.account().await;
                match context.state.generate(&account, &payload).await {
                    Ok(next) => {
                        upstream_access_token = account.credentials.access_token.clone();
                        let (next_endpoint, next_response, next_permit) = next.into_parts();
                        endpoint = next_endpoint.name.to_string();
                        source = next_response.bytes_stream();
                        upstream_permit = next_permit;
                        buffer.clear();
                        decoder = EventStreamDecoder;
                        search_round += 1;
                        continue 'rounds;
                    }
                    Err(error) => {
                        failed = Some(error.to_string());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error.to_string())));
                        break 'rounds;
                    }
                }
            }
            let web_search_keys = if context.web_search_max_rounds > 0 {
                decoded.tools.iter()
                    .filter(|(_, tool)| tool.name == "web_search")
                    .map(|(id, _)| id.clone()).collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            if !web_search_keys.is_empty() {
                let search_uses = web_search_keys.into_iter().map(|key| {
                    let search = decoded.tools.remove(&key).expect("web search buffer exists");
                    KiroToolUse {
                        tool_use_id: search.id,
                        name: search.name,
                        input: repair_json(&search.input),
                    }
                }).collect::<Vec<_>>();
                if !decoded.tools.is_empty() || output_exhausted {
                    for search_use in &search_uses {
                        let trace = ClaudeWebSearchTrace::pending(
                            format!("srvtoolu_{}", uuid::Uuid::new_v4().simple()),
                            search_use.input.clone(),
                        );
                        if matches!(protocol, StreamProtocol::Claude) {
                            for data in claude.web_search(&trace) {
                                yield Ok::<Bytes, Infallible>(Bytes::from(data));
                            }
                        }
                        accumulated_web_searches.push(trace);
                    }
                    if output_exhausted && decoded.tools.is_empty() {
                        decoded.stop_reason = Some("max_tokens".into());
                    }
                    break 'rounds;
                }
                let mut searches = Vec::with_capacity(search_uses.len());
                for search_use in search_uses {
                    let query = search_use.input.get("query").and_then(Value::as_str)
                        .unwrap_or_default().trim().to_owned();
                    let server_id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
                    let trace = if web_search_round >= context.web_search_max_rounds {
                        ClaudeWebSearchTrace::error(
                            server_id,
                            &query,
                            if context.web_search_client_limit { "max_uses_exceeded" } else { "unavailable" },
                            if context.web_search_client_limit {
                                format!("web search is limited to max_uses={}", context.web_search_max_rounds)
                            } else {
                                format!("web search reached the proxy safety limit of {} uses", context.web_search_max_rounds)
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
                        match super::handlers::execute_kiro_web_search(
                            &context.state,
                            &context.lease,
                            &query,
                        ).await {
                            Ok(results) => ClaudeWebSearchTrace::success(server_id, &query, results),
                            Err(error) => ClaudeWebSearchTrace::error(
                                server_id,
                                &query,
                                super::handlers::web_search_error_code(&error),
                                kproxy_translate::sanitize_error_message(&error.to_string()),
                            )
                            .executed(),
                        }
                    };
                    if matches!(protocol, StreamProtocol::Claude) {
                        for data in claude.web_search(&trace) {
                            data_started = true;
                            yield Ok::<Bytes, Infallible>(Bytes::from(data));
                        }
                    }
                    accumulated_web_searches.push(trace.clone());
                    searches.push((search_use, trace));
                }
                tracing::info!(
                    trace_id = %context.trace_id,
                    request_id = %context.request_id,
                    account_id = %context.lease.account_id(),
                    endpoint,
                    web_search_round,
                    search_count = searches.len(),
                    result_count = searches.iter().map(|(_, trace)| trace.results.len()).sum::<usize>(),
                    search_error = searches.iter().any(|(_, trace)| trace.error.is_some()),
                    "proxy Kiro MCP web search batch executed"
                );
                let round_text = std::mem::take(&mut decoded.text);
                accumulated_text.push_str(&round_text);
                accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
                payload = web_search_continue_payload_batch(&payload, &round_text, &searches);
                accumulate_usage(&mut accumulated_usage, &decoded.usage);
                decoded = DecodedResponse::default();
                let budget_available = super::handlers::apply_remaining_output_budget(
                    &mut payload,
                    context.max_tokens,
                    accumulated_usage.output_tokens,
                );
                debug_assert!(budget_available);
                match super::handlers::validate_internal_continuation(
                    &context.state,
                    &payload,
                    context.compact,
                    &endpoint,
                    "Web Search",
                    context.tool_search.is_some(),
                )
                .await
                {
                    Ok(tokens) => context.input_tokens = tokens,
                    Err(error) => {
                        let error = error.to_string();
                        failed = Some(error.clone());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error)));
                        break 'rounds;
                    }
                }
                if let Err(error) = context.reservation.extend(context.estimated_credits) {
                    failed = Some(error.to_string());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error.to_string())));
                    break 'rounds;
                }
                let account = context.lease.account().await;
                match context.state.generate(&account, &payload).await {
                    Ok(next) => {
                        upstream_access_token = account.credentials.access_token.clone();
                        let (next_endpoint, next_response, next_permit) = next.into_parts();
                        endpoint = next_endpoint.name.to_string();
                        source = next_response.bytes_stream();
                        upstream_permit = next_permit;
                        buffer.clear();
                        decoder = EventStreamDecoder;
                        continue 'rounds;
                    }
                    Err(error) => {
                        failed = Some(error.to_string());
                        yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error.to_string())));
                        break 'rounds;
                    }
                }
            }
            let should_continue = !client_has_tools
                && !decoded.tools.is_empty()
                && !output_exhausted
                && auto_round < context.auto_continue_rounds;
            if !should_continue {
                if output_exhausted && decoded.tools.is_empty() {
                    decoded.stop_reason = Some("max_tokens".into());
                }
                break 'rounds;
            }
            tracing::info!(
                trace_id = %context.trace_id,
                request_id = %context.request_id,
                account_id = %context.lease.account_id(),
                endpoint,
                round = auto_round + 1,
                "starting automatic stream continuation"
            );
            let tool_uses = decoded.tools.values().map(|tool| KiroToolUse {
                tool_use_id: tool.id.clone(),
                name: tool.name.clone(),
                input: repair_json(&tool.input),
            }).collect::<Vec<_>>();
            let round_text = std::mem::take(&mut decoded.text);
            accumulated_text.push_str(&round_text);
            accumulated_reasoning.push_str(&std::mem::take(&mut decoded.reasoning));
            payload = auto_continue_payload(&payload, &round_text, tool_uses);
            decoded.tools.clear();
            accumulate_usage(&mut accumulated_usage, &decoded.usage);
            decoded.usage = kproxy_kiro::UsageInfo::default();
            let budget_available = super::handlers::apply_remaining_output_budget(
                &mut payload,
                context.max_tokens,
                accumulated_usage.output_tokens,
            );
            debug_assert!(budget_available);
            if let Err(error) = context.reservation.extend(context.estimated_credits) {
                failed = Some(error.to_string());
                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error.to_string())));
                break 'rounds;
            }
            let account = context.lease.account().await;
            match context.state.generate(&account, &payload).await {
                Ok(next) => {
                    upstream_access_token = account.credentials.access_token.clone();
                    let (next_endpoint, next_response, next_permit) = next.into_parts();
                    endpoint = next_endpoint.name.to_string();
                    source = next_response.bytes_stream();
                    upstream_permit = next_permit;
                    buffer.clear();
                    decoder = EventStreamDecoder;
                    auto_round += 1;
                }
                Err(error) => {
                    failed = Some(error.to_string());
                    yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error.to_string())));
                    break 'rounds;
                }
            }
        }
        let trailing = stop_filter.finish();
        if failed.is_none() && !trailing.is_empty() {
            let output = stream_event(
                &protocol,
                &mut claude,
                &KiroEvent::AssistantResponse { content: trailing },
                created,
                &context.model,
                context.thinking_output_format,
                &context.openai_tools,
            );
            for data in prepend_pending_initial(&mut pending_initial, output) {
                yield Ok::<Bytes, Infallible>(Bytes::from(data));
            }
        }
        accumulated_text.push_str(&decoded.text);
        accumulated_reasoning.push_str(&decoded.reasoning);
        decoded.text = accumulated_text;
        decoded.reasoning = accumulated_reasoning;
        decoded.tool_searches = accumulated_searches;
        decoded.web_searches = accumulated_web_searches;
        accumulate_usage(&mut decoded.usage, &accumulated_usage);
        apply_stream_stop(&mut decoded, &stop_filter);
        let current_round_output_tokens = decoded.usage.output_tokens;
        context.state.prompt_cache.commit(
            &context.lease.account_id(),
            &prompt_cache_plan,
            &mut decoded.usage,
        );
        context.prompt_cache = None;
        if failed.is_none() {
            if matches!(protocol, StreamProtocol::Claude) && !decoded.text.is_empty() {
                for data in claude.citations(&decoded.web_searches, &decoded.text) {
                    yield Ok::<Bytes, Infallible>(Bytes::from(data));
                }
            }
            if buffer_tool_calls {
                if context.buffer_tool_calls
                    && context.tool_call_buffer_delay_ms > 0
                    && !decoded.tools.is_empty()
                {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        context.tool_call_buffer_delay_ms,
                    ))
                    .await;
                }
                for tool in decoded.tools.values() {
                    let event = KiroEvent::ToolUse {
                        id: tool.id.clone(),
                        name: tool.name.clone(),
                        input_delta: tool.input.clone(),
                        stop: true,
                    };
                    for data in stream_event(
                        &protocol,
                        &mut claude,
                        &event,
                        created,
                        &context.model,
                        context.thinking_output_format,
                        &context.openai_tools,
                    ) {
                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                    }
                }
            }
            for data in stream_finish(
                &protocol,
                &mut claude,
                &decoded,
                created,
                &context.model,
                context.max_tokens,
                current_round_output_tokens,
                context.thinking_output_format,
                context.include_usage_chunk,
            ) {
                yield Ok::<Bytes, Infallible>(Bytes::from(data));
            }
        }
        drop(upstream_permit);
        finish_accounting(context, endpoint, decoded, failed, &payload).await;
    };
    // A bounded bridge supplies backpressure and ensures a client that stops
    // reading cannot hold an account lease forever.
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        futures::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            match tokio::time::timeout(std::time::Duration::from_secs(30), sender.send(item)).await
            {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    tracing::warn!(
                        trace_id = %bridge_trace_id,
                        request_id = %bridge_request_id,
                        "client disconnected before stream completed"
                    );
                    break;
                }
                Err(_) => {
                    tracing::warn!(
                        trace_id = %bridge_trace_id,
                        request_id = %bridge_request_id,
                        "client stream write timed out"
                    );
                    break;
                }
            }
        }
    });
    let receiver = futures::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    let mut response = Response::new(Body::from_stream(receiver));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    response
}

fn accumulate_usage(total: &mut kproxy_kiro::UsageInfo, addition: &kproxy_kiro::UsageInfo) {
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

fn stream_event(
    protocol: &StreamProtocol,
    claude: &mut ClaudeState,
    event: &KiroEvent,
    created: i64,
    model: &str,
    thinking_format: ThinkingOutputFormat,
    openai_tools: &std::collections::HashMap<String, OpenAiToolIdentity>,
) -> Vec<String> {
    match protocol {
        StreamProtocol::Claude => claude.event(event),
        StreamProtocol::OpenAi => {
            openai_event(event, claude, created, model, thinking_format, openai_tools)
        }
    }
}

fn client_visible_event(
    event: &KiroEvent,
    stop_filter: &mut StopSequenceFilter,
) -> Option<KiroEvent> {
    match event {
        KiroEvent::AssistantResponse { content } => {
            let content = stop_filter.push(content);
            (!content.is_empty()).then_some(KiroEvent::AssistantResponse { content })
        }
        _ => Some(event.clone()),
    }
}

fn apply_stream_stop(decoded: &mut DecodedResponse, stop_filter: &StopSequenceFilter) {
    let Some(sequence) = stop_filter.matched().map(str::to_owned) else {
        return;
    };
    let visible_text = decoded
        .text
        .get(..stop_filter.visible_bytes())
        .unwrap_or(&decoded.text)
        .to_owned();
    decoded.stop_at_sequence(visible_text, sequence);
}

fn event_kind(event: &KiroEvent) -> &'static str {
    match event {
        KiroEvent::AssistantResponse { .. } => "assistant_response",
        KiroEvent::ToolUse { .. } => "tool_use",
        KiroEvent::Reasoning { .. } => "reasoning",
        KiroEvent::MessageMetadata { .. } => "message_metadata",
        KiroEvent::Usage { .. } => "usage",
        KiroEvent::Error { .. } => "error",
        KiroEvent::Other { .. } => "other",
    }
}

fn event_is_tool_search(event: &KiroEvent, catalog: Option<&ClaudeToolSearchCatalog>) -> bool {
    let (Some(catalog), KiroEvent::ToolUse { name, .. }) = (catalog, event) else {
        return false;
    };
    catalog.is_search_tool(name)
}

fn event_is_web_search(event: &KiroEvent, max_rounds: u32) -> bool {
    max_rounds > 0 && matches!(event, KiroEvent::ToolUse { name, .. } if name == "web_search")
}

fn restore_web_tool_name(event: &mut KiroEvent, names: &std::collections::HashMap<String, String>) {
    if let KiroEvent::ToolUse { name, .. } = event {
        if let Some(original) = names.get(name) {
            name.clone_from(original);
        }
    }
}

fn should_buffer_tool_event(
    event: &KiroEvent,
    configured: bool,
    identities: &std::collections::HashMap<String, OpenAiToolIdentity>,
) -> bool {
    match event {
        KiroEvent::ToolUse { name, .. } => {
            configured
                || identities
                    .get(name)
                    .is_some_and(|identity| identity.kind == "custom")
        }
        _ => false,
    }
}

fn openai_event(
    event: &KiroEvent,
    state: &mut ClaudeState,
    created: i64,
    model: &str,
    thinking_format: ThinkingOutputFormat,
    tool_identities: &std::collections::HashMap<String, OpenAiToolIdentity>,
) -> Vec<String> {
    let delta = match event {
        KiroEvent::AssistantResponse { content } => {
            let prefix = if state.openai_thinking_open {
                state.openai_thinking_open = false;
                "</thinking>"
            } else {
                ""
            };
            json!({"content":format!("{prefix}{content}")})
        }
        KiroEvent::Reasoning { content } => match thinking_format {
            ThinkingOutputFormat::Openai => json!({"reasoning_content":content}),
            ThinkingOutputFormat::Claude => {
                let prefix = if state.openai_thinking_open {
                    ""
                } else {
                    state.openai_thinking_open = true;
                    "<thinking>"
                };
                json!({"content":format!("{prefix}{content}")})
            }
        },
        KiroEvent::ToolUse {
            id,
            name,
            input_delta,
            stop,
        } => {
            let next = state.tool_indices.len();
            let index = *state.tool_indices.entry(id.clone()).or_insert(next);
            let identity = tool_identities.get(name);
            let original_name = identity.map_or(name.as_str(), |identity| identity.name.as_str());
            if identity.is_some_and(|identity| identity.kind == "custom") {
                let input = if *stop {
                    repair_json(input_delta)
                        .get("input")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| input_delta.clone())
                } else {
                    input_delta.clone()
                };
                json!({"tool_calls":[{
                    "index":index,"id":id,"type":"custom",
                    "custom":{"name":original_name,"input":input}
                }]})
            } else {
                json!({"tool_calls":[{
                    "index":index,"id":id,"type":"function",
                    "function":{"name":original_name,"arguments":input_delta}
                }]})
            }
        }
        _ => return Vec::new(),
    };
    let mut chunk = json!({
        "id":state.request_id,"object":"chat.completion.chunk",
        "created":created,"model":model,"choices":[{"index":0,"delta":delta,"finish_reason":Value::Null}]
    });
    if state.openai_include_usage {
        chunk["usage"] = Value::Null;
    }
    vec![format!("data: {chunk}\n\n")]
}

struct ClaudeState {
    request_id: String,
    model: String,
    input_tokens: u64,
    auto_compaction_original_input_tokens: Option<u64>,
    compaction_iteration: Option<CompactionIterationUsage>,
    message_started: bool,
    block: Option<(usize, &'static str)>,
    next_index: usize,
    tool_indices: std::collections::HashMap<String, usize>,
    openai_thinking_open: bool,
    openai_include_usage: bool,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    web_search_replay: WebSearchReplayCodec,
}

impl ClaudeState {
    fn new(
        request_id: String,
        model: String,
        input_tokens: u64,
        web_search_replay: WebSearchReplayCodec,
    ) -> Self {
        Self {
            request_id,
            model,
            input_tokens,
            auto_compaction_original_input_tokens: None,
            compaction_iteration: None,
            message_started: false,
            block: None,
            next_index: 0,
            tool_indices: std::collections::HashMap::new(),
            openai_thinking_open: false,
            openai_include_usage: false,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            web_search_replay,
        }
    }

    fn set_prompt_cache_plan(&mut self, plan: &PromptCachePlan) {
        self.cache_creation_input_tokens = plan.cache_write_tokens();
        self.cache_read_input_tokens = plan.cache_read_tokens();
    }

    fn ensure_message(&mut self, output: &mut Vec<String>) {
        if !self.message_started {
            let uncached_input_tokens = self
                .input_tokens
                .saturating_sub(self.cache_creation_input_tokens)
                .saturating_sub(self.cache_read_input_tokens);
            output.push(sse(&json!({
                "type":"message_start","message":{
                    "id":self.request_id,"type":"message","role":"assistant","content":[],
                    "model":self.model,"stop_reason":Value::Null,"stop_sequence":Value::Null,
                    "usage":{
                        "input_tokens":uncached_input_tokens,
                        "output_tokens":0,
                        "cache_creation_input_tokens":self.cache_creation_input_tokens,
                        "cache_read_input_tokens":self.cache_read_input_tokens
                    }
                }
            })));
            self.message_started = true;
        }
    }

    fn switch_block(
        &mut self,
        output: &mut Vec<String>,
        kind: &'static str,
        initial: Value,
    ) -> usize {
        self.ensure_message(output);
        if let Some((index, current)) = self.block {
            if current == kind && kind != "tool_use" {
                return index;
            }
            if current == "thinking" {
                output.push(sse(&json!({
                    "type":"content_block_delta","index":index,
                    "delta":{"type":"signature_delta","signature":kproxy_translate::SIGNATURE_PLACEHOLDER}
                })));
            }
            output.push(sse(&json!({"type":"content_block_stop","index":index})));
        }
        let index = self.next_index;
        self.next_index += 1;
        self.block = Some((index, kind));
        output.push(sse(&json!({
            "type":"content_block_start","index":index,"content_block":initial
        })));
        index
    }

    fn event(&mut self, event: &KiroEvent) -> Vec<String> {
        let mut output = Vec::new();
        match event {
            KiroEvent::AssistantResponse { content } => {
                let index =
                    self.switch_block(&mut output, "text", json!({"type":"text","text":""}));
                output.push(sse(&json!({
                    "type":"content_block_delta","index":index,
                    "delta":{"type":"text_delta","text":content}
                })));
            }
            KiroEvent::Reasoning { content } => {
                let index = self.switch_block(
                    &mut output,
                    "thinking",
                    json!({"type":"thinking","thinking":"","signature":""}),
                );
                output.push(sse(&json!({
                    "type":"content_block_delta","index":index,
                    "delta":{"type":"thinking_delta","thinking":content}
                })));
            }
            KiroEvent::ToolUse {
                id,
                name,
                input_delta,
                stop,
            } => {
                let index = if let Some(index) = self.tool_indices.get(id).copied() {
                    index
                } else {
                    let index = self.switch_block(
                        &mut output,
                        "tool_use",
                        json!({
                            "type":"tool_use","id":id,"name":name,"input":{}
                        }),
                    );
                    self.tool_indices.insert(id.clone(), index);
                    index
                };
                if !input_delta.is_empty() {
                    output.push(sse(&json!({
                        "type":"content_block_delta","index":index,
                        "delta":{"type":"input_json_delta","partial_json":input_delta}
                    })));
                }
                if *stop && self.block == Some((index, "tool_use")) {
                    output.push(sse(&json!({"type":"content_block_stop","index":index})));
                    self.block = None;
                }
            }
            _ => {}
        }
        output
    }

    fn compaction(&mut self, content: &str) -> Vec<String> {
        let mut output = Vec::new();
        let index = self.switch_block(
            &mut output,
            "compaction",
            json!({"type":"compaction","content":Value::Null}),
        );
        output.push(sse(&json!({
            "type":"content_block_delta","index":index,
            "delta":{"type":"compaction_delta","content":content}
        })));
        output.push(sse(&json!({"type":"content_block_stop","index":index})));
        self.block = None;
        output
    }

    fn tool_search(&mut self, search: &ClaudeToolSearchTrace) -> Vec<String> {
        let mut output = Vec::new();
        if search.emission != ClaudeServerToolEmission::ResultOnly {
            let index = self.switch_block(
                &mut output,
                "server_tool_use",
                json!({
                    "type":"server_tool_use",
                    "id":search.id,
                    "name":search.name,
                    "input":{}
                }),
            );
            output.push(sse(&json!({
                "type":"content_block_delta",
                "index":index,
                "delta":{"type":"input_json_delta","partial_json":search.input.to_string()}
            })));
            output.push(sse(&json!({"type":"content_block_stop","index":index})));
            self.block = None;
        }

        if search.emission == ClaudeServerToolEmission::Pending {
            return output;
        }

        let result = if let Some(error) = &search.error {
            json!({
                "type":"tool_search_tool_result_error",
                "error_code":error.code,
                "error_message":error.message
            })
        } else {
            json!({
                "type":"tool_search_tool_search_result",
                "tool_references":search.references.iter().map(|name| json!({
                    "type":"tool_reference","tool_name":name
                })).collect::<Vec<_>>()
            })
        };
        let index = self.switch_block(
            &mut output,
            "tool_search_tool_result",
            json!({
                "type":"tool_search_tool_result",
                "tool_use_id":search.id,
                "content":result
            }),
        );
        output.push(sse(&json!({"type":"content_block_stop","index":index})));
        self.block = None;
        output
    }

    fn web_search(&mut self, search: &ClaudeWebSearchTrace) -> Vec<String> {
        let mut output = Vec::new();
        if search.emission != ClaudeServerToolEmission::ResultOnly {
            let index = self.switch_block(
                &mut output,
                "server_tool_use",
                json!({
                    "type":"server_tool_use",
                    "id":search.id,
                    "name":"web_search",
                    "input":{}
                }),
            );
            output.push(sse(&json!({
                "type":"content_block_delta",
                "index":index,
                "delta":{"type":"input_json_delta","partial_json":search.input.to_string()}
            })));
            output.push(sse(&json!({"type":"content_block_stop","index":index})));
            self.block = None;
        }

        if search.emission == ClaudeServerToolEmission::Pending {
            return output;
        }

        let result = if let Some(error) = &search.error {
            json!({
                "type":"web_search_tool_result_error",
                "error_code":error.code
            })
        } else {
            Value::Array(
                search
                    .results
                    .iter()
                    .map(|result| {
                        json!({
                            "type":"web_search_result",
                            "url":result.url,
                            "title":result.title,
                            "page_age":Value::Null,
                            "encrypted_content":self.web_search_replay.encrypt(result)
                        })
                    })
                    .collect(),
            )
        };
        let index = self.switch_block(
            &mut output,
            "web_search_tool_result",
            json!({
                "type":"web_search_tool_result",
                "tool_use_id":search.id,
                "content":result,
                "caller":{"type":"direct"}
            }),
        );
        output.push(sse(&json!({"type":"content_block_stop","index":index})));
        self.block = None;
        output
    }

    fn citations(&mut self, searches: &[ClaudeWebSearchTrace], answer_text: &str) -> Vec<String> {
        // Citation deltas must belong to an open text block. If a round ended
        // immediately after a server result there is no answer text to cite.
        if !matches!(self.block, Some((_, "text"))) {
            return Vec::new();
        }
        let citations = web_search_citations(searches, answer_text, &self.web_search_replay);
        if citations.is_empty() {
            return Vec::new();
        }
        let mut output = Vec::new();
        let index = self.switch_block(
            &mut output,
            "text",
            json!({"type":"text","text":"","citations":[]}),
        );
        for citation in citations {
            output.push(sse(&json!({
                "type":"content_block_delta",
                "index":index,
                "delta":{"type":"citations_delta","citation":citation}
            })));
        }
        output
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_finish(
    protocol: &StreamProtocol,
    claude: &mut ClaudeState,
    decoded: &DecodedResponse,
    created: i64,
    model: &str,
    max_tokens: u32,
    current_round_output_tokens: u64,
    thinking_format: ThinkingOutputFormat,
    include_usage_chunk: bool,
) -> Vec<String> {
    match protocol {
        StreamProtocol::Claude => {
            let mut output = Vec::new();
            claude.ensure_message(&mut output);
            if let Some((index, kind)) = claude.block.take() {
                if kind == "thinking" {
                    output.push(sse(&json!({
                        "type":"content_block_delta","index":index,
                        "delta":{"type":"signature_delta","signature":kproxy_translate::SIGNATURE_PLACEHOLDER}
                    })));
                }
                output.push(sse(&json!({"type":"content_block_stop","index":index})));
            }
            let stop = if let Some(reason) = decoded.stop_reason.as_deref() {
                reason
            } else if !decoded.tools.is_empty() {
                "tool_use"
            } else if current_round_output_tokens >= u64::from(max_tokens) {
                "max_tokens"
            } else {
                "end_turn"
            };
            let uncached_input_tokens = decoded
                .usage
                .input_tokens
                .saturating_sub(decoded.usage.cache_read_tokens)
                .saturating_sub(decoded.usage.cache_write_tokens);
            let mut usage = json!({
                "input_tokens":uncached_input_tokens,
                "output_tokens":decoded.usage.output_tokens,
                "cache_creation_input_tokens":decoded.usage.cache_write_tokens,
                "cache_read_input_tokens":decoded.usage.cache_read_tokens
            });
            if !decoded.web_searches.is_empty() {
                usage["server_tool_use"] = json!({
                    "web_search_requests":decoded.web_searches.iter()
                        .filter(|search| search.executed)
                        .count()
                });
            }
            if let Some(compaction) = claude.compaction_iteration {
                usage["iterations"] = json!([
                    {
                        "type":"compaction",
                        "input_tokens":compaction.input_tokens,
                        "output_tokens":compaction.output_tokens
                    },
                    {
                        "type":"message",
                        "input_tokens":decoded.usage.input_tokens,
                        "output_tokens":decoded.usage.output_tokens
                    }
                ]);
            }
            let mut event = json!({
                "type":"message_delta","delta":{
                    "stop_reason":stop,
                    "stop_sequence":decoded.stop_sequence.as_deref()
                },
                "usage":usage
            });
            if let Some(original_input_tokens) = claude.auto_compaction_original_input_tokens {
                event["context_management"] = json!({
                    "applied_edits":[{
                        "type":"compact_20260112",
                        "reason":"model_mapping_overflow",
                        "original_input_tokens":original_input_tokens,
                        "compacted_input_tokens":claude.input_tokens
                    }]
                });
            }
            output.push(sse(&event));
            output.push(sse(&json!({"type":"message_stop"})));
            output
        }
        StreamProtocol::OpenAi => {
            let mut output = Vec::new();
            if thinking_format == ThinkingOutputFormat::Claude && claude.openai_thinking_open {
                claude.openai_thinking_open = false;
                let mut chunk = json!({
                    "id":claude.request_id,"object":"chat.completion.chunk",
                    "created":created,"model":model,
                    "choices":[{"index":0,"delta":{"content":"</thinking>"},"finish_reason":Value::Null}]
                });
                if include_usage_chunk {
                    chunk["usage"] = Value::Null;
                }
                output.push(format!("data: {chunk}\n\n"));
            }
            let finish_reason = if !decoded.tools.is_empty() {
                "tool_calls"
            } else if decoded.stop_reason.as_deref() == Some("max_tokens")
                || current_round_output_tokens >= u64::from(max_tokens)
            {
                "length"
            } else {
                "stop"
            };
            let mut final_chunk = json!({
                "id":claude.request_id,"object":"chat.completion.chunk",
                "created":created,"model":model,"choices":[{"index":0,"delta":{},
                    "finish_reason":finish_reason}]
            });
            if include_usage_chunk {
                final_chunk["usage"] = Value::Null;
            }
            output.push(format!("data: {final_chunk}\n\n"));
            if include_usage_chunk {
                output.push(format!(
                    "data: {}\n\n",
                    json!({
                        "id":claude.request_id,"object":"chat.completion.chunk",
                        "created":created,"model":model,"choices":[],
                        "usage":{"prompt_tokens":decoded.usage.input_tokens,
                            "completion_tokens":decoded.usage.output_tokens,
                            "total_tokens":decoded.usage.input_tokens+decoded.usage.output_tokens,
                            "prompt_tokens_details":{
                                "cached_tokens":decoded.usage.cache_read_tokens
                            },
                            "completion_tokens_details":{
                                "reasoning_tokens":decoded.usage.reasoning_tokens
                            }}
                    })
                ));
            }
            output.push("data: [DONE]\n\n".into());
            output
        }
    }
}

fn stream_error(protocol: &StreamProtocol, message: &str) -> String {
    let safe = kproxy_translate::sanitize_error_message(message);
    match protocol {
        StreamProtocol::Claude => sse(&json!({
            "type":"error","error":{"type":"api_error","message":safe}
        })),
        StreamProtocol::OpenAi => format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "error":{"type":"server_error","message":safe,"code":Value::Null}
            })
        ),
    }
}

fn sse(value: &Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message"),
        value
    )
}

async fn finish_accounting(
    mut context: StreamContext,
    endpoint: String,
    mut decoded: DecodedResponse,
    failure: Option<String>,
    payload: &KiroPayload,
) {
    context.diagnostics.tool_search_rounds = decoded.tool_searches.len();
    context.diagnostics.tool_search_matches = decoded
        .tool_searches
        .iter()
        .map(|search| search.matched_count)
        .sum();
    context.diagnostics.search_requested_limit = decoded
        .tool_searches
        .iter()
        .map(|search| search.requested_limit)
        .max()
        .unwrap_or_default();
    context.diagnostics.search_returned_count = decoded
        .tool_searches
        .iter()
        .map(|search| search.references.len())
        .sum();
    context.diagnostics.search_budget_truncated = decoded
        .tool_searches
        .iter()
        .any(|search| search.budget_truncated);
    context.diagnostics.web_search_rounds = decoded
        .web_searches
        .iter()
        .filter(|search| search.executed)
        .count();
    context.diagnostics.web_search_results = decoded
        .web_searches
        .iter()
        .map(|search| search.results.len())
        .sum();
    // The HTTP status is already committed as 200 once an SSE stream starts;
    // retain the semantic failure in RequestLog.status and these stable fields.
    context.diagnostics.client_status = 200;
    if let Some(message) = failure.as_deref() {
        let details = classify_stream_failure(message);
        context.diagnostics.upstream_status = details.upstream_status;
        context.diagnostics.error_code = details.error_code.into();
        context.diagnostics.error_stage = details.error_stage.into();
        context.diagnostics.account_error = details.account_error;
    } else {
        context.diagnostics.upstream_status = Some(200);
    }
    if failure.is_none() {
        context
            .state
            .pool()
            .record_success(&context.lease.account_id())
            .await;
    }
    let produced_output = produced_output(&decoded);
    fill_missing_usage(&context.state, &mut decoded, payload).await;
    context.state.prompt_cache.apply(
        &context.lease.account_id(),
        context.prompt_cache.as_ref(),
        &mut decoded.usage,
    );
    let credits = if failure.is_some() && !produced_output {
        0.0
    } else if decoded.usage.credits > 0.0 {
        decoded.usage.credits
    } else {
        fallback_credits(
            &context.state,
            &context.kiro_model,
            decoded.usage.input_tokens,
            decoded.usage.output_tokens,
        )
    };
    context.lease.settle_credits(credits).await;
    if let Err(error) = context
        .reservation
        .settle(UsageRecord {
            timestamp: now_secs(),
            model: context.mapped_model.clone(),
            original_model: Some(context.original_model.clone()),
            kiro_model: Some(context.kiro_model.clone()),
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
            path: context.path.clone(),
        })
        .await
    {
        tracing::error!(
            trace_id = %context.trace_id,
            request_id = %context.request_id,
            account_id = %context.lease.account_id(),
            %error,
            "failed to persist stream usage"
        );
    }
    let status = if failure.is_some() { 502 } else { 200 };
    let duration_ms = context.started.elapsed().as_millis() as u64;
    let account = context.lease.account().await;
    let account_name = account.display_name().to_owned();
    if let Some(error) = failure.as_deref() {
        tracing::error!(
            trace_id = %context.trace_id,
            request_id = %context.request_id,
            account_id = %context.lease.account_id(),
            account_name,
            endpoint,
            model_path = %context.model_path.join(" -> "),
            mapping_rule = context.model_mapping_rule.as_deref().unwrap_or("none"),
            status,
            input_tokens = decoded.usage.input_tokens,
            output_tokens = decoded.usage.output_tokens,
            credits,
            duration_ms,
            error = %kproxy_translate::sanitize_error_message(error),
            "client stream response completed with failure"
        );
    } else {
        tracing::info!(
            trace_id = %context.trace_id,
            request_id = %context.request_id,
            account_id = %context.lease.account_id(),
            account_name,
            endpoint,
            model_path = %context.model_path.join(" -> "),
            mapping_rule = context.model_mapping_rule.as_deref().unwrap_or("none"),
            status,
            input_tokens = decoded.usage.input_tokens,
            output_tokens = decoded.usage.output_tokens,
            credits,
            duration_ms,
            "client stream response completed"
        );
    }
    context.state.stats.record(RequestLog {
        timestamp: now_secs(),
        trace_id: context.trace_id,
        request_id: context.request_id,
        path: context.path,
        model: context.mapped_model,
        original_model: context.original_model,
        kiro_model: context.kiro_model,
        account_id: context.lease.account_id(),
        account_name,
        endpoint,
        model_path: context.model_path,
        model_mapping_rule: context.model_mapping_rule,
        attempts: context.attempts,
        duration_ms,
        status,
        input_tokens: decoded.usage.input_tokens,
        output_tokens: decoded.usage.output_tokens,
        credits,
        error: failure,
        diagnostics: context.diagnostics,
    });
}

struct StreamFailureDetails {
    upstream_status: Option<u16>,
    error_code: &'static str,
    error_stage: &'static str,
    account_error: bool,
}

fn classify_stream_failure(message: &str) -> StreamFailureDetails {
    let lower = message.to_ascii_lowercase();
    let upstream_status = extract_upstream_status(&lower);
    if lower.contains("prompt is too long") || lower.contains("context length") {
        return StreamFailureDetails {
            upstream_status,
            error_code: "context_length_exceeded",
            error_stage: "context_validation",
            account_error: false,
        };
    }
    if lower.contains("too many loaded tools")
        || lower.contains("tool definitions are too large")
        || lower.contains("payload is too large")
        || lower.contains("payload too large")
    {
        return StreamFailureDetails {
            upstream_status,
            error_code: "tool_budget_exceeded",
            error_stage: "request_budget",
            account_error: false,
        };
    }
    if kproxy_kiro::client::text_is_request_rejection(message) {
        return StreamFailureDetails {
            upstream_status,
            error_code: "invalid_tool_protocol",
            error_stage: "upstream_stream",
            account_error: false,
        };
    }
    if kproxy_kiro::client::text_is_throttle_error(message) {
        return StreamFailureDetails {
            upstream_status: upstream_status.or(Some(429)),
            error_code: "upstream_rate_limited",
            error_stage: "upstream_stream",
            account_error: true,
        };
    }
    let account_error = kproxy_kiro::client::text_is_auth_error(message)
        || kproxy_kiro::client::text_is_quota_error(message)
        || upstream_status.is_some_and(|status| status >= 500)
        || upstream_status.is_none();
    StreamFailureDetails {
        upstream_status,
        error_code: "upstream_unavailable",
        error_stage: "upstream_stream",
        account_error,
    }
}

fn extract_upstream_status(message: &str) -> Option<u16> {
    ["returned some(", "http status ", "status code "]
        .iter()
        .find_map(|marker| {
            let start = message.find(marker)? + marker.len();
            let digits = message[start..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            let status = digits.parse::<u16>().ok()?;
            (400..=599).contains(&status).then_some(status)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replay_codec() -> WebSearchReplayCodec {
        WebSearchReplayCodec::from_key([0x6B; 32])
    }

    fn streamed_values(output: &[String]) -> Vec<Value> {
        output
            .iter()
            .flat_map(|event| event.lines())
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|line| *line != "[DONE]")
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    #[test]
    fn stream_failures_keep_real_upstream_status_and_account_scope() {
        let rejected = classify_stream_failure(
            "Kiro Amazon Q returned Some(400): tool schema payload too large",
        );
        assert_eq!(rejected.upstream_status, Some(400));
        assert_eq!(rejected.error_code, "tool_budget_exceeded");
        assert!(!rejected.account_error);

        let rejected_5xx = classify_stream_failure(
            "Kiro Amazon Q returned Some(503): tool schema payload too large",
        );
        assert_eq!(rejected_5xx.upstream_status, Some(503));
        assert_eq!(rejected_5xx.error_code, "tool_budget_exceeded");
        assert!(!rejected_5xx.account_error);

        let unavailable =
            classify_stream_failure("Kiro Amazon Q returned Some(503): Internal Server Error");
        assert_eq!(unavailable.upstream_status, Some(503));
        assert_eq!(unavailable.error_code, "upstream_unavailable");
        assert!(unavailable.account_error);
    }

    #[test]
    fn thinking_signature_stays_on_the_same_block_and_message_starts_once() {
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
        let mut output = state.event(&KiroEvent::Reasoning {
            content: "think".into(),
        });
        output.extend(state.event(&KiroEvent::AssistantResponse {
            content: "answer".into(),
        }));
        let joined = output.join("");
        assert_eq!(joined.matches("event: message_start").count(), 1);
        assert_eq!(joined.matches("signature_delta").count(), 1);
        let signature = joined.find("signature_delta").expect("signature");
        let stop = joined.find("content_block_stop").expect("stop");
        assert!(signature < stop);
        assert!(joined.contains("\"index\":0"));
    }

    #[test]
    fn tagged_thinking_streams_as_native_claude_blocks() {
        let mut filter = ThinkingContentFilter::new(true);
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
        let mut output = Vec::new();
        for chunk in ["<thin", "king>hidden", "</thinking>Hello"] {
            for event in filter.push(KiroEvent::AssistantResponse {
                content: chunk.into(),
            }) {
                output.extend(state.event(&event));
            }
        }
        for event in filter.finish() {
            output.extend(state.event(&event));
        }
        let joined = output.join("");
        let values = streamed_values(&output);

        assert!(!joined.contains("<thinking>"));
        assert!(!joined.contains("</thinking>"));
        assert!(values.iter().any(|value| {
            value["delta"]["type"] == "thinking_delta" && value["delta"]["thinking"] == "hidden"
        }));
        assert!(values.iter().any(|value| {
            value["delta"]["type"] == "text_delta" && value["delta"]["text"] == "Hello"
        }));
        assert_eq!(joined.matches("signature_delta").count(), 1);
    }

    #[test]
    fn disabled_thinking_streams_only_visible_text() {
        let mut filter = ThinkingContentFilter::new(false);
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
        let mut events = filter.push(KiroEvent::AssistantResponse {
            content: "<thinking>tagged secret</thinking>Hello".into(),
        });
        events.extend(filter.push(KiroEvent::Reasoning {
            content: "native secret".into(),
        }));
        events.extend(filter.finish());
        let output = events
            .iter()
            .flat_map(|event| state.event(event))
            .collect::<Vec<_>>();
        let joined = output.join("");
        let values = streamed_values(&output);

        assert!(!joined.contains("thinking_delta"));
        assert!(!joined.contains("signature_delta"));
        assert!(!joined.contains("secret"));
        assert!(values.iter().any(|value| {
            value["delta"]["type"] == "text_delta" && value["delta"]["text"] == "Hello"
        }));
    }

    #[test]
    fn finish_adds_signature_without_opening_a_second_thinking_block() {
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
        let started = state.event(&KiroEvent::Reasoning {
            content: "think".into(),
        });
        let decoded = DecodedResponse {
            reasoning: "think".into(),
            ..DecodedResponse::default()
        };
        let finished = stream_finish(
            &StreamProtocol::Claude,
            &mut state,
            &decoded,
            0,
            "model",
            100,
            10,
            ThinkingOutputFormat::Claude,
            false,
        );
        let joined = [started, finished].concat().join("");
        assert_eq!(joined.matches("event: message_start").count(), 1);
        assert_eq!(joined.matches("\"type\":\"thinking\"").count(), 1);
        assert_eq!(joined.matches("signature_delta").count(), 1);
        assert!(joined.contains(kproxy_translate::SIGNATURE_PLACEHOLDER));
    }

    #[test]
    fn compaction_stream_block_is_emitted_before_text() {
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
        let mut output = state.compaction("summary");
        output.extend(state.event(&KiroEvent::AssistantResponse {
            content: "answer".into(),
        }));
        let joined = output.join("");

        assert_eq!(joined.matches("event: message_start").count(), 1);
        assert!(joined.contains("compaction_delta"));
        assert!(
            joined.find("compaction_delta").expect("compaction")
                < joined.find("text_delta").expect("text")
        );
    }

    #[test]
    fn compaction_precedes_resumed_server_events() {
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
        let search = ClaudeToolSearchTrace {
            id: "srvtoolu_1".into(),
            name: "tool_search_tool_regex".into(),
            input: json!({"pattern":"github"}),
            references: vec!["mcp__github__list_issues".into()],
            error: None,
            requested_limit: 5,
            matched_count: 1,
            budget_truncated: false,
            emission: ClaudeServerToolEmission::ResultOnly,
        };
        let output = claude_initial_events(
            &mut state,
            Some("summary"),
            &[crate::http::response::ClaudeServerEvent::ToolSearch {
                index: 0,
                preceding_text: String::new(),
            }],
            &[search],
            &[],
        )
        .join("");

        assert!(
            output.find("compaction_delta").expect("compaction")
                < output
                    .find("tool_search_tool_result")
                    .expect("resumed result")
        );
    }

    #[test]
    fn compaction_prelude_waits_for_the_first_semantic_event() {
        let mut pending = vec!["message_start".into(), "compaction".into()];

        assert!(prepend_pending_initial(&mut pending, Vec::new()).is_empty());
        assert_eq!(pending, ["message_start", "compaction"]);

        let output = prepend_pending_initial(&mut pending, vec!["text".into()]);
        assert_eq!(output, ["message_start", "compaction", "text"]);
        assert!(pending.is_empty());
    }

    #[test]
    fn automatic_compaction_stats_are_emitted_in_the_final_message_delta() {
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 23_000, replay_codec());
        state.auto_compaction_original_input_tokens = Some(180_000);
        state.compaction_iteration = Some(CompactionIterationUsage {
            input_tokens: 180_500,
            output_tokens: 3_500,
        });
        let decoded = DecodedResponse {
            usage: kproxy_kiro::UsageInfo {
                input_tokens: 47_000,
                output_tokens: 900,
                ..kproxy_kiro::UsageInfo::default()
            },
            ..DecodedResponse::default()
        };
        let output = stream_finish(
            &StreamProtocol::Claude,
            &mut state,
            &decoded,
            0,
            "model",
            100,
            0,
            ThinkingOutputFormat::Claude,
            false,
        )
        .join("");

        assert!(output.contains("\"reason\":\"model_mapping_overflow\""));
        assert!(output.contains("\"original_input_tokens\":180000"));
        assert!(output.contains("\"compacted_input_tokens\":23000"));
        assert!(output.contains("\"type\":\"compaction\""));
        assert!(output.contains("\"input_tokens\":180500"));
        let final_delta = output
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|event| event["type"] == "message_delta")
            .expect("message delta");
        assert_eq!(
            final_delta["usage"]["iterations"][1]["input_tokens"],
            47_000
        );
    }

    #[test]
    fn tool_search_stream_uses_server_blocks_and_references() {
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
        let output = state.tool_search(&ClaudeToolSearchTrace {
            id: "srvtoolu_1".into(),
            name: "tool_search_tool_regex".into(),
            input: json!({"pattern":"github"}),
            references: vec!["mcp__github__list_issues".into()],
            error: None,
            requested_limit: 5,
            matched_count: 1,
            budget_truncated: false,
            emission: ClaudeServerToolEmission::Complete,
        });
        let joined = output.join("");
        assert_eq!(joined.matches("event: message_start").count(), 1);
        assert!(joined.contains("server_tool_use"));
        assert!(joined.contains("tool_search_tool_result"));
        assert!(joined.contains("tool_reference"));
        assert!(joined.contains("mcp__github__list_issues"));
    }

    #[test]
    fn web_search_stream_uses_native_server_result_blocks() {
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10, replay_codec());
        let trace = ClaudeWebSearchTrace::success(
            "srvtoolu_web".into(),
            "rust async",
            kproxy_translate::WebSearchResults {
                query: "rust async".into(),
                total_results: 1,
                results: vec![kproxy_translate::WebSearchResult {
                    title: "Tokio".into(),
                    url: "https://tokio.rs".into(),
                    snippet: "runtime".into(),
                    published_date: None,
                }],
            },
        );
        let mut output = state.web_search(&trace);
        output.extend(state.event(&KiroEvent::AssistantResponse {
            content: "Tokio uses an async runtime: https://tokio.rs".into(),
        }));
        output.extend(state.citations(&[trace], "Tokio uses an async runtime: https://tokio.rs"));
        let joined = output.join("");
        assert_eq!(joined.matches("event: message_start").count(), 1);
        assert!(joined.contains("server_tool_use"));
        assert!(joined.contains("web_search_tool_result"));
        assert!(joined.contains("https://tokio.rs"));
        assert!(!joined.contains("snippet"));
        assert!(joined.contains("citations_delta"));
        assert!(joined.contains("encrypted_index"));
    }

    #[test]
    fn stream_finish_honors_pause_turn_override() {
        let mut state = ClaudeState::new("msg_pause".into(), "model".into(), 10, replay_codec());
        let decoded = DecodedResponse {
            stop_reason: Some("pause_turn".into()),
            ..DecodedResponse::default()
        };
        let output = stream_finish(
            &StreamProtocol::Claude,
            &mut state,
            &decoded,
            0,
            "model",
            100,
            0,
            ThinkingOutputFormat::Claude,
            false,
        )
        .join("");
        assert!(output.contains("\"stop_reason\":\"pause_turn\""));
    }

    #[test]
    fn claude_stream_reports_stop_sequences_and_cache_usage() {
        let mut state = ClaudeState::new("msg_stop".into(), "model".into(), 100, replay_codec());
        state.cache_creation_input_tokens = 5;
        state.cache_read_input_tokens = 20;
        let mut filter = StopSequenceFilter::new(&["<END>".into()]);
        let first = client_visible_event(
            &KiroEvent::AssistantResponse {
                content: "hello <E".into(),
            },
            &mut filter,
        )
        .expect("visible prefix");
        assert_eq!(
            first,
            KiroEvent::AssistantResponse {
                content: "hello ".into()
            }
        );
        assert!(client_visible_event(
            &KiroEvent::AssistantResponse {
                content: "ND>ignored".into(),
            },
            &mut filter,
        )
        .is_none());

        let mut decoded = DecodedResponse {
            text: "hello <END>ignored".into(),
            usage: kproxy_kiro::UsageInfo {
                input_tokens: 100,
                output_tokens: 10,
                cache_read_tokens: 20,
                cache_write_tokens: 5,
                ..kproxy_kiro::UsageInfo::default()
            },
            ..DecodedResponse::default()
        };
        decoded.stop_at_sequence("hello ".into(), "<END>".into());
        let output = stream_finish(
            &StreamProtocol::Claude,
            &mut state,
            &decoded,
            123,
            "model",
            100,
            10,
            ThinkingOutputFormat::Claude,
            false,
        );
        let values = streamed_values(&output);
        let start = values
            .iter()
            .find(|value| value["type"] == "message_start")
            .expect("message start");
        assert_eq!(start["message"]["usage"]["input_tokens"], 75);
        assert_eq!(start["message"]["usage"]["cache_creation_input_tokens"], 5);
        assert_eq!(start["message"]["usage"]["cache_read_input_tokens"], 20);
        let delta = values
            .iter()
            .find(|value| value["type"] == "message_delta")
            .expect("message delta");
        assert_eq!(delta["delta"]["stop_reason"], "stop_sequence");
        assert_eq!(delta["delta"]["stop_sequence"], "<END>");
        assert_eq!(delta["usage"]["input_tokens"], 75);
        assert_eq!(delta["usage"]["cache_creation_input_tokens"], 5);
        assert_eq!(delta["usage"]["cache_read_input_tokens"], 20);
    }

    #[test]
    fn stream_stop_position_does_not_cross_non_text_boundaries() {
        let mut filter = StopSequenceFilter::new(&["END".into()]);
        assert_eq!(filter.push("E"), "");
        assert_eq!(filter.finish(), "E");
        assert_eq!(filter.push("ND"), "ND");

        let mut decoded = DecodedResponse {
            text: "END".into(),
            ..DecodedResponse::default()
        };
        apply_stream_stop(&mut decoded, &filter);
        assert_eq!(decoded.text, "END");
        assert!(decoded.stop_sequence.is_none());

        assert_eq!(filter.push(" END ignored"), " ");
        decoded.text.push_str(" END ignored");
        apply_stream_stop(&mut decoded, &filter);
        assert_eq!(decoded.text, "END ");
        assert_eq!(decoded.stop_sequence.as_deref(), Some("END"));
    }

    #[test]
    fn openai_stream_reports_length_and_detailed_usage() {
        let mut state = ClaudeState::new(
            "chatcmpl-length".into(),
            "model".into(),
            100,
            replay_codec(),
        );
        let decoded = DecodedResponse {
            stop_reason: Some("max_tokens".into()),
            usage: kproxy_kiro::UsageInfo {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 20,
                reasoning_tokens: 7,
                ..kproxy_kiro::UsageInfo::default()
            },
            ..DecodedResponse::default()
        };
        let output = stream_finish(
            &StreamProtocol::OpenAi,
            &mut state,
            &decoded,
            123,
            "model",
            50,
            50,
            ThinkingOutputFormat::Claude,
            true,
        );
        let values = streamed_values(&output);
        assert_eq!(values[0]["choices"][0]["finish_reason"], "length");
        assert_eq!(values[1]["choices"], json!([]));
        assert_eq!(
            values[1]["usage"]["prompt_tokens_details"]["cached_tokens"],
            20
        );
        assert_eq!(
            values[1]["usage"]["completion_tokens_details"]["reasoning_tokens"],
            7
        );
    }

    #[test]
    fn web_search_stream_separates_pending_and_resumed_protocol_phases() {
        let mut pending_state =
            ClaudeState::new("msg_pending".into(), "model".into(), 10, replay_codec());
        let pending = pending_state
            .web_search(&ClaudeWebSearchTrace::pending(
                "srvtoolu_pending".into(),
                json!({"query":"rust"}),
            ))
            .join("");
        assert!(pending.contains("server_tool_use"));
        assert!(!pending.contains("web_search_tool_result"));

        let mut resumed_state =
            ClaudeState::new("msg_resumed".into(), "model".into(), 10, replay_codec());
        let resumed = resumed_state
            .web_search(
                &ClaudeWebSearchTrace::success(
                    "srvtoolu_pending".into(),
                    "rust",
                    kproxy_translate::WebSearchResults::default(),
                )
                .result_only(),
            )
            .join("");
        assert!(!resumed.contains("server_tool_use"));
        assert!(resumed.contains("web_search_tool_result"));
    }

    #[test]
    fn openai_tool_chunks_keep_request_id_and_stable_per_tool_indices() {
        let mut state = ClaudeState::new(
            "chatcmpl-request".into(),
            "model".into(),
            10,
            replay_codec(),
        );
        let first = openai_event(
            &KiroEvent::ToolUse {
                id: "tool-a".into(),
                name: "one".into(),
                input_delta: "{".into(),
                stop: false,
            },
            &mut state,
            123,
            "model",
            ThinkingOutputFormat::Openai,
            &std::collections::HashMap::new(),
        );
        let second = openai_event(
            &KiroEvent::ToolUse {
                id: "tool-b".into(),
                name: "two".into(),
                input_delta: "{}".into(),
                stop: true,
            },
            &mut state,
            123,
            "model",
            ThinkingOutputFormat::Openai,
            &std::collections::HashMap::new(),
        );
        let third = openai_event(
            &KiroEvent::ToolUse {
                id: "tool-a".into(),
                name: "one".into(),
                input_delta: "}".into(),
                stop: true,
            },
            &mut state,
            123,
            "model",
            ThinkingOutputFormat::Openai,
            &std::collections::HashMap::new(),
        );
        assert!(first[0].contains("\"id\":\"chatcmpl-request\""));
        assert!(first[0].contains("\"index\":0"));
        assert!(second[0].contains("\"index\":1"));
        assert!(third[0].contains("\"index\":0"));
        let finished = stream_finish(
            &StreamProtocol::OpenAi,
            &mut state,
            &DecodedResponse::default(),
            123,
            "model",
            100,
            1,
            ThinkingOutputFormat::Openai,
            false,
        );
        assert!(finished[0].contains("\"id\":\"chatcmpl-request\""));
        assert!(!finished[0].contains("\"usage\""));

        let with_usage = stream_finish(
            &StreamProtocol::OpenAi,
            &mut state,
            &DecodedResponse::default(),
            123,
            "model",
            100,
            1,
            ThinkingOutputFormat::Openai,
            true,
        );
        assert_eq!(with_usage.len(), 3);
        assert!(with_usage[1].contains("\"choices\":[]"));
        assert!(with_usage[1].contains("\"usage\""));
    }
}

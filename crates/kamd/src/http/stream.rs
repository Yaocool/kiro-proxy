use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use kam_core::config::ThinkingOutputFormat;
use kam_kiro::{EventStreamDecoder, KiroEvent, KiroResponse};
use kam_pool::AccountLease;
use kam_translate::{auto_continue_payload, KiroPayload, KiroToolUse};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::codec::Decoder;

use crate::meter::{now_secs, CreditReservation, UsageRecord};
use crate::state::AppState;
use crate::stats::{RequestLog, UpstreamAttemptLog};

use super::prompt_cache::PromptCacheProfile;
use super::response::{repair_json, DecodedResponse, OpenAiToolIdentity, ToolLeakFilter};

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
    pub estimated_credits: f64,
    pub max_tokens: u32,
    pub started: Instant,
    pub prompt_cache: Option<PromptCacheProfile>,
    pub payload: KiroPayload,
    pub auto_continue_rounds: u32,
    pub buffer_tool_calls: bool,
    pub tool_call_buffer_delay_ms: u64,
    pub enable_tool_leak_filter: bool,
    pub thinking_output_format: ThinkingOutputFormat,
    pub include_usage_chunk: bool,
    /// Kiro canonical web tool name -> original Claude server tool type.
    pub web_tool_names: std::collections::HashMap<String, String>,
    /// Kiro-normalized tool name -> original OpenAI tool type and name.
    pub openai_tools: std::collections::HashMap<String, OpenAiToolIdentity>,
    pub _connection_guard: crate::state::AdmissionGuard,
    pub _admission_guard: crate::state::AdmissionGuard,
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
        let mut accumulated_usage = kam_kiro::UsageInfo::default();
        let mut claude = ClaudeState::new(context.request_id.clone(), context.model.clone(), context.input_tokens);
        claude.openai_include_usage = context.include_usage_chunk;
        let created = now_secs();
        let mut failed = None;
        let mut data_started = false;
        let mut pre_data_retries = 0;
        let mut attempted_accounts = HashSet::new();
        let mut fallback_model = None::<String>;
        let mut accumulated_text = String::new();
        let mut accumulated_reasoning = String::new();
        let mut payload = context.payload.clone();
        let client_has_tools = payload.conversation_state.current_message.user_input_message
            .user_input_message_context.as_ref().is_some_and(|context| !context.tools.is_empty());
        let mut auto_round = 0;
        'rounds: loop {
            let mut leak_filter = ToolLeakFilter::new(context.enable_tool_leak_filter);
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
                                        for mut event in leak_filter.push(event) {
                                            restore_web_tool_name(&mut event, &context.web_tool_names);
                                            if !should_buffer_tool_event(
                                                &event,
                                                context.buffer_tool_calls,
                                                &context.openai_tools,
                                            ) {
                                                let output = stream_event(
                                                    &protocol,
                                                    &mut claude,
                                                    &event,
                                                    created,
                                                    &context.model,
                                                    context.thinking_output_format,
                                                    &context.openai_tools,
                                                );
                                                data_started |= !output.is_empty();
                                                for data in output {
                                                    yield Ok::<Bytes, Infallible>(Bytes::from(data));
                                                }
                                            }
                                            if let Err(error) = decoded.push(event) {
                                                failed = Some(error);
                                                break;
                                            }
                                        }
                                    }
                                    Ok(None) => break,
                                    Err(error) => {
                                        failed = Some(error.to_string());
                                        break;
                                    }
                                }
                            }
                            if failed.is_some() { break; }
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
            for mut event in leak_filter.finish() {
                restore_web_tool_name(&mut event, &context.web_tool_names);
                if !should_buffer_tool_event(
                    &event,
                    context.buffer_tool_calls,
                    &context.openai_tools,
                ) {
                    let output = stream_event(
                        &protocol,
                        &mut claude,
                        &event,
                        created,
                        &context.model,
                        context.thinking_output_format,
                        &context.openai_tools,
                    );
                    data_started |= !output.is_empty();
                    for data in output {
                        yield Ok::<Bytes, Infallible>(Bytes::from(data));
                    }
                }
                if let Err(error) = decoded.push(event) {
                    failed = Some(error);
                    break;
                }
            }
            if failed.is_some() {
                let failure_text = failed.as_deref().unwrap_or_default().to_string();
                let failed_account_id = context.lease.account_id();
                let is_auth = kam_kiro::client::text_is_auth_error(&failure_text);
                let is_quota = kam_kiro::client::text_is_quota_error(&failure_text);
                let is_throttle = kam_kiro::client::text_is_throttle_error(&failure_text);
                tracing::warn!(
                    trace_id = %context.trace_id,
                    request_id = %context.request_id,
                    account_id = %failed_account_id,
                    endpoint,
                    data_started,
                    auth_error = is_auth,
                    quota_error = is_quota,
                    throttle_error = is_throttle,
                    error = %kam_translate::sanitize_error_message(&failure_text),
                    "upstream stream failed"
                );
                let config = context.state.config.current();
                let retry_limit = config
                    .upstream
                    .max_retries
                    .max(context.state.pool().snapshot().await.len() as u32);
                let may_switch = !is_quota || config.pool.auto_switch_on_quota_exhausted;
                if !data_started && pre_data_retries < retry_limit && may_switch && is_auth {
                    let mut disable_account = true;
                    if context
                        .state
                        .refresh_account_token(&context.state.pool(), &failed_account_id, true)
                        .await
                        .is_ok()
                    {
                        tracing::info!(
                            trace_id = %context.trace_id,
                            request_id = %context.request_id,
                            account_id = %failed_account_id,
                            "stream account token refreshed"
                        );
                        if let Err(error) = crate::tasks::persist_pool_accounts(&context.state).await {
                            failed = Some(error.to_string());
                            disable_account = false;
                        } else {
                            let account = context.lease.account().await;
                            payload.profile_arn.clone_from(&account.profile_arn);
                            match context.state.kiro().generate(&account, &payload, None).await {
                                Ok(retry) => {
                                    let (next_endpoint, next_response, next_permit) = retry.into_parts();
                                    endpoint = next_endpoint.name.to_string();
                                    source = next_response.bytes_stream();
                                    upstream_permit = next_permit;
                                    buffer.clear();
                                    decoder = EventStreamDecoder;
                                    decoded = DecodedResponse::default();
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
                    }
                    if disable_account {
                        context.state.pool().mark_banned(&failed_account_id).await;
                        let mut event = kam_notify::WebhookEvent::new(
                            kam_notify::WebhookEventKind::AccountBanned,
                            "Kiro account disabled",
                            "Token refresh failed after a streaming authentication error",
                        );
                        event.account_id = Some(failed_account_id.clone());
                        context.state.notifier().emit(event);
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
                        fallback_model = Some(fallback.clone());
                        context.mapped_model.clone_from(&fallback);
                        context.kiro_model.clone_from(&fallback);
                        super::handlers::set_payload_model(&mut payload, &fallback);
                        let account = context.lease.account().await;
                        match context.state.kiro().generate(&account, &payload, None).await {
                            Ok(retry) => {
                                let (next_endpoint, next_response, next_permit) = retry.into_parts();
                                endpoint = next_endpoint.name.to_string();
                                source = next_response.bytes_stream();
                                upstream_permit = next_permit;
                                buffer.clear();
                                decoder = EventStreamDecoder;
                                decoded = DecodedResponse::default();
                                failed = None;
                                pre_data_retries += 1;
                                continue 'rounds;
                            }
                            Err(error) => failed = Some(error.to_string()),
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
                        runtime.health() == kam_pool::AccountHealth::Exhausted
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
                    }
                } else if !is_auth {
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
                                    super::handlers::trigger_quota_shutdown(
                                        &context.state,
                                        "All compatible Kiro accounts have exhausted their credit allowance",
                                    );
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
                            kam_translate::model::map_model(
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
                        }
                        if incompatible {
                            attempted_accounts.insert(account.id);
                            drop(new_lease);
                            continue;
                        }
                        break (new_lease, account);
                    };
                    context.lease = new_lease;
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
                    match context.state.kiro().generate(&account, &payload, None).await {
                        Ok(retry) => {
                            let (next_endpoint, next_response, next_permit) = retry.into_parts();
                            endpoint = next_endpoint.name.to_string();
                            source = next_response.bytes_stream();
                            upstream_permit = next_permit;
                            buffer.clear();
                            decoder = EventStreamDecoder;
                            decoded = DecodedResponse::default();
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
            let should_continue = !client_has_tools
                && !decoded.tools.is_empty()
                && auto_round < context.auto_continue_rounds;
            if let Err(error) = decoded.validate_tool_inputs() {
                failed = Some(error.clone());
                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error)));
                break 'rounds;
            }
            if !should_continue {
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
            decoded.usage = kam_kiro::UsageInfo::default();
            if let Err(error) = context.reservation.extend(context.estimated_credits) {
                if matches!(error, crate::meter::MeterError::DailyLimitExceeded) {
                    super::handlers::trigger_quota_shutdown(
                        &context.state,
                        "The service daily credit limit was reached during auto-continuation",
                    );
                }
                failed = Some(error.to_string());
                yield Ok::<Bytes, Infallible>(Bytes::from(stream_error(&protocol, &error.to_string())));
                break 'rounds;
            }
            let account = context.lease.account().await;
            match context.state.kiro().generate(&account, &payload, None).await {
                Ok(next) => {
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
        accumulated_text.push_str(&decoded.text);
        accumulated_reasoning.push_str(&decoded.reasoning);
        decoded.text = accumulated_text;
        decoded.reasoning = accumulated_reasoning;
        let current_round_output_tokens = decoded.usage.output_tokens;
        accumulate_usage(&mut decoded.usage, &accumulated_usage);
        if failed.is_none() {
            if context.buffer_tool_calls {
                if context.tool_call_buffer_delay_ms > 0 && !decoded.tools.is_empty() {
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
        finish_accounting(context, endpoint, decoded, failed).await;
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

fn accumulate_usage(total: &mut kam_kiro::UsageInfo, addition: &kam_kiro::UsageInfo) {
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
    message_started: bool,
    block: Option<(usize, &'static str)>,
    next_index: usize,
    tool_indices: std::collections::HashMap<String, usize>,
    openai_thinking_open: bool,
    openai_include_usage: bool,
}

impl ClaudeState {
    fn new(request_id: String, model: String, input_tokens: u64) -> Self {
        Self {
            request_id,
            model,
            input_tokens,
            message_started: false,
            block: None,
            next_index: 0,
            tool_indices: std::collections::HashMap::new(),
            openai_thinking_open: false,
            openai_include_usage: false,
        }
    }

    fn ensure_message(&mut self, output: &mut Vec<String>) {
        if !self.message_started {
            output.push(sse(&json!({
                "type":"message_start","message":{
                    "id":self.request_id,"type":"message","role":"assistant","content":[],
                    "model":self.model,"stop_reason":Value::Null,"stop_sequence":Value::Null,
                    "usage":{"input_tokens":self.input_tokens,"output_tokens":0}
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
                    "delta":{"type":"signature_delta","signature":kam_translate::SIGNATURE_PLACEHOLDER}
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
                        "delta":{"type":"signature_delta","signature":kam_translate::SIGNATURE_PLACEHOLDER}
                    })));
                }
                output.push(sse(&json!({"type":"content_block_stop","index":index})));
            }
            let stop = if !decoded.tools.is_empty() {
                "tool_use"
            } else if current_round_output_tokens >= u64::from(max_tokens) {
                "max_tokens"
            } else {
                "end_turn"
            };
            output.push(sse(&json!({
                "type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":Value::Null},
                "usage":{"output_tokens":decoded.usage.output_tokens}
            })));
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
            let mut final_chunk = json!({
                "id":claude.request_id,"object":"chat.completion.chunk",
                "created":created,"model":model,"choices":[{"index":0,"delta":{},
                    "finish_reason":if decoded.tools.is_empty(){"stop"}else{"tool_calls"}}]
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
                            "total_tokens":decoded.usage.input_tokens+decoded.usage.output_tokens}
                    })
                ));
            }
            output.push("data: [DONE]\n\n".into());
            output
        }
    }
}

fn stream_error(protocol: &StreamProtocol, message: &str) -> String {
    let safe = kam_translate::sanitize_error_message(message);
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
) {
    if failure.is_none() {
        context
            .state
            .pool()
            .record_success(&context.lease.account_id())
            .await;
    }
    let produced_output = !decoded.text.is_empty()
        || !decoded.reasoning.is_empty()
        || !decoded.tools.is_empty()
        || decoded.usage.output_tokens > 0
        || decoded.usage.credits > 0.0;
    if decoded.usage.input_tokens == 0 {
        decoded.usage.input_tokens = context.input_tokens;
    }
    if decoded.usage.output_tokens == 0 {
        decoded.usage.output_tokens = if failure.is_some() && !produced_output {
            0
        } else {
            ((decoded.text.len() + decoded.reasoning.len()) as u64 / 4).max(1)
        };
    }
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
        (decoded.usage.input_tokens + decoded.usage.output_tokens) as f64 / 1_000.0
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
            error = %kam_translate::sanitize_error_message(error),
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
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_signature_stays_on_the_same_block_and_message_starts_once() {
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10);
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
    fn finish_adds_signature_without_opening_a_second_thinking_block() {
        let mut state = ClaudeState::new("msg_test".into(), "model".into(), 10);
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
        assert!(joined.contains(kam_translate::SIGNATURE_PLACEHOLDER));
    }

    #[test]
    fn openai_tool_chunks_keep_request_id_and_stable_per_tool_indices() {
        let mut state = ClaudeState::new("chatcmpl-request".into(), "model".into(), 10);
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

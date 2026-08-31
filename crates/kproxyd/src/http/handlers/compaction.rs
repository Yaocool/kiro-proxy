use super::{
    compact_target_from_maximum, compact_target_tokens, compaction_summary_payload,
    context_maximum, credits, estimated_credits, execute_upstream, fill_missing_usage,
    prepare_kiro_payload, request_log, sanitize_error_message, upstream_error, usage_record,
    ApiError, AppState, Arc, BytesMut, CancellationToken, CompactionArtifact, CompactionDecision,
    CompactionIterationUsage, CompactionReason, CompactionRequest, CompactionRun,
    CompactionSummaryFailure, ContextLimitError, DecodedResponse, Duration, ErrorFormat,
    EventStreamDecoder, ExecuteError, GeneratedCompactionSummary, Instant, KiroEvent, KiroPayload,
    RequestDiagnostics, StatusCode, StreamExt, UpstreamExecution, Uuid, COMPACTION_CLEANUP_GRACE,
    COMPACTION_USAGE_PATH, MAX_COMPACTION_BACKGROUND_GRACE, MIN_COMPACTION_BACKGROUND_GRACE,
};
use tokio_util::codec::Decoder;

async fn generate_compaction_summary(
    state: &Arc<AppState>,
    trace_id: &str,
    key_id: Option<&str>,
    summary_model: &str,
    payload: kproxy_translate::KiroPayload,
    timeout_ms: u64,
) -> Result<GeneratedCompactionSummary, CompactionSummaryFailure> {
    let owned_state = Arc::clone(state);
    let owned_trace_id = trace_id.to_owned();
    let owned_key_id = key_id.map(str::to_owned);
    let owned_summary_model = summary_model.to_owned();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        generate_compaction_summary_inner(
            &owned_state,
            &owned_trace_id,
            owned_key_id.as_deref(),
            &owned_summary_model,
            payload,
            task_cancel,
        )
        .await
    });
    await_compaction_summary_task_with_policy(
        task,
        timeout_ms,
        cancel,
        compaction_background_grace(timeout_ms),
        COMPACTION_CLEANUP_GRACE,
    )
    .await
}

#[cfg(test)]
pub(super) async fn await_compaction_summary_task(
    task: tokio::task::JoinHandle<Result<GeneratedCompactionSummary, CompactionSummaryFailure>>,
    timeout_ms: u64,
) -> Result<GeneratedCompactionSummary, CompactionSummaryFailure> {
    await_compaction_summary_task_with_policy(
        task,
        timeout_ms,
        CancellationToken::new(),
        compaction_background_grace(timeout_ms),
        COMPACTION_CLEANUP_GRACE,
    )
    .await
}

fn compaction_background_grace(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms)
        .max(MIN_COMPACTION_BACKGROUND_GRACE)
        .min(MAX_COMPACTION_BACKGROUND_GRACE)
}

pub(super) async fn await_compaction_summary_task_with_policy(
    mut task: tokio::task::JoinHandle<Result<GeneratedCompactionSummary, CompactionSummaryFailure>>,
    timeout_ms: u64,
    cancel: CancellationToken,
    background_grace: Duration,
    cleanup_grace: Duration,
) -> Result<GeneratedCompactionSummary, CompactionSummaryFailure> {
    match tokio::time::timeout(Duration::from_millis(timeout_ms), &mut task).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(CompactionSummaryFailure {
            message: format!("Kiro compaction summary task failed: {error}"),
            usage: None,
        }),
        Err(_) => {
            tokio::spawn(async move {
                match tokio::time::timeout(background_grace, &mut task).await {
                    Ok(result) => log_late_compaction_result(result),
                    Err(_) => {
                        cancel.cancel();
                        match tokio::time::timeout(cleanup_grace, &mut task).await {
                            Ok(result) => log_late_compaction_result(result),
                            Err(_) => {
                                task.abort();
                                let _ = task.await;
                                tracing::warn!(
                                    background_grace_ms = background_grace.as_millis() as u64,
                                    cleanup_grace_ms = cleanup_grace.as_millis() as u64,
                                    "timed-out compaction summary exceeded its bounded accounting grace and was aborted"
                                );
                            }
                        }
                    }
                }
            });
            Err(CompactionSummaryFailure {
                message: format!("Kiro compaction summary timed out after {timeout_ms} ms"),
                usage: None,
            })
        }
    }
}

fn log_late_compaction_result(
    result: Result<
        Result<GeneratedCompactionSummary, CompactionSummaryFailure>,
        tokio::task::JoinError,
    >,
) {
    match result {
        Ok(Ok(summary)) => tracing::info!(
            input_tokens = summary.usage.input_tokens,
            output_tokens = summary.usage.output_tokens,
            "timed-out compaction summary completed and was accounted"
        ),
        Ok(Err(error)) => tracing::warn!(
            reason = %sanitize_error_message(&error.message),
            "timed-out compaction summary finished with an error"
        ),
        Err(error) => tracing::warn!(
            %error,
            "timed-out compaction summary task could not be joined"
        ),
    }
}

async fn generate_compaction_summary_inner(
    state: &Arc<AppState>,
    trace_id: &str,
    key_id: Option<&str>,
    summary_model: &str,
    payload: kproxy_translate::KiroPayload,
    cancel: CancellationToken,
) -> Result<GeneratedCompactionSummary, CompactionSummaryFailure> {
    let started = Instant::now();
    let input_tokens = state.tokenizer.estimate_kiro_payload(&payload).await? as u64;
    let max_output_tokens = payload
        .inference_config
        .as_ref()
        .map_or(1, |inference| inference.max_tokens);
    let estimate = estimated_credits(
        input_tokens,
        max_output_tokens,
        &state.config.current().pool,
    );
    let reservation = state
        .meter
        .reserve(key_id, estimate)
        .map_err(|error| error.to_string())?;
    let default_model = state.config.current().features.default_model_id.clone();
    let execution = tokio::select! {
        result = execute_upstream(
            state,
            trace_id,
            summary_model,
            summary_model,
            key_id,
            &default_model,
            estimate,
            input_tokens,
            true,
            &payload,
        ) => result.map_err(execute_error_message)?,
        _ = cancel.cancelled() => {
            return Err(CompactionSummaryFailure {
                message: "Kiro compaction summary canceled after its accounting grace expired".into(),
                usage: None,
            });
        }
    };
    let UpstreamExecution {
        mut lease,
        response,
        upstream_access_token: _,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        payload,
    } = execution;
    let account_id = lease.account_id();
    let account_name = lease.account().await.display_name().to_owned();
    let (endpoint_definition, response, upstream_permit) = response.into_parts();
    let endpoint = endpoint_definition.name.to_string();
    let mut source = response.bytes_stream();
    let mut buffer = BytesMut::new();
    let mut decoder = EventStreamDecoder;
    let mut decoded = DecodedResponse::default();
    let mut collection_error = None;
    'collect: loop {
        let chunk = tokio::select! {
            chunk = source.next() => chunk,
            _ = cancel.cancelled() => {
                collection_error = Some(
                    "Kiro compaction summary canceled after its accounting grace expired".into(),
                );
                break 'collect;
            }
        };
        match chunk {
            Some(Ok(chunk)) => {
                buffer.extend_from_slice(&chunk);
                loop {
                    match decoder.decode(&mut buffer) {
                        Ok(Some(KiroEvent::Error { kind, message })) => {
                            collection_error = Some(format!("{kind}: {message}"));
                            break 'collect;
                        }
                        Ok(Some(event)) => {
                            if let Err(error) = decoded.push(event) {
                                collection_error = Some(error);
                                break 'collect;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            collection_error = Some(error.to_string());
                            break 'collect;
                        }
                    }
                }
            }
            Some(Err(error)) => {
                collection_error = Some(error.to_string());
                break 'collect;
            }
            None => break,
        }
    }
    if collection_error.is_none() {
        loop {
            match decoder.decode_eof(&mut buffer) {
                Ok(Some(KiroEvent::Error { kind, message })) => {
                    collection_error = Some(format!("{kind}: {message}"));
                    break;
                }
                Ok(Some(event)) => {
                    if let Err(error) = decoded.push(event) {
                        collection_error = Some(error);
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    collection_error = Some(error.to_string());
                    break;
                }
            }
        }
    }
    drop(source);
    drop(upstream_permit);
    fill_missing_usage(state, &mut decoded, &payload).await;
    let parsed_summary = if let Some(error) = collection_error.clone() {
        Err(error)
    } else if decoded.tools.is_empty() {
        parse_compaction_summary(&decoded.text)
    } else {
        Err("Kiro compaction summary unexpectedly returned a tool call".into())
    };
    let credits = credits(state, &kiro_model, &decoded);
    lease.settle_credits(credits).await;
    let settlement_error = reservation
        .settle(usage_record(
            &mapped_model,
            summary_model,
            &kiro_model,
            COMPACTION_USAGE_PATH,
            &decoded,
            credits,
        ))
        .await
        .err()
        .map(|error| error.to_string());
    let mut log = request_log(
        trace_id,
        &format!("cmp_{}", Uuid::new_v4().simple()),
        COMPACTION_USAGE_PATH,
        &mapped_model,
        summary_model,
        &kiro_model,
        &account_id,
        &account_name,
        &endpoint,
        &model_path,
        model_mapping_rule.as_deref(),
        attempts,
        started,
        RequestDiagnostics {
            payload_bytes: serde_json::to_vec(&payload).map_or(0, |value| value.len()),
            ..RequestDiagnostics::default()
        },
        &decoded,
        credits,
    );
    if let Some(error) = collection_error.as_deref() {
        log.status = 502;
        log.error = Some(sanitize_error_message(error));
        log.diagnostics.client_status = 502;
        log.diagnostics.upstream_status = None;
        log.diagnostics.error_code = "compaction_stream_error".into();
        log.diagnostics.error_stage = "upstream_stream".into();
    }
    state.stats.record(log);
    let usage = CompactionIterationUsage {
        input_tokens: decoded.usage.input_tokens,
        output_tokens: decoded.usage.output_tokens,
    };
    if let Some(error) = settlement_error {
        tracing::error!(
            trace_id,
            account_id,
            summary_model,
            reason = %sanitize_error_message(&error),
            "Kiro compaction summary usage settlement failed"
        );
        return Err(CompactionSummaryFailure {
            message: error,
            usage: Some(usage),
        });
    }
    if let Some(error) = collection_error.as_deref() {
        tracing::warn!(
            trace_id,
            account_id,
            account_name,
            summary_model,
            mapped_model,
            kiro_model,
            endpoint,
            input_tokens = decoded.usage.input_tokens,
            output_tokens = decoded.usage.output_tokens,
            credits,
            duration_ms = started.elapsed().as_millis() as u64,
            reason = %sanitize_error_message(error),
            "Kiro semantic compaction summary failed after partial usage was accounted"
        );
    } else {
        tracing::info!(
            trace_id,
            account_id,
            account_name,
            summary_model,
            mapped_model,
            kiro_model,
            endpoint,
            input_tokens = decoded.usage.input_tokens,
            output_tokens = decoded.usage.output_tokens,
            credits,
            duration_ms = started.elapsed().as_millis() as u64,
            "Kiro semantic compaction summary completed"
        );
    }
    match parsed_summary {
        Ok(content) => Ok(GeneratedCompactionSummary { content, usage }),
        Err(message) => Err(CompactionSummaryFailure {
            message,
            usage: Some(usage),
        }),
    }
}

pub(super) fn parse_compaction_summary(output: &str) -> Result<String, String> {
    let output = output.trim();
    if output.is_empty() {
        return Err("Kiro returned an empty compaction summary".into());
    }
    if let Some(open) = output.find("<summary>") {
        let content_start = open + "<summary>".len();
        let close = output[content_start..]
            .find("</summary>")
            .map(|offset| content_start + offset)
            .ok_or_else(|| "Kiro returned an unterminated <summary> block".to_owned())?;
        let summary = output[content_start..close].trim();
        if summary.is_empty() {
            return Err("Kiro returned an empty <summary> block".into());
        }
        return Ok(summary.to_owned());
    }
    let output = output
        .strip_prefix("```markdown")
        .or_else(|| output.strip_prefix("```"))
        .unwrap_or(output);
    let output = output.strip_suffix("```").unwrap_or(output).trim();
    if output.is_empty() {
        Err("Kiro returned an empty compaction summary".into())
    } else {
        Ok(output.to_owned())
    }
}

pub(super) fn initial_compaction_decision(
    state: &Arc<AppState>,
    model: &str,
    input_tokens: u64,
    client_trigger: Option<u64>,
    auto_compact_on_overflow: bool,
) -> Option<CompactionDecision> {
    let mapped_maximum = context_maximum(state, false, model);
    let mut reasons = Vec::new();
    let mut triggers = Vec::new();
    if let Some(trigger) = client_trigger
        .map(|trigger| trigger.min(mapped_maximum))
        .filter(|trigger| input_tokens >= *trigger)
    {
        reasons.push(CompactionReason::ClientTrigger);
        triggers.push(trigger);
    }
    if auto_compact_on_overflow && input_tokens > mapped_maximum {
        reasons.push(CompactionReason::MappedWindowOverflow);
        triggers.push(mapped_maximum);
    }
    let trigger_tokens = triggers.into_iter().min()?;
    Some(CompactionDecision {
        reasons,
        model: model.to_owned(),
        trigger_tokens,
        target_tokens: compact_target_tokens(state, model, trigger_tokens),
        maximum_tokens: context_maximum(state, true, model),
    })
}

pub(super) fn resolved_compaction_decision(limit: &ContextLimitError) -> CompactionDecision {
    CompactionDecision {
        reasons: vec![CompactionReason::ResolvedWindowOverflow],
        model: limit.model.clone(),
        trigger_tokens: limit.maximum,
        target_tokens: compact_target_from_maximum(limit.maximum),
        maximum_tokens: limit.maximum,
    }
}

pub(super) fn upstream_overflow_compaction_decision(
    state: &Arc<AppState>,
    model: &str,
    input_tokens: u64,
) -> Option<CompactionDecision> {
    if input_tokens <= 1 {
        return None;
    }
    let context = &state.config.current().context;
    // An upstream overflow means the dynamic model metadata is not a safe
    // authority for this request. Fall back to the operator-controlled default
    // window and force meaningful progress even when the request is already
    // below that default.
    let configured_maximum =
        (f64::from(context.max_input_tokens) * context.safe_input_ratio) as u64;
    let maximum_tokens = configured_maximum
        .max(1)
        .min(input_tokens.saturating_sub(1));
    Some(CompactionDecision {
        reasons: vec![CompactionReason::UpstreamWindowOverflow],
        model: model.to_owned(),
        trigger_tokens: maximum_tokens,
        target_tokens: compact_target_from_maximum(maximum_tokens),
        maximum_tokens,
    })
}

fn conservative_summary_context_maximum(state: &Arc<AppState>, model: &str) -> u64 {
    let context = &state.config.current().context;
    let configured =
        (f64::from(context.max_input_tokens) * context.compact_safe_input_ratio) as u64;
    configured.max(1).min(context_maximum(state, true, model))
}

async fn extractive_compaction(
    state: &Arc<AppState>,
    source_payload: &KiroPayload,
    target_tokens: u64,
    preserve_recent_turns: usize,
) -> Result<(KiroPayload, kproxy_translate::ContextCompactionStats), ApiError> {
    let mut payload = source_payload.clone();
    let mut stats = state
        .tokenizer
        .compact_kiro_payload(&mut payload, target_tokens as usize, preserve_recent_turns)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })?;
    finalize_compaction_payload(state, &mut payload, &mut stats, "extractive compaction").await?;
    Ok((payload, stats))
}

async fn finalize_compaction_payload(
    state: &Arc<AppState>,
    payload: &mut KiroPayload,
    stats: &mut kproxy_translate::ContextCompactionStats,
    stage: &str,
) -> Result<(), ApiError> {
    prepare_kiro_payload(payload, "compaction", stage).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            error.message,
            ErrorFormat::Claude,
        )
    })?;
    stats.compacted_tokens = state
        .tokenizer
        .estimate_kiro_payload(payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })?;
    Ok(())
}

pub(super) async fn compaction_operation_target(
    state: &Arc<AppState>,
    source_payload: &KiroPayload,
    decision: &CompactionDecision,
) -> Result<u64, ApiError> {
    let mut minimum_payload = source_payload.clone();
    minimum_payload.retain_protected_history();
    let minimum_tokens = state
        .tokenizer
        .estimate_kiro_payload(&minimum_payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64;
    if minimum_tokens > decision.maximum_tokens {
        return Err(upstream_error(
            ExecuteError::ContextLimit(ContextLimitError {
                model: decision.model.clone(),
                input_tokens: minimum_tokens,
                maximum: decision.maximum_tokens,
            }),
            ErrorFormat::Claude,
        ));
    }
    Ok(if minimum_tokens > decision.target_tokens {
        // The 75% target is desirable headroom, not a smaller model window.
        // Prefer the trigger as the relaxed target, but never turn a client
        // trigger into a false hard limit when the indivisible current turn
        // still fits the model's real safe window.
        let relaxed_trigger = decision.trigger_tokens.min(decision.maximum_tokens);
        if minimum_tokens <= relaxed_trigger {
            relaxed_trigger
        } else {
            decision.maximum_tokens
        }
    } else {
        decision.target_tokens
    })
}

pub(super) async fn run_compaction(
    state: &Arc<AppState>,
    request: CompactionRequest<'_>,
) -> Result<CompactionRun, ApiError> {
    let CompactionRequest {
        trace_id,
        key_id,
        source_payload,
        decision,
        summary_model,
        summary_timeout_ms,
        preserve_recent_turns,
    } = request;
    let operation_target = compaction_operation_target(state, source_payload, decision).await?;
    let plan = state
        .tokenizer
        .plan_kiro_compaction(
            source_payload,
            operation_target as usize,
            preserve_recent_turns,
        )
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })?;
    let Some(plan) = plan else {
        let original_tokens = state
            .tokenizer
            .estimate_kiro_payload(source_payload)
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error,
                    ErrorFormat::Claude,
                )
            })?;
        return Ok(CompactionRun {
            payload: source_payload.clone(),
            stats: kproxy_translate::ContextCompactionStats {
                original_tokens,
                compacted_tokens: original_tokens,
                ..kproxy_translate::ContextCompactionStats::default()
            },
            artifact: None,
            mode: "none",
            summary_model: None,
            summary_input_tokens: None,
            fallback_reason: None,
            iteration_usage: None,
        });
    };

    // Never send the original oversized conversation directly to the summary
    // model. Dynamic model metadata can overstate the real upstream window, so
    // first create a bounded local checkpoint using the configured fallback
    // window. The semantic summary then improves that checkpoint instead of
    // recursively failing on the same oversized prompt.
    let summary_context_maximum = conservative_summary_context_maximum(state, summary_model);
    let summary_preprocess_target = compact_target_from_maximum(summary_context_maximum);
    let source_input_tokens = state
        .tokenizer
        .estimate_kiro_payload(source_payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64;
    let (summary_source, summary_preprocessed) = if source_input_tokens > summary_preprocess_target
    {
        let (payload, stats) = extractive_compaction(
            state,
            source_payload,
            summary_preprocess_target,
            preserve_recent_turns,
        )
        .await?;
        tracing::info!(
            trace_id,
            summary_model,
            original_input_tokens = source_input_tokens,
            preprocessed_input_tokens = stats.compacted_tokens,
            preprocess_target_tokens = summary_preprocess_target,
            removed_messages = stats.removed_messages,
            "compaction summary input preprocessed locally"
        );
        (payload, true)
    } else {
        (source_payload.clone(), false)
    };
    let summary_payload = compaction_summary_payload(&summary_source, &plan, summary_model);
    let summary_input_tokens = state
        .tokenizer
        .estimate_kiro_payload(&summary_payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })? as u64;
    let capacity_insufficient = summary_input_tokens > summary_context_maximum;
    let mut fallback_reason = None;
    let mut semantic_summary = None;
    let mut iteration_usage = None;
    if capacity_insufficient {
        fallback_reason = Some("summary_capacity_insufficient");
        tracing::warn!(
            trace_id,
            summary_model,
            summary_input_tokens,
            summary_context_maximum,
            summary_preprocessed,
            "semantic compaction summary cannot fit its model; using extractive fallback"
        );
    } else {
        match generate_compaction_summary(
            state,
            trace_id,
            key_id,
            summary_model,
            summary_payload,
            summary_timeout_ms,
        )
        .await
        {
            Ok(summary) => semantic_summary = Some(summary),
            Err(error) => {
                iteration_usage = error.usage;
                fallback_reason = Some(if error.message.contains("timed out") {
                    "summary_timeout"
                } else {
                    "summary_upstream_error"
                });
                tracing::warn!(
                    trace_id,
                    reason = %sanitize_error_message(&error.message),
                    "semantic compaction request failed; using extractive fallback"
                );
            }
        }
    }

    if let Some(generated) = semantic_summary {
        iteration_usage = Some(generated.usage);
        let mut payload = source_payload.clone();
        match state
            .tokenizer
            .apply_semantic_compaction(
                &mut payload,
                &plan,
                &generated.content,
                operation_target as usize,
            )
            .await
        {
            Ok(mut stats) => {
                finalize_compaction_payload(state, &mut payload, &mut stats, "semantic compaction")
                    .await?;
                if stats.compacted_tokens as u64 <= operation_target {
                    return Ok(CompactionRun {
                        payload,
                        stats,
                        artifact: Some(CompactionArtifact::Semantic {
                            source_payload: source_payload.clone(),
                            plan,
                            summary: generated.content,
                            usage: generated.usage,
                        }),
                        mode: "semantic",
                        summary_model: Some(summary_model.to_owned()),
                        summary_input_tokens: Some(summary_input_tokens),
                        fallback_reason: None,
                        iteration_usage: Some(generated.usage),
                    });
                }
                fallback_reason = Some("semantic_target_not_reached_after_tool_history_repair");
                tracing::warn!(
                    trace_id,
                    compacted_tokens = stats.compacted_tokens,
                    target_tokens = operation_target,
                    "semantic compaction exceeded the target after tool-history preparation; using extractive fallback"
                );
            }
            Err(error) => {
                fallback_reason = Some("semantic_target_not_reached");
                tracing::warn!(
                    trace_id,
                    reason = %sanitize_error_message(&error),
                    "semantic compaction could not satisfy the target; using extractive fallback"
                );
            }
        }
    }

    let (payload, stats) = extractive_compaction(
        state,
        source_payload,
        operation_target,
        preserve_recent_turns,
    )
    .await?;
    if stats.compacted_tokens as u64 > decision.maximum_tokens {
        return Err(upstream_error(
            ExecuteError::ContextLimit(ContextLimitError {
                model: decision.model.clone(),
                input_tokens: stats.compacted_tokens as u64,
                maximum: decision.maximum_tokens,
            }),
            ErrorFormat::Claude,
        ));
    }
    Ok(CompactionRun {
        payload,
        stats,
        artifact: Some(CompactionArtifact::Extractive {
            source_payload: source_payload.clone(),
            preserve_recent_turns,
            usage: iteration_usage,
        }),
        mode: "extractive_fallback",
        summary_model: Some(summary_model.to_owned()),
        summary_input_tokens: Some(summary_input_tokens),
        fallback_reason,
        iteration_usage,
    })
}

pub(super) async fn reapply_compaction(
    state: &Arc<AppState>,
    artifact: &CompactionArtifact,
    decision: &CompactionDecision,
) -> Result<CompactionRun, ApiError> {
    let source_payload = match artifact {
        CompactionArtifact::Semantic { source_payload, .. }
        | CompactionArtifact::Extractive { source_payload, .. } => source_payload,
    };
    let operation_target = compaction_operation_target(state, source_payload, decision).await?;
    let (payload, stats, mode, iteration_usage) = match artifact {
        CompactionArtifact::Semantic {
            source_payload,
            plan,
            summary,
            usage,
        } => {
            let mut payload = source_payload.clone();
            let first = state
                .tokenizer
                .apply_semantic_compaction(&mut payload, plan, summary, operation_target as usize)
                .await;
            let mut stats = match first {
                Ok(stats) => stats,
                Err(error) if operation_target < decision.maximum_tokens => {
                    tracing::warn!(
                        model = %decision.model,
                        target_tokens = operation_target,
                        safe_window = decision.maximum_tokens,
                        reason = %sanitize_error_message(&error),
                        "semantic artifact missed the preferred target; retrying at the safe window"
                    );
                    payload.clone_from(source_payload);
                    match state
                        .tokenizer
                        .apply_semantic_compaction(
                            &mut payload,
                            plan,
                            summary,
                            decision.maximum_tokens as usize,
                        )
                        .await
                    {
                        Ok(stats) => stats,
                        Err(error) => {
                            let actual = state
                                .tokenizer
                                .estimate_kiro_payload(&payload)
                                .await
                                .unwrap_or(decision.maximum_tokens as usize + 1)
                                as u64;
                            tracing::warn!(
                                model = %decision.model,
                                safe_window = decision.maximum_tokens,
                                reason = %sanitize_error_message(&error),
                                "semantic artifact could not fit the resolved context window"
                            );
                            return Err(upstream_error(
                                ExecuteError::ContextLimit(ContextLimitError {
                                    model: decision.model.clone(),
                                    input_tokens: actual,
                                    maximum: decision.maximum_tokens,
                                }),
                                ErrorFormat::Claude,
                            ));
                        }
                    }
                }
                Err(error) => {
                    let actual = state
                        .tokenizer
                        .estimate_kiro_payload(&payload)
                        .await
                        .unwrap_or(decision.maximum_tokens as usize + 1)
                        as u64;
                    tracing::warn!(
                        model = %decision.model,
                        safe_window = decision.maximum_tokens,
                        reason = %sanitize_error_message(&error),
                        "semantic artifact could not fit the resolved context window"
                    );
                    return Err(upstream_error(
                        ExecuteError::ContextLimit(ContextLimitError {
                            model: decision.model.clone(),
                            input_tokens: actual,
                            maximum: decision.maximum_tokens,
                        }),
                        ErrorFormat::Claude,
                    ));
                }
            };
            finalize_compaction_payload(
                state,
                &mut payload,
                &mut stats,
                "reapplied semantic compaction",
            )
            .await?;
            (payload, stats, "semantic", Some(*usage))
        }
        CompactionArtifact::Extractive {
            source_payload,
            preserve_recent_turns,
            usage,
        } => {
            let (payload, stats) = extractive_compaction(
                state,
                source_payload,
                operation_target,
                *preserve_recent_turns,
            )
            .await?;
            (payload, stats, "extractive_fallback", *usage)
        }
    };
    if stats.compacted_tokens as u64 > decision.maximum_tokens {
        return Err(upstream_error(
            ExecuteError::ContextLimit(ContextLimitError {
                model: decision.model.clone(),
                input_tokens: stats.compacted_tokens as u64,
                maximum: decision.maximum_tokens,
            }),
            ErrorFormat::Claude,
        ));
    }
    Ok(CompactionRun {
        payload,
        stats,
        artifact: Some(artifact.clone()),
        mode,
        summary_model: None,
        summary_input_tokens: None,
        fallback_reason: None,
        iteration_usage,
    })
}

fn execute_error_message(error: ExecuteError) -> String {
    match error {
        ExecuteError::Pool(error) => error.to_string(),
        ExecuteError::Upstream(error) => error.to_string(),
        ExecuteError::Dispatch(error) => error.error.to_string(),
        ExecuteError::Meter(error) => error.to_string(),
        ExecuteError::ContextLimit(limit) => format!(
            "compaction summary input is too long for {}: {} > {}",
            limit.model, limit.input_tokens, limit.maximum
        ),
    }
}

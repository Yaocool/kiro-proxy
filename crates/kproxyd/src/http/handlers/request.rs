use super::{
    apply_adaptive_thinking, apply_context_management_edits, authenticate, claude_loaded_tools,
    claude_pending_server_tool_uses, claude_to_kiro, claude_validation_error,
    claude_web_tool_names, compact_trigger_tokens, enforce_claude_user_agent, enforce_context,
    enforce_payload_budget, estimated_credits, execute_kiro_web_search, execute_upstream,
    initial_compaction_decision, loaded_tool_bytes, loaded_tool_count, loaded_tool_names,
    map_model, matches_type_family, model_token_limit, nonstream_claude, nonstream_openai,
    normalize_compaction_boundary, openai_to_kiro, openai_tool_identities, prepare_kiro_payload,
    prepend_attempt_logs, prepend_execute_error_attempts, reapply_compaction,
    remaining_tool_search_budget, reserve_credits, resolved_compaction_decision,
    resume_tool_search_payload, resume_web_search_payload, run_compaction, sanitize_error_message,
    serialized_payload_bytes, stream, thinking_enabled_for_model, upstream_error,
    upstream_overflow_compaction_decision, validate_claude, validate_openai, web_search_error_code,
    ApiError, AppState, Arc, Bytes, ClaudeRequest, ClaudeServerEvent, ClaudeToolSearchCatalog,
    ClaudeWebSearchTrace, CompactionReason, CompactionRequest, Duration, Engine, ErrorFormat,
    ExecuteError, HeaderMap, Instant, IpAddr, OpenAiRequest, RequestDiagnostics, Response,
    ServiceHttpState, StatusCode, StreamContext, StreamExt, StreamProtocol, TranslationOptions,
    UpstreamExecution, Url, Uuid, Value,
};

pub(super) async fn handle_claude(
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
        event = "proxy.authentication.completed",
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
    let compaction_normalization = normalize_compaction_boundary(&mut request);
    let compact_boundary_applied = compaction_normalization.boundary_applied;
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
        event = "proxy.request.validated",
        trace_id = %trace_id,
        protocol = "claude",
        model = %request.model,
        streaming = request.stream,
        message_count = request.messages.len(),
        tool_count = request.tools.len(),
        max_tokens = request.max_tokens,
        removed_noop_compaction_blocks = compaction_normalization.removed_noop_blocks,
        removed_noop_compaction_messages = compaction_normalization.removed_noop_messages,
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
        event = "proxy.adaptive_thinking.decided",
        trace_id = %trace_id,
        enabled = decision.enabled,
        reason = ?decision.reason,
        budget_tokens = decision.budget_tokens,
        "adaptive thinking decision"
    );
    prepare_kiro_payload(
        &mut payload,
        "request-preparation",
        "initial Claude request",
    )
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            error.message,
            ErrorFormat::Claude,
        )
    })?;
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
    let initial_input_tokens = input_tokens;
    let mut compaction_artifact = None;
    let mut auto_compaction_original_input_tokens = None;
    let mut compaction_iteration = None;
    if let Some(decision) = initial_compaction_decision(
        &state,
        &route.mapped,
        input_tokens,
        compact_trigger,
        config.context.auto_compact_on_overflow,
    ) {
        let summary_model = if config.context.compaction_summary_model.trim().is_empty() {
            route.mapped.as_str()
        } else {
            config.context.compaction_summary_model.trim()
        };
        let run = run_compaction(
            &state,
            CompactionRequest {
                trace_id: &trace_id,
                key_id: key_id.as_deref(),
                source_payload: &payload,
                decision: &decision,
                summary_model,
                summary_timeout_ms: config.context.compaction_summary_timeout_ms,
                preserve_recent_turns: config.context.compaction_preserve_recent_turns,
            },
        )
        .await?;
        if decision
            .reasons
            .contains(&CompactionReason::MappedWindowOverflow)
            && run.stats.removed_messages > 0
        {
            auto_compaction_original_input_tokens = Some(initial_input_tokens);
        }
        payload = run.payload;
        input_tokens = run.stats.compacted_tokens as u64;
        compacted |= run.stats.removed_messages > 0;
        compaction_summary = run.stats.summary.clone();
        compaction_iteration = run.iteration_usage;
        compaction_artifact = run.artifact;
        tracing::info!(
            trace_id = %trace_id,
            compaction_reason = %decision.reason_names(),
            compaction_model = %decision.model,
            trigger_tokens = decision.trigger_tokens,
            target_tokens = decision.target_tokens,
            maximum_tokens = decision.maximum_tokens,
            original_input_tokens = run.stats.original_tokens,
            compacted_input_tokens = run.stats.compacted_tokens,
            removed_messages = run.stats.removed_messages,
            compaction_mode = run.mode,
            compaction_summary_model = run.summary_model.as_deref().unwrap_or("none"),
            summary_input_tokens = run.summary_input_tokens.unwrap_or(0),
            summary_output_tokens = run.iteration_usage.map_or(0, |usage| usage.output_tokens),
            fallback_reason = run.fallback_reason.unwrap_or("none"),
            "request context compacted"
        );
    }
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
    let mut diagnostics = RequestDiagnostics {
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
    let mut estimate = estimated_credits(input_tokens, request.max_tokens, &config.pool);
    let reservation = reserve_credits(&state, key_id.as_deref(), estimate, ErrorFormat::Claude)?;
    tracing::info!(
        event = "proxy.request.prepared",
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
    let first_execution = execute_upstream(
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
    .await;
    let (execution, reservation) = match first_execution {
        Ok(execution) => (execution, reservation),
        Err(error) if config.context.auto_compact_on_overflow => {
            let retry_plan = match &error {
                ExecuteError::ContextLimit(limit) => {
                    Some((resolved_compaction_decision(limit), Vec::new(), false))
                }
                ExecuteError::Dispatch(failure) if failure.error.is_context_too_long() => {
                    let model = if failure.context.kiro_model.is_empty() {
                        route.mapped.as_str()
                    } else {
                        failure.context.kiro_model.as_str()
                    };
                    upstream_overflow_compaction_decision(&state, model, input_tokens)
                        .map(|decision| (decision, failure.context.attempts.clone(), true))
                }
                ExecuteError::Upstream(upstream) if upstream.is_context_too_long() => {
                    upstream_overflow_compaction_decision(&state, &route.mapped, input_tokens)
                        .map(|decision| (decision, Vec::new(), true))
                }
                _ => None,
            };
            let Some((decision, previous_attempts, upstream_rejected)) = retry_plan else {
                return Err(upstream_error(error, ErrorFormat::Claude));
            };
            drop(reservation);
            let summary_model = if config.context.compaction_summary_model.trim().is_empty() {
                route.mapped.as_str()
            } else {
                config.context.compaction_summary_model.trim()
            };
            let run = if let Some(artifact) = compaction_artifact.as_ref() {
                reapply_compaction(&state, artifact, &decision).await?
            } else {
                run_compaction(
                    &state,
                    CompactionRequest {
                        trace_id: &trace_id,
                        key_id: key_id.as_deref(),
                        source_payload: &payload,
                        decision: &decision,
                        summary_model,
                        summary_timeout_ms: config.context.compaction_summary_timeout_ms,
                        preserve_recent_turns: config.context.compaction_preserve_recent_turns,
                    },
                )
                .await?
            };
            if run.stats.removed_messages > 0 {
                auto_compaction_original_input_tokens.get_or_insert(initial_input_tokens);
            }
            payload = run.payload;
            input_tokens = run.stats.compacted_tokens as u64;
            compacted |= run.stats.removed_messages > 0;
            compaction_summary = run.stats.summary.clone();
            compaction_iteration = run.iteration_usage;
            let replanned_tool_tokens = (state
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
            let replanned_payload_bytes = serialized_payload_bytes(&payload, ErrorFormat::Claude)?;
            enforce_payload_budget(
                &state,
                replanned_tool_tokens,
                replanned_payload_bytes,
                loaded_tool_count(&payload),
                tool_search.is_some(),
                ErrorFormat::Claude,
            )?;
            enforce_context(
                &state,
                input_tokens,
                true,
                &decision.model,
                ErrorFormat::Claude,
            )?;
            diagnostics.tool_tokens = replanned_tool_tokens;
            diagnostics.payload_bytes = replanned_payload_bytes;
            diagnostics.loaded_tool_count = loaded_tool_count(&payload);
            diagnostics.loaded_tool_bytes = loaded_tool_bytes(&payload);
            estimate = estimated_credits(input_tokens, request.max_tokens, &config.pool);
            let reservation =
                reserve_credits(&state, key_id.as_deref(), estimate, ErrorFormat::Claude)?;
            tracing::info!(
                trace_id = %trace_id,
                compaction_reason = %decision.reason_names(),
                resolved_model = %decision.model,
                resolved_context_maximum = decision.maximum_tokens,
                target_tokens = decision.target_tokens,
                original_input_tokens = run.stats.original_tokens,
                compacted_input_tokens = run.stats.compacted_tokens,
                removed_messages = run.stats.removed_messages,
                compaction_mode = run.mode,
                compaction_summary_model = run.summary_model.as_deref().unwrap_or("reused"),
                summary_input_tokens = run.summary_input_tokens.unwrap_or(0),
                summary_output_tokens = run.iteration_usage.map_or(0, |usage| usage.output_tokens),
                fallback_reason = run.fallback_reason.unwrap_or("none"),
                resolved_replanned = !upstream_rejected,
                upstream_overflow_retry = upstream_rejected,
                "request context replanned for automatic overflow retry"
            );
            let retry_execution = execute_upstream(
                &state,
                &trace_id,
                &route.mapped,
                &request.model,
                key_id.as_deref(),
                &config.features.default_model_id,
                estimate,
                input_tokens,
                true,
                &payload,
            )
            .await;
            let execution = match retry_execution {
                Ok(mut execution) => {
                    prepend_attempt_logs(&mut execution.attempts, previous_attempts);
                    execution
                }
                Err(mut retry_error) => {
                    prepend_execute_error_attempts(&mut retry_error, previous_attempts);
                    return Err(upstream_error(retry_error, ErrorFormat::Claude));
                }
            };
            (execution, reservation)
        }
        Err(error) => return Err(upstream_error(error, ErrorFormat::Claude)),
    };
    let prompt_cache = if config.features.enable_prompt_cache {
        if compaction_summary.is_some() {
            state
                .prompt_cache
                .claude_compacted_profile(&state.tokenizer, &request, input_tokens)
                .await
        } else {
            state.prompt_cache.claude_profile(&request, input_tokens)
        }
    } else {
        None
    };
    let UpstreamExecution {
        lease,
        response: upstream,
        upstream_access_token,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        payload,
    } = execution;
    if request.stream {
        return Ok(stream::response(
            upstream,
            StreamProtocol::Claude,
            StreamContext {
                state,
                lease,
                upstream_access_token,
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
                compaction_iteration,
                auto_compaction_original_input_tokens,
                estimated_credits: estimate,
                max_tokens: request.max_tokens,
                stop_sequences: request.stop_sequences.clone(),
                started,
                prompt_cache,
                payload,
                auto_continue_rounds: config.features.auto_continue_rounds.min(30),
                buffer_tool_calls: config.features.buffer_tool_calls,
                tool_call_buffer_delay_ms: config.features.tool_call_buffer_delay_ms,
                enable_tool_leak_filter: config.features.enable_tool_leak_filter,
                thinking_enabled: decision.enabled,
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
        decision.enabled,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        input_tokens,
        compacted,
        started,
        prompt_cache,
        compaction_summary,
        compaction_iteration,
        auto_compaction_original_input_tokens,
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

pub(super) async fn handle_openai(
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
        event = "proxy.authentication.completed",
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
        event = "proxy.request.validated",
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
        event = "proxy.adaptive_thinking.decided",
        trace_id = %trace_id,
        enabled = decision.enabled,
        reason = ?decision.reason,
        budget_tokens = decision.budget_tokens,
        "adaptive thinking decision"
    );
    prepare_kiro_payload(
        &mut payload,
        "request-preparation",
        "initial OpenAI request",
    )
    .map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            error.message,
            ErrorFormat::OpenAi,
        )
    })?;
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
        event = "proxy.request.prepared",
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
        upstream_access_token,
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
                upstream_access_token,
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
                compaction_iteration: None,
                auto_compaction_original_input_tokens: None,
                estimated_credits: estimate,
                max_tokens,
                stop_sequences: Vec::new(),
                started,
                prompt_cache,
                payload,
                auto_continue_rounds: config.features.auto_continue_rounds.min(30),
                buffer_tool_calls: config.features.buffer_tool_calls,
                tool_call_buffer_delay_ms: config.features.tool_call_buffer_delay_ms,
                enable_tool_leak_filter: config.features.enable_tool_leak_filter,
                thinking_enabled: decision.enabled,
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
        decision.enabled,
        mapped_model,
        kiro_model,
        model_path,
        model_mapping_rule,
        attempts,
        input_tokens,
        max_tokens,
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

pub(super) fn is_public_address(address: IpAddr) -> bool {
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

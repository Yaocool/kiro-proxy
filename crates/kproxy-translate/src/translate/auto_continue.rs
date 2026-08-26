use crate::{
    ClaudeToolSearchOutcome, KiroAssistantMessage, KiroHistoryMessage, KiroMessageContext,
    KiroPayload, KiroText, KiroToolResult, KiroToolUse,
};

use super::common::history_user_without_tools;

/// Completes an internal Tool Search call, loads only the matching definitions,
/// and asks Kiro to continue the same assistant turn.
pub fn tool_search_continue_payload(
    payload: &KiroPayload,
    assistant_content: &str,
    tool_use: KiroToolUse,
    outcome: &ClaudeToolSearchOutcome,
) -> KiroPayload {
    tool_search_continue_payload_batch(payload, assistant_content, &[(tool_use, outcome.clone())])
}

/// Completes multiple Tool Search calls from one assistant turn without
/// dropping parallel server-tool uses.
pub fn tool_search_continue_payload_batch(
    payload: &KiroPayload,
    assistant_content: &str,
    searches: &[(KiroToolUse, ClaudeToolSearchOutcome)],
) -> KiroPayload {
    let mut next = payload.clone();
    let previous_user = next
        .conversation_state
        .current_message
        .user_input_message
        .clone();
    next.conversation_state.history.push(KiroHistoryMessage {
        user_input_message: Some(history_user_without_tools(previous_user.clone())),
        assistant_response_message: None,
    });
    next.conversation_state.history.push(KiroHistoryMessage {
        user_input_message: None,
        assistant_response_message: Some(KiroAssistantMessage {
            content: if assistant_content.trim().is_empty() {
                "Searching for tools.".into()
            } else {
                tail_chars(assistant_content, 48_000)
            },
            tool_uses: searches
                .iter()
                .map(|(tool_use, _)| tool_use.clone())
                .collect(),
        }),
    });

    let mut tools = previous_user
        .user_input_message_context
        .map(|context| context.tools)
        .unwrap_or_default();
    for (_, outcome) in searches {
        for found in &outcome.tools {
            let name = &found.tool_specification.name;
            if !tools
                .iter()
                .any(|existing| existing.tool_specification.name == *name)
            {
                tools.push(found.clone());
            }
        }
    }
    let tool_results = searches
        .iter()
        .map(|(tool_use, outcome)| tool_search_kiro_result(tool_use, outcome))
        .collect();
    let documentation = searches
        .iter()
        .flat_map(|(_, outcome)| outcome.documentation.iter().cloned())
        .collect::<Vec<_>>();
    let current = &mut next.conversation_state.current_message.user_input_message;
    current.content = if documentation.is_empty() {
        "Continue using the Tool Search result.".into()
    } else {
        format!(
            "Continue using the Tool Search result.\n\n{}",
            documentation.join("\n\n")
        )
    };
    current.images.clear();
    current.user_input_message_context = Some(KiroMessageContext {
        tool_results,
        tools,
    });
    next.truncate_history_preserving_protected_prefix(30);
    next
}

/// Adds the result for a previously emitted pending Tool Search call to the
/// current Kiro user turn.
pub fn resume_tool_search_payload(
    payload: &mut KiroPayload,
    tool_use: &KiroToolUse,
    outcome: &ClaudeToolSearchOutcome,
) {
    let current = &mut payload
        .conversation_state
        .current_message
        .user_input_message;
    let context = current
        .user_input_message_context
        .get_or_insert_with(KiroMessageContext::default);
    for found in &outcome.tools {
        if !context
            .tools
            .iter()
            .any(|existing| existing.tool_specification.name == found.tool_specification.name)
        {
            context.tools.push(found.clone());
        }
    }
    context
        .tool_results
        .push(tool_search_kiro_result(tool_use, outcome));
    if !outcome.documentation.is_empty() {
        current.content.push_str("\n\n");
        current
            .content
            .push_str(&outcome.documentation.join("\n\n"));
    }
}

fn tool_search_kiro_result(
    tool_use: &KiroToolUse,
    outcome: &ClaudeToolSearchOutcome,
) -> KiroToolResult {
    let (status, result) = if let Some(error) = &outcome.trace.error {
        (
            "error",
            format!("Tool Search failed ({}): {}", error.code, error.message),
        )
    } else if outcome.trace.references.is_empty() {
        ("success", "Tool Search found no matching tools.".into())
    } else {
        (
            "success",
            format!(
                "Tool Search loaded these tools: {}. Their complete definitions are now available.",
                outcome.trace.references.join(", ")
            ),
        )
    };
    KiroToolResult {
        content: vec![KiroText { text: result }],
        status: status.into(),
        tool_use_id: tool_use.tool_use_id.clone(),
    }
}

/// Appends a completed assistant tool round and asks Kiro to continue.
pub fn auto_continue_payload(
    payload: &KiroPayload,
    assistant_content: &str,
    tool_uses: Vec<KiroToolUse>,
) -> KiroPayload {
    let mut next = payload.clone();
    let previous_user = next
        .conversation_state
        .current_message
        .user_input_message
        .clone();
    next.conversation_state.history.push(KiroHistoryMessage {
        user_input_message: Some(history_user_without_tools(previous_user.clone())),
        assistant_response_message: None,
    });
    next.conversation_state.history.push(KiroHistoryMessage {
        user_input_message: None,
        assistant_response_message: Some(KiroAssistantMessage {
            content: if assistant_content.trim().is_empty() {
                "Using tools.".into()
            } else {
                tail_chars(assistant_content, 48_000)
            },
            tool_uses: tool_uses.clone(),
        }),
    });
    let tools = previous_user
        .user_input_message_context
        .map(|context| context.tools)
        .unwrap_or_default();
    let results = tool_uses
        .into_iter()
        .map(|tool| KiroToolResult {
            content: vec![KiroText {
                text: "Done. Continue with the next step.".into(),
            }],
            status: "success".into(),
            tool_use_id: tool.tool_use_id,
        })
        .collect();
    let current = &mut next.conversation_state.current_message.user_input_message;
    current.content = "Continue with the next step.".into();
    current.images.clear();
    current.user_input_message_context = Some(KiroMessageContext {
        tool_results: results,
        tools,
    });
    next.truncate_history_preserving_protected_prefix(30);
    next
}

fn tail_chars(value: &str, maximum: usize) -> String {
    let character_count = value.chars().count();
    if character_count <= maximum {
        return value.into();
    }
    value.chars().skip(character_count - maximum).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        claude_to_kiro, ClaudeRequest, ClaudeToolSearchTrace, KiroConversationState,
        KiroCurrentMessage, KiroInputSchema, KiroTool, KiroToolSpecification, KiroUserInputMessage,
        TranslationOptions,
    };
    use serde_json::json;

    #[test]
    fn preserves_conversation_and_adds_tool_results() {
        let payload = KiroPayload {
            conversation_state: KiroConversationState {
                chat_trigger_type: "MANUAL".into(),
                conversation_id: "conversation".into(),
                current_message: KiroCurrentMessage {
                    user_input_message: KiroUserInputMessage {
                        content: "do it".into(),
                        model_id: "model".into(),
                        origin: "CLI".into(),
                        images: vec![],
                        user_input_message_context: None,
                    },
                },
                history: vec![],
            },
            profile_arn: None,
            inference_config: None,
            protected_history_messages: 0,
        };
        let next = auto_continue_payload(
            &payload,
            "working",
            vec![KiroToolUse {
                tool_use_id: "tool_1".into(),
                name: "server_tool".into(),
                input: json!({"x":1}),
            }],
        );
        assert_eq!(next.conversation_state.conversation_id, "conversation");
        assert_eq!(next.conversation_state.history.len(), 2);
        let context = next
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .expect("context");
        assert_eq!(context.tool_results[0].tool_use_id, "tool_1");
    }

    #[test]
    fn long_internal_continuation_preserves_system_prefix() {
        let mut messages = Vec::new();
        for index in 0..16 {
            messages.push(json!({"role":"user","content":format!("user {index}")}));
            messages.push(json!({"role":"assistant","content":format!("assistant {index}")}));
        }
        messages.push(json!({"role":"user","content":"current"}));
        let request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-opus-5",
            "max_tokens":128,
            "system":"You are Claude Code, Anthropic's official CLI for Claude.",
            "messages":messages
        }))
        .expect("request");
        let payload = claude_to_kiro(
            &request,
            &TranslationOptions::new("mapped-opus", "AI_EDITOR"),
        );

        let continued = auto_continue_payload(&payload, "using a tool", Vec::new());
        assert_eq!(continued.protected_history_len(), 2);
        assert_eq!(continued.conversation_state.history.len(), 30);
        assert!(continued.conversation_state.history[0]
            .user_input_message
            .as_ref()
            .expect("protected system")
            .content
            .contains("You are Claude Code"));
        assert_eq!(
            continued.conversation_state.history[1]
                .assistant_response_message
                .as_ref()
                .expect("protected acknowledgement")
                .content,
            super::super::SYSTEM_PROMPT_ACKNOWLEDGEMENT
        );

        let search_use = KiroToolUse {
            tool_use_id: "srvtoolu_1".into(),
            name: "tool_search_tool_regex".into(),
            input: json!({"pattern":"issue"}),
        };
        let outcome = ClaudeToolSearchOutcome {
            trace: ClaudeToolSearchTrace {
                id: search_use.tool_use_id.clone(),
                name: search_use.name.clone(),
                input: search_use.input.clone(),
                references: vec![],
                error: None,
                requested_limit: 5,
                matched_count: 0,
                budget_truncated: false,
                emission: crate::ClaudeServerToolEmission::Complete,
            },
            tools: vec![],
            documentation: vec![],
            truncated: false,
        };
        let searched = tool_search_continue_payload(&payload, "searching", search_use, &outcome);
        assert_eq!(searched.protected_history_len(), 2);
        assert_eq!(searched.conversation_state.history.len(), 30);
        assert!(searched.conversation_state.history[0]
            .user_input_message
            .as_ref()
            .expect("protected system")
            .content
            .contains("You are Claude Code"));
        assert_eq!(
            searched.conversation_state.history[1]
                .assistant_response_message
                .as_ref()
                .expect("protected acknowledgement")
                .content,
            super::super::SYSTEM_PROMPT_ACKNOWLEDGEMENT
        );
    }

    #[test]
    fn tool_search_continuation_loads_only_matched_definitions() {
        let mut payload = KiroPayload {
            conversation_state: KiroConversationState {
                chat_trigger_type: "MANUAL".into(),
                conversation_id: "conversation".into(),
                current_message: KiroCurrentMessage {
                    user_input_message: KiroUserInputMessage {
                        content: "find issues".into(),
                        model_id: "model".into(),
                        origin: "CLI".into(),
                        images: vec![],
                        user_input_message_context: None,
                    },
                },
                history: vec![],
            },
            profile_arn: None,
            inference_config: None,
            protected_history_messages: 0,
        };
        payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context = Some(KiroMessageContext {
            tools: vec![KiroTool {
                tool_specification: KiroToolSpecification {
                    name: "tool_search_tool_regex".into(),
                    description: "search".into(),
                    input_schema: KiroInputSchema {
                        json: serde_json::json!({"type":"object"}),
                    },
                },
            }],
            tool_results: vec![],
        });
        let search_use = KiroToolUse {
            tool_use_id: "srvtoolu_1".into(),
            name: "tool_search_tool_regex".into(),
            input: serde_json::json!({"pattern":"issue"}),
        };
        let outcome = ClaudeToolSearchOutcome {
            trace: ClaudeToolSearchTrace {
                id: "srvtoolu_1".into(),
                name: "tool_search_tool_regex".into(),
                input: search_use.input.clone(),
                references: vec!["mcp__github__list_issues".into()],
                error: None,
                requested_limit: 5,
                matched_count: 1,
                budget_truncated: false,
                emission: crate::ClaudeServerToolEmission::Complete,
            },
            tools: vec![KiroTool {
                tool_specification: KiroToolSpecification {
                    name: "mcp__github__list_issues".into(),
                    description: "List issues".into(),
                    input_schema: KiroInputSchema {
                        json: serde_json::json!({"type":"object"}),
                    },
                },
            }],
            documentation: vec![],
            truncated: false,
        };
        let next = tool_search_continue_payload(&payload, "searching", search_use, &outcome);
        assert!(next.conversation_state.history[0]
            .user_input_message
            .as_ref()
            .expect("history user")
            .user_input_message_context
            .is_none());
        let context = next
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .expect("context");
        assert_eq!(context.tools.len(), 2);
        assert_eq!(context.tool_results[0].tool_use_id, "srvtoolu_1");
        assert!(context.tool_results[0].content[0]
            .text
            .contains("mcp__github__list_issues"));

        let second_use = KiroToolUse {
            tool_use_id: "srvtoolu_2".into(),
            name: "tool_search_tool_regex".into(),
            input: serde_json::json!({"pattern":"pull request"}),
        };
        let mut second_outcome = outcome.clone();
        second_outcome.trace.id = second_use.tool_use_id.clone();
        second_outcome.trace.references = Vec::new();
        second_outcome.tools = Vec::new();
        let batch = tool_search_continue_payload_batch(
            &payload,
            "searching",
            &[
                (
                    KiroToolUse {
                        tool_use_id: "srvtoolu_1".into(),
                        name: "tool_search_tool_regex".into(),
                        input: serde_json::json!({"pattern":"issue"}),
                    },
                    outcome,
                ),
                (second_use, second_outcome),
            ],
        );
        let batch_context = batch
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .expect("batch context");
        assert_eq!(batch_context.tool_results.len(), 2);
    }
}

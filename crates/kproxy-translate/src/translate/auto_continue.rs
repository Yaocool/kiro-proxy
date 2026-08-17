use crate::{
    ClaudeToolSearchOutcome, KiroAssistantMessage, KiroHistoryMessage, KiroMessageContext,
    KiroPayload, KiroText, KiroToolResult, KiroToolUse,
};

/// Completes an internal Tool Search call, loads only the matching definitions,
/// and asks Kiro to continue the same assistant turn.
pub fn tool_search_continue_payload(
    payload: &KiroPayload,
    assistant_content: &str,
    tool_use: KiroToolUse,
    outcome: &ClaudeToolSearchOutcome,
) -> KiroPayload {
    let mut next = payload.clone();
    let previous_user = next
        .conversation_state
        .current_message
        .user_input_message
        .clone();
    next.conversation_state.history.push(KiroHistoryMessage {
        user_input_message: Some(previous_user.clone()),
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
            tool_uses: vec![tool_use.clone()],
        }),
    });

    let mut tools = previous_user
        .user_input_message_context
        .map(|context| context.tools)
        .unwrap_or_default();
    for found in &outcome.tools {
        let name = &found.tool_specification.name;
        if !tools
            .iter()
            .any(|existing| existing.tool_specification.name == *name)
        {
            tools.push(found.clone());
        }
    }
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
    let current = &mut next.conversation_state.current_message.user_input_message;
    current.content = if outcome.documentation.is_empty() {
        "Continue using the Tool Search result.".into()
    } else {
        format!(
            "Continue using the Tool Search result.\n\n{}",
            outcome.documentation.join("\n\n")
        )
    };
    current.images.clear();
    current.user_input_message_context = Some(KiroMessageContext {
        tool_results: vec![KiroToolResult {
            content: vec![KiroText { text: result }],
            status: status.into(),
            tool_use_id: tool_use.tool_use_id,
        }],
        tools,
    });
    if next.conversation_state.history.len() > 30 {
        let remove = next.conversation_state.history.len() - 30;
        next.conversation_state.history.drain(..remove);
    }
    next
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
        user_input_message: Some(previous_user.clone()),
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
    if next.conversation_state.history.len() > 30 {
        let remove = next.conversation_state.history.len() - 30;
        next.conversation_state.history.drain(..remove);
    }
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
        ClaudeToolSearchTrace, KiroConversationState, KiroCurrentMessage, KiroInputSchema,
        KiroTool, KiroToolSpecification, KiroUserInputMessage,
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
        };
        let next = tool_search_continue_payload(&payload, "searching", search_use, &outcome);
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
    }
}

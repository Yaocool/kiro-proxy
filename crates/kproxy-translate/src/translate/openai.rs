use serde_json::{json, Value};

use crate::{
    KiroAssistantMessage, KiroConversationState, KiroCurrentMessage, KiroHistoryMessage,
    KiroPayload, KiroText, KiroToolResult, KiroToolUse, KiroUserInputMessage, OpenAiRequest,
};

use super::common::{
    content_text, context, enhance_system, extract_openai_images, inference, kiro_tool_named,
    ToolNameRegistry,
};
use super::TranslationOptions;

pub fn openai_to_kiro(request: &OpenAiRequest, options: &TranslationOptions) -> KiroPayload {
    let tool_names = ToolNameRegistry::new(request.tools.iter().filter_map(|tool| {
        tool.body
            .get(&tool.r#type)
            .and_then(|definition| definition.get("name"))
            .and_then(Value::as_str)
    }));
    let selected = selected_tools(request);
    let mut documentation = Vec::new();
    let tools = selected
        .iter()
        .filter_map(|tool| {
            let definition = tool.body.get(&tool.r#type)?;
            let name = definition.get("name")?.as_str()?;
            let description = definition
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let schema = if tool.r#type == "custom" {
                json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"]})
            } else {
                definition
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object","properties":{}}))
            };
            let (tool, docs) = kiro_tool_named(
                name,
                &tool_names.kiro_name(name),
                description,
                &schema,
            );
            documentation.extend(docs);
            Some(tool)
        })
        .collect::<Vec<_>>();

    let mut system = request
        .messages
        .iter()
        .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
        .filter_map(|message| message.content.as_ref())
        .map(content_text)
        .collect::<Vec<_>>()
        .join("\n");
    if options.enhance_system_prompt {
        let has_write = tools.iter().any(|tool| {
            matches!(
                tool.tool_specification.name.to_ascii_lowercase().as_str(),
                "write" | "edit" | "multiedit" | "notebookedit"
            )
        });
        system = enhance_system(system, has_write);
    }
    if !documentation.is_empty() {
        system = join(&system, &documentation.join("\n\n"));
    }
    system = join(&system, &choice_directive(request));

    let non_system = request
        .messages
        .iter()
        .filter(|message| !matches!(message.role.as_str(), "system" | "developer"))
        .collect::<Vec<_>>();
    let mut history = Vec::new();
    let mut current_text = String::new();
    let mut current_results = Vec::new();
    let mut current_images = Vec::new();
    let mut system_merged = false;

    for (index, message) in non_system.iter().enumerate() {
        let last = index + 1 == non_system.len();
        match message.role.as_str() {
            "user" => {
                let mut text = message
                    .content
                    .as_ref()
                    .map(content_text)
                    .unwrap_or_default();
                let images = message
                    .content
                    .as_ref()
                    .map(extract_openai_images)
                    .unwrap_or_default();
                if !system_merged {
                    text = join(&system, &text);
                    system_merged = true;
                }
                if last {
                    current_text = text;
                    current_images = images;
                } else {
                    push_user(&mut history, make_user(text, images, Vec::new(), options));
                }
            }
            "assistant" => push_assistant(&mut history, assistant_message(message, &tool_names)),
            "tool" => {
                if let Some(id) = &message.tool_call_id {
                    let text = message
                        .content
                        .as_ref()
                        .map(content_text)
                        .unwrap_or_default();
                    current_results.push(KiroToolResult {
                        content: vec![KiroText { text }],
                        status: "success".into(),
                        tool_use_id: id.clone(),
                    });
                }
                let next_is_tool = non_system
                    .get(index + 1)
                    .is_some_and(|next| next.role == "tool");
                if !last && !next_is_tool {
                    push_user(
                        &mut history,
                        make_user(
                            "Tool results provided.".into(),
                            Vec::new(),
                            current_results.split_off(0),
                            options,
                        ),
                    );
                }
            }
            _ => {}
        }
    }
    if !system_merged {
        current_text = join(&system, &current_text);
    }
    if current_text.trim().is_empty() {
        current_text = if current_results.is_empty() {
            "Continue.".into()
        } else {
            "Tool results provided.".into()
        };
    }
    let current = KiroUserInputMessage {
        content: current_text,
        model_id: options.model_id.clone(),
        origin: options.origin.clone(),
        images: current_images,
        user_input_message_context: context(tools.clone(), current_results),
    };

    KiroPayload {
        conversation_state: KiroConversationState {
            chat_trigger_type: "MANUAL".into(),
            conversation_id: random_id(),
            current_message: KiroCurrentMessage {
                user_input_message: current,
            },
            history,
        },
        profile_arn: options.profile_arn.clone(),
        inference_config: Some(inference(
            request.max_completion_tokens.or(request.max_tokens),
            !tools.is_empty(),
            request.temperature,
            request.top_p,
        )),
        protected_history_messages: 0,
    }
}

fn selected_tools(request: &OpenAiRequest) -> Vec<&crate::OpenAiTool> {
    let Some(choice) = request.tool_choice.as_ref() else {
        return request.tools.iter().collect();
    };
    if choice == "none" {
        return Vec::new();
    }
    if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") {
        let allowed = choice
            .pointer("/allowed_tools/tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|reference| {
                let kind = reference.get("type")?.as_str()?;
                let name = reference.get(kind)?.get("name")?.as_str()?;
                Some((kind, name))
            })
            .collect::<Vec<_>>();
        return request
            .tools
            .iter()
            .filter(|tool| {
                tool.body
                    .get(&tool.r#type)
                    .and_then(|definition| definition.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|name| {
                        allowed.iter().any(|(kind, allowed_name)| {
                            *kind == tool.r#type && *allowed_name == name
                        })
                    })
            })
            .collect();
    }
    let selected_name = choice
        .get("function")
        .or_else(|| choice.get("custom"))
        .and_then(|body| body.get("name"))
        .and_then(Value::as_str);
    match selected_name {
        Some(name) => request
            .tools
            .iter()
            .filter(|tool| {
                tool.body
                    .get(&tool.r#type)
                    .and_then(|body| body.get("name"))
                    .and_then(Value::as_str)
                    == Some(name)
            })
            .collect(),
        None => request.tools.iter().collect(),
    }
}

fn assistant_message(
    message: &crate::OpenAiMessage,
    tool_names: &ToolNameRegistry,
) -> KiroAssistantMessage {
    let mut content = message
        .content
        .as_ref()
        .map(content_text)
        .unwrap_or_default();
    let tool_uses = message
        .tool_calls
        .iter()
        .filter_map(|call| {
            let id = call.get("id")?.as_str()?.to_string();
            let (name, input) = if let Some(function) = call.get("function") {
                let arguments = function.get("arguments")?.as_str().unwrap_or("{}");
                (
                    function.get("name")?.as_str()?,
                    serde_json::from_str(arguments).unwrap_or_else(|_| json!({"raw":arguments})),
                )
            } else {
                let custom = call.get("custom")?;
                (
                    custom.get("name")?.as_str()?,
                    json!({"input":custom.get("input").and_then(Value::as_str).unwrap_or_default()}),
                )
            };
            Some(KiroToolUse {
                tool_use_id: id,
                name: tool_names.kiro_name(name),
                input,
            })
        })
        .collect::<Vec<_>>();
    if content.trim().is_empty() {
        content = if tool_uses.is_empty() {
            "I understand."
        } else {
            "Using tools."
        }
        .into();
    }
    KiroAssistantMessage { content, tool_uses }
}

fn make_user(
    text: String,
    images: Vec<crate::KiroImage>,
    results: Vec<KiroToolResult>,
    options: &TranslationOptions,
) -> KiroUserInputMessage {
    KiroUserInputMessage {
        content: if text.trim().is_empty() {
            "Continue".into()
        } else {
            text
        },
        model_id: options.model_id.clone(),
        origin: options.origin.clone(),
        images,
        user_input_message_context: context(Vec::new(), results),
    }
}

fn push_user(history: &mut Vec<KiroHistoryMessage>, message: KiroUserInputMessage) {
    history.push(KiroHistoryMessage {
        user_input_message: Some(message),
        assistant_response_message: None,
    });
}

fn push_assistant(history: &mut Vec<KiroHistoryMessage>, message: KiroAssistantMessage) {
    history.push(KiroHistoryMessage {
        user_input_message: None,
        assistant_response_message: Some(message),
    });
}

fn choice_directive(request: &OpenAiRequest) -> String {
    let required = request.tool_choice.as_ref().is_some_and(|choice| {
        choice == "required"
            || choice.get("function").is_some()
            || choice.get("custom").is_some()
            || choice
                .pointer("/allowed_tools/mode")
                .and_then(Value::as_str)
                == Some("required")
    });
    let mut parts = Vec::new();
    if required {
        parts.push("You must call at least one of the provided tools.");
    }
    if !request.parallel_tool_calls && !request.tools.is_empty() {
        parts.push(if required {
            "Make exactly one tool call."
        } else {
            "Make at most one tool call."
        });
    }
    parts.join("\n")
}

fn join(left: &str, right: &str) -> String {
    match (left.trim().is_empty(), right.trim().is_empty()) {
        (true, _) => right.trim().into(),
        (_, true) => left.trim().into(),
        _ => format!("{}\n\n{}", left.trim(), right.trim()),
    }
}

fn random_id() -> String {
    use std::fmt::Write as _;

    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_url_images_are_forwarded_for_history_and_current_turn() {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model":"claude-sonnet",
            "messages":[
                {"role":"user","content":[
                    {"type":"text","text":"first"},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,AQID"}}
                ]},
                {"role":"assistant","content":"ok"},
                {"role":"user","content":[
                    {"type":"text","text":"second"},
                    {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,BAUG"}}
                ]}
            ],
            "max_tokens":128
        }))
        .expect("request");
        let payload = openai_to_kiro(
            &request,
            &TranslationOptions::new("claude-sonnet", "AI_EDITOR"),
        );
        let history_image = &payload.conversation_state.history[0]
            .user_input_message
            .as_ref()
            .expect("user")
            .images[0];
        assert_eq!(history_image.format, "png");
        let current = &payload
            .conversation_state
            .current_message
            .user_input_message
            .images[0];
        assert_eq!(current.format, "jpeg");
        assert_eq!(current.source.bytes, "BAUG");
    }

    #[test]
    fn allowed_tools_only_forward_the_declared_subset() {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model":"claude-sonnet",
            "messages":[{"role":"user","content":"use a tool"}],
            "tools":[
                {"type":"function","function":{"name":"one","parameters":{"type":"object"}}},
                {"type":"function","function":{"name":"two","parameters":{"type":"object"}}}
            ],
            "tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"required","tools":[
                {"type":"function","function":{"name":"two"}}
            ]}}
        }))
        .expect("request");
        let payload = openai_to_kiro(
            &request,
            &TranslationOptions::new("claude-sonnet", "AI_EDITOR"),
        );
        let tools = &payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .expect("context")
            .tools;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_specification.name, "two");
    }

    #[test]
    fn colliding_tool_names_use_the_same_registry_for_definitions_and_history() {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model":"claude-sonnet",
            "messages":[
                {"role":"user","content":"look it up"},
                {"role":"assistant","tool_calls":[{
                    "id":"call_1","type":"function",
                    "function":{"name":"mcp.a/read","arguments":"{}"}
                }]},
                {"role":"tool","tool_call_id":"call_1","content":"done"}
            ],
            "tools":[
                {"type":"function","function":{"name":"mcp.a/read","parameters":{"type":"object"}}},
                {"type":"function","function":{"name":"mcp_a/read","parameters":{"type":"object"}}}
            ]
        }))
        .expect("request");

        let payload = openai_to_kiro(
            &request,
            &TranslationOptions::new("claude-sonnet", "AI_EDITOR"),
        );
        let mapped = ToolNameRegistry::new(["mcp.a/read", "mcp_a/read"]).kiro_name("mcp.a/read");
        let context = payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .as_ref()
            .expect("context");
        assert!(context
            .tools
            .iter()
            .any(|tool| tool.tool_specification.name == mapped));
        let history_name = &payload.conversation_state.history[1]
            .assistant_response_message
            .as_ref()
            .expect("assistant")
            .tool_uses[0]
            .name;
        assert_eq!(history_name, &mapped);
    }
}

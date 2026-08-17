use serde_json::Value;
use std::collections::HashSet;

use crate::{
    ClaudeRequest, KiroAssistantMessage, KiroConversationState, KiroCurrentMessage,
    KiroHistoryMessage, KiroPayload, KiroToolUse, KiroUserInputMessage,
};

use super::common::{
    content_text, context, enhance_system, extract_images, extract_tool_results, inference,
    kiro_tool, system_text, tool_name,
};
use super::tool_search::{is_tool_search_tool, tool_search_kiro_tool};
use super::TranslationOptions;

pub fn claude_to_kiro(request: &ClaudeRequest, options: &TranslationOptions) -> KiroPayload {
    let selected_tools = claude_loaded_tools(request);
    let mut documentation = Vec::new();
    let tools = selected_tools
        .iter()
        .map(|tool| {
                let (name, description, schema) = match tool.r#type.as_deref() {
                    Some(kind) if kind.starts_with("web_search") => (
                        "web_search",
                        "Search the web for real-time information. Returns relevant search results with titles, URLs, and snippets.".to_string(),
                        serde_json::json!({
                            "type":"object",
                            "properties":{"query":{"type":"string","description":"The search query"}},
                            "required":["query"]
                        }),
                    ),
                    Some(kind) if kind.starts_with("web_fetch") => (
                        "web_fetch",
                        "Fetch and read content from a specific URL. Returns the page content in readable text format.".to_string(),
                        serde_json::json!({
                            "type":"object",
                            "properties":{"url":{"type":"string","description":"The URL to fetch content from"}},
                            "required":["url"]
                        }),
                    ),
                    _ if is_tool_search_tool(tool) => {
                        return tool_search_kiro_tool(tool)
                            .expect("validated Tool Search definition")
                    }
                    _ => (
                        tool.name.as_str(),
                        tool_description(tool),
                        tool.input_schema.clone(),
                    ),
                };
                let (tool, docs) = kiro_tool(name, &description, &schema);
                documentation.extend(docs);
                tool
        })
        .collect::<Vec<_>>();
    let mut system = system_text(request.system.as_ref());
    if options.enhance_system_prompt {
        let has_write = selected_tools.iter().any(|tool| is_write_tool(&tool.name));
        system = enhance_system(system, has_write);
    }
    if !documentation.is_empty() {
        system = join_nonempty(&system, &documentation.join("\n\n"));
    }
    if request.tools.iter().any(|tool| tool.defer_loading)
        && selected_tools.iter().any(|tool| is_tool_search_tool(tool))
    {
        system = join_nonempty(
            &system,
            "Deferred tools are available through the tool search tool. When the required tool is not loaded, call tool search by itself; use the optional limit when more than the default five matches are needed. The proxy will load the matching tool definitions and continue the same assistant turn.",
        );
    }
    system = join_nonempty(&system, &tool_choice_directive(request, &tools));

    let mut history = Vec::new();
    let mut pending_system = String::new();
    let mut current = KiroUserInputMessage {
        content: "Continue".into(),
        model_id: options.model_id.clone(),
        origin: options.origin.clone(),
        images: Vec::new(),
        user_input_message_context: None,
    };

    for (index, message) in request.messages.iter().enumerate() {
        let last = index + 1 == request.messages.len();
        match message.role.as_str() {
            "system" => {
                pending_system = join_nonempty(&pending_system, &content_text(&message.content))
            }
            "user" => {
                let mut text = content_text(&message.content);
                text = join_nonempty(&pending_system, &text);
                pending_system.clear();
                let images = extract_images(&message.content);
                let results = extract_tool_results(&message.content);
                if last {
                    current.content = nonempty(text, !results.is_empty());
                    current.images = images;
                    current.user_input_message_context = context(tools.clone(), results);
                } else {
                    push_user(&mut history, user_message(text, images, results, options));
                }
            }
            "assistant" => {
                let (text, uses) = assistant_parts(&message.content);
                push_assistant(
                    &mut history,
                    KiroAssistantMessage {
                        content: if text.trim().is_empty() {
                            "Using tools.".into()
                        } else {
                            text
                        },
                        tool_uses: uses,
                    },
                );
            }
            _ => {}
        }
    }

    if !pending_system.is_empty() {
        current.content = join_nonempty(&pending_system, "Continue");
    }
    if current.user_input_message_context.is_none() {
        current.user_input_message_context = context(tools.clone(), Vec::new());
    }
    inject_system(&mut history, &mut current, &system, options);
    sanitize_history(&mut history, options);

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
            Some(request.max_tokens),
            !tools.is_empty(),
            request.temperature,
            request.top_p,
        )),
    }
}

fn tool_description(tool: &crate::ClaudeTool) -> String {
    let Some(examples) = tool
        .input_examples
        .as_ref()
        .filter(|examples| !examples.is_empty())
    else {
        return tool.description.clone();
    };
    let examples = serde_json::to_string_pretty(examples).unwrap_or_else(|_| "[]".into());
    join_nonempty(&tool.description, &format!("Input examples:\n{examples}"))
}

pub fn claude_loaded_tools(request: &ClaudeRequest) -> Vec<&crate::ClaudeTool> {
    match request
        .tool_choice
        .as_ref()
        .map(|choice| choice.r#type.as_str())
    {
        Some("none") => Vec::new(),
        Some("tool") => {
            let selected = request
                .tool_choice
                .as_ref()
                .and_then(|choice| choice.name.as_deref());
            request
                .tools
                .iter()
                .filter(|tool| Some(tool.name.as_str()) == selected)
                .collect()
        }
        _ => {
            let discovered = discovered_tool_names(request);
            request
                .tools
                .iter()
                .filter(|tool| {
                    !tool.defer_loading
                        || is_tool_search_tool(tool)
                        || discovered.contains(tool.name.as_str())
                })
                .collect()
        }
    }
}

fn discovered_tool_names(request: &ClaudeRequest) -> HashSet<&str> {
    let mut discovered = HashSet::new();
    for message in &request.messages {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    if let Some(name) = block.get("name").and_then(Value::as_str) {
                        discovered.insert(name);
                    }
                }
                Some("tool_search_tool_result") => {
                    if let Some(references) = block
                        .pointer("/content/tool_references")
                        .and_then(Value::as_array)
                    {
                        for reference in references {
                            if let Some(name) = reference.get("tool_name").and_then(Value::as_str) {
                                discovered.insert(name);
                            }
                        }
                    }
                }
                Some("tool_result") => {
                    if let Some(references) = block.get("content").and_then(Value::as_array) {
                        for reference in references {
                            if reference.get("type").and_then(Value::as_str)
                                == Some("tool_reference")
                            {
                                if let Some(name) =
                                    reference.get("tool_name").and_then(Value::as_str)
                                {
                                    discovered.insert(name);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    discovered
}

fn assistant_parts(content: &Value) -> (String, Vec<KiroToolUse>) {
    let text = content_text(content);
    let uses = content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| {
            Some(KiroToolUse {
                tool_use_id: block.get("id")?.as_str()?.into(),
                name: normalize_history_tool_name(block.get("name")?.as_str()?),
                input: block
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            })
        })
        .collect();
    (text, uses)
}

fn normalize_history_tool_name(name: &str) -> String {
    if name.starts_with("web_search") {
        "web_search".into()
    } else if name.starts_with("web_fetch") {
        "web_fetch".into()
    } else {
        tool_name(name)
    }
}

fn user_message(
    text: String,
    images: Vec<crate::KiroImage>,
    results: Vec<crate::KiroToolResult>,
    options: &TranslationOptions,
) -> KiroUserInputMessage {
    KiroUserInputMessage {
        content: nonempty(text, !results.is_empty()),
        model_id: options.model_id.clone(),
        origin: options.origin.clone(),
        images,
        user_input_message_context: context(Vec::new(), results),
    }
}

fn push_user(history: &mut Vec<KiroHistoryMessage>, message: KiroUserInputMessage) {
    if history
        .last()
        .is_some_and(|item| item.user_input_message.is_some())
    {
        push_assistant(
            history,
            KiroAssistantMessage {
                content: "Continue.".into(),
                tool_uses: Vec::new(),
            },
        );
    }
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

fn sanitize_history(history: &mut Vec<KiroHistoryMessage>, options: &TranslationOptions) {
    if history
        .first()
        .is_some_and(|item| item.assistant_response_message.is_some())
    {
        history.insert(
            0,
            KiroHistoryMessage {
                user_input_message: Some(user_message(
                    "Begin conversation".into(),
                    vec![],
                    vec![],
                    options,
                )),
                assistant_response_message: None,
            },
        );
    }
}

fn inject_system(
    history: &mut [KiroHistoryMessage],
    current: &mut KiroUserInputMessage,
    system: &str,
    options: &TranslationOptions,
) {
    if system.trim().is_empty() {
        return;
    }
    if options.compact_mode {
        // A compactable request may drop old history after translation. Keep
        // the system prompt on the current turn so compaction can never remove
        // the caller's governing instructions.
        current.content = join_nonempty(system, &current.content);
    } else if let Some(first) = history
        .iter_mut()
        .find_map(|item| item.user_input_message.as_mut())
    {
        first.content = join_nonempty(system, &first.content);
    } else {
        current.content = join_nonempty(system, &current.content);
    }
}

fn tool_choice_directive(request: &ClaudeRequest, tools: &[crate::KiroTool]) -> String {
    let Some(choice) = &request.tool_choice else {
        return String::new();
    };
    let mut directive = match choice.r#type.as_str() {
        "any" => "You must call at least one of the provided tools.".into(),
        "tool" => choice
            .name
            .as_ref()
            .map(|name| format!("You must call the tool \"{}\".", tool_name(name)))
            .unwrap_or_default(),
        _ => String::new(),
    };
    if choice.disable_parallel_tool_use && !tools.is_empty() {
        let constraint = if matches!(choice.r#type.as_str(), "any" | "tool") {
            "Make exactly one tool call."
        } else {
            "Make at most one tool call."
        };
        directive = join_nonempty(&directive, constraint);
    }
    directive
}

fn nonempty(text: String, has_results: bool) -> String {
    if text.trim().is_empty() {
        if has_results {
            "Tool results provided."
        } else {
            "Continue"
        }
        .into()
    } else {
        text
    }
}

fn join_nonempty(left: &str, right: &str) -> String {
    match (left.trim().is_empty(), right.trim().is_empty()) {
        (true, _) => right.trim().into(),
        (_, true) => left.trim().into(),
        _ => format!("{}\n\n{}", left.trim(), right.trim()),
    }
}

fn is_write_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "write" | "edit" | "multiedit" | "notebookedit"
    )
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
    fn versioned_server_web_tools_use_kiro_canonical_contracts() {
        let request: ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4",
            "max_tokens":256,
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"web_search_20250305","input":{"query":"rust"}}]},
                {"role":"user","content":"continue"}
            ],
            "tools":[
                {"type":"web_search_20250305","name":"web_search","description":"","input_schema":{}},
                {"type":"web_fetch_20250910","name":"web_fetch","description":"","input_schema":{}}
            ]
        }))
        .expect("request");
        let mut options = TranslationOptions::new("dynamic-sonnet", "AI_EDITOR");
        options.profile_arn = Some("arn:aws:codewhisperer:us-east-1:1:profile/p".into());
        let payload = claude_to_kiro(&request, &options);
        assert_eq!(payload.profile_arn, options.profile_arn);
        let context = payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .expect("context");
        assert_eq!(context.tools[0].tool_specification.name, "web_search");
        assert_eq!(
            context.tools[0].tool_specification.input_schema.json["required"],
            serde_json::json!(["query"])
        );
        assert_eq!(context.tools[1].tool_specification.name, "web_fetch");
        let history_tool = &payload.conversation_state.history[1]
            .assistant_response_message
            .as_ref()
            .expect("assistant")
            .tool_uses[0];
        assert_eq!(history_tool.name, "web_search");
    }

    #[test]
    fn tool_result_images_are_forwarded_with_a_textual_pairing_marker() {
        let request: ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4",
            "max_tokens":256,
            "messages":[
                {"role":"assistant","content":[{
                    "type":"tool_use","id":"tool_1","name":"screenshot","input":{}
                }]},
                {"role":"user","content":[{
                    "type":"tool_result","tool_use_id":"tool_1","content":[{
                        "type":"image","source":{
                            "type":"base64","media_type":"image/png","data":"aGVsbG8="
                        }
                    }]
                }]}
            ]
        }))
        .expect("request");
        let payload = claude_to_kiro(
            &request,
            &TranslationOptions::new("dynamic-sonnet", "AI_EDITOR"),
        );
        let current = payload
            .conversation_state
            .current_message
            .user_input_message;
        assert_eq!(current.images.len(), 1);
        let result = &current
            .user_input_message_context
            .expect("context")
            .tool_results[0];
        assert_eq!(result.content[0].text, "(image result attached)");
    }

    #[test]
    fn deferred_tools_stay_out_of_kiro_until_referenced() {
        let mut request: ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4-5",
            "max_tokens":256,
            "messages":[{"role":"user","content":"inspect the issue"}],
            "tools":[
                {"type":"tool_search_tool_regex_20251119","name":"tool_search_tool_regex"},
                {"name":"Read","description":"Read a file","input_schema":{"type":"object"}},
                {"name":"mcp__github__list_issues","description":"List issues","input_schema":{"type":"object"},"defer_loading":true}
            ]
        }))
        .expect("request");
        let options = TranslationOptions::new("dynamic-sonnet", "AI_EDITOR");
        let payload = claude_to_kiro(&request, &options);
        let tools = &payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .expect("context")
            .tools;
        assert_eq!(tools.len(), 2);
        assert!(tools
            .iter()
            .any(|tool| { tool.tool_specification.name == "tool_search_tool_regex" }));
        assert!(!tools
            .iter()
            .any(|tool| { tool.tool_specification.name == "mcp__github__list_issues" }));

        request.messages.insert(
            0,
            crate::ClaudeMessage {
                role: "assistant".into(),
                content: serde_json::json!([{
                    "type":"tool_search_tool_result",
                    "tool_use_id":"srvtoolu_1",
                    "content":{
                        "type":"tool_search_tool_search_result",
                        "tool_references":[{
                            "type":"tool_reference","tool_name":"mcp__github__list_issues"
                        }]
                    }
                }]),
            },
        );
        let payload = claude_to_kiro(&request, &options);
        let tools = &payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .expect("context")
            .tools;
        assert_eq!(tools.len(), 3);
        assert!(tools
            .iter()
            .any(|tool| { tool.tool_specification.name == "mcp__github__list_issues" }));
    }
}

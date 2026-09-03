//! Stateless OpenAI Responses input, normalized into the shared OpenAI/Kiro path.
//!
//! Protocol reference: https://developers.openai.com/api/reference/resources/responses
//! Storage, opaque replay and hosted tools are rejected before upstream execution.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    validate_openai, OpenAiMessage, OpenAiRequest, OpenAiTool, ThinkingConfig, ValidationError,
};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Value,
    pub instructions: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub stream: bool,
    pub stream_options: Option<Value>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    #[serde(default)]
    pub tools: Vec<Value>,
    pub tool_choice: Option<Value>,
    pub parallel_tool_calls: Option<bool>,
    pub reasoning: Option<ResponsesReasoning>,
    pub text: Option<ResponsesText>,
    pub metadata: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub include: Vec<String>,
    pub store: Option<bool>,
    pub background: Option<bool>,
    pub previous_response_id: Option<String>,
    pub conversation: Option<Value>,
    pub truncation: Option<String>,
    pub service_tier: Option<String>,
    pub max_tool_calls: Option<u32>,
    pub context_management: Option<Value>,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<String>,
    pub safety_identifier: Option<String>,
    pub user: Option<String>,
    /// Codex telemetry; never used for authorization or added to model input.
    pub client_metadata: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesReasoning {
    pub effort: Option<String>,
    pub summary: Option<String>,
    pub context: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesText {
    pub format: Option<Value>,
    pub verbosity: Option<String>,
}

// The Responses schema permits explicit null for these optional controls.
// Normalize it at this boundary without relaxing Chat Completions validation.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Debug, Clone)]
pub struct ResponsesToolName {
    pub name: String,
    pub namespace: Option<String>,
}

pub struct ResponsesTranslation {
    pub request: OpenAiRequest,
    /// Flattened OpenAI name -> original Responses namespace and leaf name.
    pub tool_names: HashMap<String, ResponsesToolName>,
}

pub fn responses_to_openai(
    request: &ResponsesRequest,
) -> Result<ResponsesTranslation, ValidationError> {
    validate_controls(request)?;
    let mut messages = Vec::new();
    if let Some(instructions) = &request.instructions {
        messages.push(message("system", Some(json!(instructions))));
    }
    if let Some(verbosity) = request
        .text
        .as_ref()
        .and_then(|text| text.verbosity.as_deref())
    {
        let instruction = match verbosity {
            "low" => "Keep the final response concise.",
            "medium" => "Use a moderate level of detail in the final response.",
            "high" => "Provide a detailed final response.",
            _ => return invalid("text.verbosity", "expected low, medium, or high"),
        };
        messages.push(message("system", Some(json!(instruction))));
    }
    match &request.input {
        Value::String(text) => messages.push(message("user", Some(json!(text)))),
        Value::Array(items) if !items.is_empty() => {
            let mut calls = HashSet::new();
            let mut pending = HashMap::new();
            for (index, item) in items.iter().enumerate() {
                let field = format!("input.{index}");
                let kind = match item.get("type") {
                    None => "message",
                    Some(value) => value
                        .as_str()
                        .ok_or_else(|| error(format!("{field}.type"), "expected a string"))?,
                };
                match kind {
                    "message" => {
                        let role = required_string(item, "role", &field)?;
                        if !matches!(role, "system" | "developer" | "user" | "assistant") {
                            return invalid(format!("{field}.role"), "expected system, developer, user, or assistant");
                        }
                        let content = content(item.get("content"), &format!("{field}.content"), role == "user")?;
                        messages.push(message(role, Some(content)));
                    }
                    "function_call" | "custom_tool_call" => {
                        let id = required_string(item, "call_id", &field)?;
                        if !calls.insert(id.to_owned()) {
                            return invalid(format!("{field}.call_id"), "duplicate tool call id");
                        }
                        pending.insert(id.to_owned(), kind);
                        let name = qualified_name(item, &field)?;
                        let (tool_type, key) = if kind == "function_call" { ("function", "arguments") } else { ("custom", "input") };
                        let input = string(item, key, &field)?;
                        let mut assistant = message("assistant", None);
                        assistant.tool_calls.push(json!({
                            "id":id,"type":tool_type,tool_type:{"name":name,key:input}
                        }));
                        messages.push(assistant);
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let id = required_string(item, "call_id", &field)?;
                        let expected = if kind == "function_call_output" { "function_call" } else { "custom_tool_call" };
                        if pending.remove(id) != Some(expected) {
                            return invalid(format!("{field}.call_id"), "tool output must match an earlier, unanswered call in input; send the complete history");
                        }
                        let output = content(item.get("output"), &format!("{field}.output"), true)?;
                        let mut result = message("tool", Some(output));
                        result.tool_call_id = Some(id.to_owned());
                        messages.push(result);
                    }
                    "reasoning" => {
                        if item.get("encrypted_content").is_some_and(|v| !v.is_null() && v.as_str() != Some("")) {
                            return invalid(format!("{field}.encrypted_content"), "opaque OpenAI reasoning cannot be replayed by Kiro; send plaintext history");
                        }
                        let mut reasoning = Vec::new();
                        for key in ["summary", "content"] {
                            if let Some(parts) = item.get(key).filter(|v| !v.is_null()) {
                                let Some(parts) = parts.as_array() else {
                                    return invalid(format!("{field}.{key}"), "expected an array");
                                };
                                for (part_index, part) in parts.iter().enumerate() {
                                    let part_field = format!("{field}.{key}.{part_index}");
                                    if !matches!(part.get("type").and_then(Value::as_str), Some("summary_text" | "reasoning_text" | "text")) {
                                        return invalid(part_field, "expected a plaintext reasoning part");
                                    }
                                    reasoning.push(string(part, "text", &part_field)?.to_owned());
                                }
                            }
                        }
                        if !reasoning.is_empty() {
                            // Kiro has no Responses reasoning-history item.
                            // Preserve the supplied plaintext summary explicitly
                            // without changing Chat Completions history policy.
                            let assistant = message("assistant", Some(json!(format!("Reasoning summary:\n{}", reasoning.join("\n")))));
                            messages.push(assistant);
                        }
                    }
                    _ => return invalid(format!("{field}.type"), "unsupported Responses input item; send messages and function/custom tool calls with their outputs"),
                }
            }
            if !pending.is_empty() {
                return invalid(
                    "input",
                    "every tool call in input must have a matching tool output",
                );
            }
        }
        _ => {
            return invalid(
                "input",
                "expected a string or a non-empty array of input items",
            )
        }
    }

    // Responses emits reasoning, messages and each parallel tool call as
    // separate items in one assistant turn. Kiro requires one assistant
    // message followed by the complete batch of matching tool results.
    let messages = merge_assistant_items(messages);
    let mut tools = Vec::new();
    let mut tool_names = HashMap::new();
    for (index, tool) in request.tools.iter().enumerate() {
        let field = format!("tools.{index}");
        if tool.get("type").and_then(Value::as_str) == Some("namespace") {
            let namespace = required_string(tool, "name", &field)?;
            let description = tool
                .get("description")
                .filter(|value| !value.is_null())
                .map(|_| string(tool, "description", &field))
                .transpose()?;
            let Some(children) = tool.get("tools").and_then(Value::as_array) else {
                return invalid(
                    format!("{field}.tools"),
                    "expected an array of function or custom tools",
                );
            };
            for (child_index, child) in children.iter().enumerate() {
                add_tool(
                    child,
                    Some(namespace),
                    description,
                    &format!("{field}.tools.{child_index}"),
                    &mut tools,
                    &mut tool_names,
                )?;
            }
        } else {
            add_tool(tool, None, None, &field, &mut tools, &mut tool_names)?;
        }
    }
    let tool_choice = request
        .tool_choice
        .as_ref()
        .filter(|v| !v.is_null())
        .map(|choice| {
            if choice.is_string() {
                return Ok(choice.clone());
            }
            let kind = required_string(choice, "type", "tool_choice")?;
            if !matches!(kind, "function" | "custom") {
                return invalid("tool_choice.type", "expected function or custom");
            }
            let name = qualified_name(choice, "tool_choice")?;
            Ok(json!({"type":kind,kind:{"name":name}}))
        })
        .transpose()?;
    let effort = request.reasoning.as_ref().and_then(|r| r.effort.as_deref());
    let thinking = (effort == Some("none")).then(|| ThinkingConfig {
        r#type: "disabled".into(),
        budget_tokens: None,
        display: None,
    });
    let normalized = OpenAiRequest {
        model: request.model.clone(),
        messages,
        temperature: request.temperature,
        top_p: request.top_p,
        max_tokens: request.max_output_tokens,
        max_completion_tokens: None,
        stream: request.stream,
        stream_options: request.stream.then(|| json!({"include_usage":true})),
        tools,
        tool_choice,
        parallel_tool_calls: request.parallel_tool_calls.unwrap_or(true),
        thinking,
        reasoning_effort: effort.filter(|effort| *effort != "none").map(str::to_owned),
        conversation_id: None,
        metadata: request.metadata.clone(),
        response_format: request.text.as_ref().and_then(|text| text.format.clone()),
    };
    validate_openai(&normalized)?;
    Ok(ResponsesTranslation {
        request: normalized,
        tool_names,
    })
}

fn validate_controls(request: &ResponsesRequest) -> Result<(), ValidationError> {
    for (field, unsupported) in [
        ("store", request.store == Some(true)),
        ("background", request.background == Some(true)),
        (
            "previous_response_id",
            request.previous_response_id.is_some(),
        ),
        ("conversation", request.conversation.is_some()),
        ("max_tool_calls", request.max_tool_calls.is_some()),
        ("context_management", request.context_management.is_some()),
    ] {
        if unsupported {
            return invalid(field, "not supported by this stateless endpoint; use store=false and send the complete history in input");
        }
    }
    if request
        .truncation
        .as_deref()
        .is_some_and(|v| v != "disabled")
    {
        return invalid(
            "truncation",
            "only disabled is supported; compact the history on the client",
        );
    }
    if request.service_tier.as_deref().is_some_and(|v| v != "auto") {
        return invalid(
            "service_tier",
            "only auto is supported by the Kiro upstream",
        );
    }
    if request
        .prompt_cache_retention
        .as_deref()
        .is_some_and(|v| !matches!(v, "in_memory" | "in-memory"))
    {
        return invalid("prompt_cache_retention", "only in_memory is supported");
    }
    if let Some(reasoning) = &request.reasoning {
        if reasoning
            .summary
            .as_deref()
            .is_some_and(|v| !matches!(v, "auto" | "concise" | "detailed"))
        {
            return invalid("reasoning.summary", "expected auto, concise, or detailed");
        }
        if reasoning.context.as_deref().is_some_and(|v| v != "auto") {
            return invalid("reasoning.context", "only auto is supported");
        }
    }
    for include in &request.include {
        if include != "reasoning.encrypted_content" {
            return invalid("include", "only reasoning.encrypted_content is accepted; Kiro returns plaintext reasoning without an encrypted replay token");
        }
    }
    if let Some(options) = &request.stream_options {
        if !request.stream {
            return invalid("stream_options", "is only valid when stream is true");
        }
        let Some(options) = options.as_object() else {
            return invalid("stream_options", "expected an object");
        };
        for (key, value) in options {
            match key.as_str() {
                "include_obfuscation" if value == false => {}
                // Codex asks to finish each reasoning summary before the answer.
                "reasoning_summary_delivery" if value == "sequential_cutoff" => {}
                _ => return invalid(format!("stream_options.{key}"), "unsupported stream option"),
            }
        }
    }
    Ok(())
}

fn add_tool(
    tool: &Value,
    namespace: Option<&str>,
    namespace_description: Option<&str>,
    field: &str,
    tools: &mut Vec<OpenAiTool>,
    names: &mut HashMap<String, ResponsesToolName>,
) -> Result<(), ValidationError> {
    let kind = required_string(tool, "type", field)?;
    if !matches!(kind, "function" | "custom") {
        return invalid(
            format!("{field}.type"),
            "hosted tools are not supported; use function or custom tools executed by the client",
        );
    }
    let name = required_string(tool, "name", field)?;
    let qualified = namespace.map_or_else(
        || name.to_owned(),
        |namespace| format!("{namespace}.{name}"),
    );
    if names
        .insert(
            qualified.clone(),
            ResponsesToolName {
                name: name.to_owned(),
                namespace: namespace.map(str::to_owned),
            },
        )
        .is_some()
    {
        return invalid(format!("{field}.name"), "duplicate or ambiguous tool name");
    }
    let mut definition = tool.as_object().cloned().unwrap_or_default();
    definition.remove("type");
    // These fields are nullable in the Responses function schema. An absent
    // parameters schema uses the existing no-argument tool default.
    for key in ["description", "parameters", "strict"] {
        if definition.get(key).is_some_and(Value::is_null) {
            definition.remove(key);
        }
    }
    if let Some(deferred) = definition.remove("defer_loading") {
        if deferred != false {
            return invalid(
                format!("{field}.defer_loading"),
                "deferred Responses tools are not supported",
            );
        }
    }
    definition.insert("name".into(), json!(qualified));
    if let Some(scope) = namespace_description.filter(|text| !text.is_empty()) {
        let description = tool
            .get("description")
            .filter(|value| !value.is_null())
            .map(|_| string(tool, "description", field))
            .transpose()?
            .unwrap_or_default();
        definition.insert(
            "description".into(),
            json!(format!(
                "Namespace {}: {scope}\n\n{description}",
                namespace.unwrap_or_default()
            )),
        );
    }
    if kind == "custom" {
        if let Some(format) = definition.get_mut("format") {
            if format.get("type").and_then(Value::as_str) == Some("grammar") {
                *format = json!({"type":"grammar","grammar":{
                    "syntax":format.get("syntax"),"definition":format.get("definition")
                }});
            }
        }
    }
    tools.push(OpenAiTool {
        r#type: kind.into(),
        body: json!({kind:definition}),
    });
    Ok(())
}

fn qualified_name(item: &Value, field: &str) -> Result<String, ValidationError> {
    let name = required_string(item, "name", field)?;
    match item.get("namespace").filter(|v| !v.is_null()) {
        Some(value) => {
            let namespace = value
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    error(format!("{field}.namespace"), "expected a non-empty string")
                })?;
            Ok(format!("{namespace}.{name}"))
        }
        None => Ok(name.to_owned()),
    }
}

fn content(value: Option<&Value>, field: &str, images: bool) -> Result<Value, ValidationError> {
    match value {
        Some(Value::String(text)) => Ok(json!(text)),
        Some(Value::Array(parts)) => parts
            .iter()
            .enumerate()
            .map(|(index, part)| {
                let field = format!("{field}.{index}");
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text") => {
                        Ok(json!({"type":"text","text":string(part, "text", &field)?}))
                    }
                    Some("input_image") if images => {
                        let url = required_string(part, "image_url", &field)?;
                        if part.get("file_id").is_some_and(|v| !v.is_null()) {
                            return invalid(
                                format!("{field}.file_id"),
                                "use an image URL or base64 data URL",
                            );
                        }
                        let mut image = json!({"url":url});
                        if let Some(detail) = part.get("detail").filter(|v| !v.is_null()) {
                            if !matches!(
                                detail.as_str(),
                                Some("auto" | "low" | "high" | "original")
                            ) {
                                return invalid(format!("{field}.detail"), "invalid image detail");
                            }
                            image["detail"] = detail.clone();
                        }
                        Ok(json!({"type":"image_url","image_url":image}))
                    }
                    _ => invalid(
                        format!("{field}.type"),
                        "unsupported content part; use text or input_image with an image URL",
                    ),
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => invalid(field, "expected a string or content array"),
    }
}

fn message(role: &str, content: Option<Value>) -> OpenAiMessage {
    OpenAiMessage {
        role: role.into(),
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        reasoning_content: None,
        name: None,
        cache_control: None,
    }
}

fn merge_assistant_items(messages: Vec<OpenAiMessage>) -> Vec<OpenAiMessage> {
    let mut merged: Vec<OpenAiMessage> = Vec::with_capacity(messages.len());
    for mut message in messages {
        let previous = merged
            .last_mut()
            .filter(|previous| previous.role == "assistant" && message.role == "assistant");
        if let Some(previous) = previous {
            let had_content = previous.content.is_some() || message.content.is_some();
            // Reuse the accumulated vector: copying all earlier parts for
            // every assistant item makes fragmented histories quadratic.
            let mut parts = match previous.content.take() {
                Some(Value::Array(parts)) => parts,
                Some(Value::String(text)) if !text.is_empty() => {
                    vec![json!({"type":"text","text":text})]
                }
                _ => Vec::new(),
            };
            if let Some(content) = message.content.take() {
                match content {
                    Value::String(text) if !text.is_empty() => {
                        parts.push(json!({"type":"text","text":text}))
                    }
                    Value::Array(items) => parts.extend(items),
                    _ => {}
                }
            }
            previous.content = if parts.is_empty() {
                had_content.then(|| json!(""))
            } else {
                Some(Value::Array(parts))
            };
            previous.tool_calls.append(&mut message.tool_calls);
        } else {
            merged.push(message);
        }
    }
    merged
}

fn string<'a>(value: &'a Value, key: &str, field: &str) -> Result<&'a str, ValidationError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| error(format!("{field}.{key}"), "expected a string"))
}

fn required_string<'a>(
    value: &'a Value,
    key: &str,
    field: &str,
) -> Result<&'a str, ValidationError> {
    let value = string(value, key, field)?;
    if value.trim().is_empty() {
        return invalid(format!("{field}.{key}"), "must not be empty");
    }
    Ok(value)
}

fn error(field: impl Into<String>, message: impl Into<String>) -> ValidationError {
    ValidationError::InvalidField {
        field: field.into(),
        message: message.into(),
    }
}

fn invalid<T>(field: impl Into<String>, message: impl Into<String>) -> Result<T, ValidationError> {
    Err(error(field, message))
}

#[cfg(test)]
mod tests;

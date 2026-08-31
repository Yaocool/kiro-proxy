//! Request validation and bounded tool-schema traversal.

use base64::Engine as _;
use serde_json::Value;
use std::collections::HashSet;
use thiserror::Error;

use crate::{is_tool_search_type, matches_type_family, ClaudeRequest, OpenAiRequest};

pub const MAX_SCHEMA_DEPTH: usize = 64;
pub const MAX_SCHEMA_NODES: usize = 50_000;
pub const MAX_TOOL_DOC_CHARS: usize = 512_000;
pub const MAX_TOOLS: usize = kproxy_core::config::MAX_LOADED_TOOLS;
pub const MAX_DEFERRED_TOOLS: usize = 10_000;
pub const MAX_LOADED_TOOL_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DEFERRED_TOOL_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TOOL_BYTES: usize = 256 * 1024;
pub const MAX_TOOL_NAME_CHARS: usize = 128;
pub const MAX_STOP_SEQUENCES: usize = 64;
pub const MAX_STOP_SEQUENCE_BYTES: usize = 4 * 1024;
pub const MAX_STOP_SEQUENCE_TOTAL_BYTES: usize = 64 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("model is required")]
    MissingModel,
    #[error("messages must not be empty")]
    MissingMessages,
    #[error("max_tokens must be greater than zero")]
    InvalidMaxTokens,
    #[error("too many tools: maximum is {MAX_TOOLS}")]
    TooManyTools,
    #[error("too many deferred tools: maximum is {MAX_DEFERRED_TOOLS}")]
    TooManyDeferredTools,
    #[error("loaded tool definitions exceed the {MAX_LOADED_TOOL_BYTES} byte limit; enable Anthropic Tool Search and defer MCP tools")]
    LoadedToolDefinitionsTooLarge,
    #[error("deferred tool catalog exceeds the {MAX_DEFERRED_TOOL_BYTES} byte limit")]
    DeferredToolDefinitionsTooLarge,
    #[error("a single tool definition exceeds the {MAX_TOOL_BYTES} byte limit")]
    ToolDefinitionTooLarge,
    #[error("tool name exceeds {MAX_TOOL_NAME_CHARS} characters")]
    ToolNameTooLong,
    #[error("tool documentation exceeds {MAX_TOOL_DOC_CHARS} characters")]
    ToolDocumentationTooLarge,
    #[error("tool schema exceeds maximum depth {MAX_SCHEMA_DEPTH}")]
    SchemaTooDeep,
    #[error("tool schema exceeds maximum node count {MAX_SCHEMA_NODES}")]
    SchemaTooLarge,
    #[error("message role is not supported: {0}")]
    InvalidRole(String),
    #[error("invalid value for {field}: {message}")]
    InvalidField { field: String, message: String },
}

pub fn validate_claude(request: &ClaudeRequest) -> Result<(), ValidationError> {
    common(&request.model, request.messages.is_empty())?;
    if request.max_tokens == 0 {
        return Err(ValidationError::InvalidMaxTokens);
    }
    if request.stop_sequences.iter().any(String::is_empty) {
        return invalid("stop_sequences", "sequences must not be empty");
    }
    if request.stop_sequences.len() > MAX_STOP_SEQUENCES {
        return invalid(
            "stop_sequences",
            format!("must contain at most {MAX_STOP_SEQUENCES} sequences"),
        );
    }
    if request
        .stop_sequences
        .iter()
        .any(|sequence| sequence.len() > MAX_STOP_SEQUENCE_BYTES)
    {
        return invalid(
            "stop_sequences",
            format!("each sequence must be at most {MAX_STOP_SEQUENCE_BYTES} bytes"),
        );
    }
    let stop_sequence_bytes = request
        .stop_sequences
        .iter()
        .map(String::len)
        .fold(0usize, usize::saturating_add);
    if stop_sequence_bytes > MAX_STOP_SEQUENCE_TOTAL_BYTES {
        return invalid(
            "stop_sequences",
            format!("sequences must total at most {MAX_STOP_SEQUENCE_TOTAL_BYTES} bytes"),
        );
    }
    validate_claude_system(request.system.as_ref())?;
    for (index, message) in request.messages.iter().enumerate() {
        if !matches!(message.role.as_str(), "user" | "assistant" | "system") {
            return Err(ValidationError::InvalidRole(message.role.clone()));
        }
        validate_claude_content(
            &message.content,
            &message.role,
            format!("messages.{index}.content"),
        )?;
    }
    validate_server_tool_history(request)?;
    let deferred_count = request
        .tools
        .iter()
        .filter(|tool| tool.defer_loading)
        .count();
    let loaded_count = request.tools.len().saturating_sub(deferred_count);
    if loaded_count > MAX_TOOLS {
        return Err(ValidationError::TooManyTools);
    }
    if deferred_count > MAX_DEFERRED_TOOLS {
        return Err(ValidationError::TooManyDeferredTools);
    }
    if request.tools.iter().any(|tool| {
        serde_json::to_vec(tool).map_or(true, |definition| definition.len() > MAX_TOOL_BYTES)
    }) {
        return Err(ValidationError::ToolDefinitionTooLarge);
    }
    let (loaded_bytes, deferred_bytes) =
        request
            .tools
            .iter()
            .fold((2usize, 2usize), |(loaded, deferred), tool| {
                let bytes = serde_json::to_vec(tool).map_or(usize::MAX, |value| value.len());
                if tool.defer_loading {
                    (loaded, deferred.saturating_add(bytes).saturating_add(1))
                } else {
                    (loaded.saturating_add(bytes).saturating_add(1), deferred)
                }
            });
    if loaded_bytes > MAX_LOADED_TOOL_BYTES {
        return Err(ValidationError::LoadedToolDefinitionsTooLarge);
    }
    if deferred_bytes > MAX_DEFERRED_TOOL_BYTES {
        return Err(ValidationError::DeferredToolDefinitionsTooLarge);
    }
    if deferred_count > 0 && loaded_count == 0 {
        return invalid("tools", "at least one tool must have defer_loading=false");
    }
    let docs = request
        .tools
        .iter()
        .filter(|tool| !tool.defer_loading)
        .map(|tool| {
            tool.description.chars().count()
                + tool
                    .input_examples
                    .as_ref()
                    .and_then(|examples| serde_json::to_string(examples).ok())
                    .map_or(0, |examples| examples.chars().count())
        })
        .sum::<usize>();
    if docs > MAX_TOOL_DOC_CHARS {
        return Err(ValidationError::ToolDocumentationTooLarge);
    }
    let mut seen_tool_names = HashSet::new();
    for (index, tool) in request.tools.iter().enumerate() {
        let kind = tool.r#type.as_deref();
        if kind.is_some_and(|kind| matches_type_family(kind, "web_fetch")) {
            return invalid(
                format!("tools.{index}.type"),
                "Claude Web Fetch is not implemented by this proxy; rejecting it avoids exposing a client tool with incompatible server-tool semantics",
            );
        }
        if kind == Some("mcp_toolset") {
            return invalid(
                format!("tools.{index}.type"),
                "mcp_toolset is not supported; expand it into individual custom tools or use Tool Search with defer_loading",
            );
        }
        if tool.name.trim().is_empty() {
            return invalid(format!("tools.{index}.name"), "must not be empty");
        }
        if !seen_tool_names.insert(tool.name.as_str()) {
            return invalid(format!("tools.{index}.name"), "tool names must be unique");
        }
        let web_tool = kind.is_some_and(|kind| matches_type_family(kind, "web_search"));
        let search_tool = kind.is_some_and(is_tool_search_type);
        if kind.is_some_and(|kind| kind.starts_with("tool_search_tool_")) && !search_tool {
            return invalid(
                format!("tools.{index}.type"),
                format!(
                    "unsupported Claude Tool Search version '{}'",
                    kind.unwrap_or_default()
                ),
            );
        }
        if web_tool && tool.name != "web_search" {
            return invalid(
                format!("tools.{index}.name"),
                "web_search server tools must use name=\"web_search\"",
            );
        }
        if kind.is_some_and(|kind| kind != "custom") && !web_tool && !search_tool {
            return invalid(
                format!("tools.{index}.type"),
                format!(
                    "unsupported Claude server tool '{}'",
                    kind.unwrap_or_default()
                ),
            );
        }
        if !tool.extra.is_empty() {
            let fields = tool.extra.keys().cloned().collect::<Vec<_>>().join(", ");
            return invalid(
                format!("tools.{index}"),
                format!(
                    "unsupported tool protocol field(s): {fields}; unknown tool controls cannot be safely ignored"
                ),
            );
        }
        if !web_tool && !search_tool {
            validate_tool(&tool.name, &tool.input_schema).map_err(|error| {
                if error == ValidationError::ToolNameTooLong {
                    error
                } else {
                    ValidationError::InvalidField {
                        field: format!("tools.{index}.input_schema"),
                        message: error.to_string(),
                    }
                }
            })?;
        }
        if tool.strict == Some(true) {
            return invalid(
                format!("tools.{index}.strict"),
                "strict tool schemas are not supported by the Kiro upstream",
            );
        }
        if (web_tool || search_tool) && tool.eager_input_streaming.is_some() {
            return invalid(
                format!("tools.{index}.eager_input_streaming"),
                "eager_input_streaming is only valid for client-defined tools",
            );
        }
        if tool.eager_input_streaming == Some(true) {
            return invalid(
                format!("tools.{index}.eager_input_streaming"),
                "eager tool input streaming is not supported by the Kiro upstream",
            );
        }
        if let Some(callers) = &tool.allowed_callers {
            if callers.as_slice() != ["direct"] {
                return invalid(
                    format!("tools.{index}.allowed_callers"),
                    "only allowed_callers=[\"direct\"] is supported; code-execution callers cannot be emulated by Kiro",
                );
            }
        }
        if kind.is_some_and(|kind| {
            matches_type_family(kind, "web_search")
                && !matches!(kind, "web_search" | "web_search_20250305")
        }) && tool.allowed_callers.is_none()
        {
            return invalid(
                format!("tools.{index}.allowed_callers"),
                "this web search version defaults to code execution; set allowed_callers=[\"direct\"] for Kiro compatibility",
            );
        }
        if web_tool {
            if tool.input_examples.is_some() {
                return invalid(
                    format!("tools.{index}.input_examples"),
                    "input_examples is not supported on Claude server tools",
                );
            }
            if tool.allowed_domains.is_some() || tool.blocked_domains.is_some() {
                return invalid(
                    format!("tools.{index}"),
                    "web search domain filters are not supported by the Kiro MCP search endpoint",
                );
            }
            if tool.user_location.is_some() {
                return invalid(
                    format!("tools.{index}.user_location"),
                    "web search localization is not supported by the Kiro MCP search endpoint",
                );
            }
            if tool
                .response_inclusion
                .as_deref()
                .is_some_and(|value| !matches!(value, "full" | "excluded"))
            {
                return invalid(
                    format!("tools.{index}.response_inclusion"),
                    "expected \"full\" or \"excluded\"",
                );
            }
            if tool.response_inclusion.as_deref() == Some("excluded") {
                return invalid(
                    format!("tools.{index}.response_inclusion"),
                    "response_inclusion=\"excluded\" cannot be emulated because Kiro needs the search results to synthesize the answer",
                );
            }
            if tool.max_uses == Some(0) {
                return invalid(format!("tools.{index}.max_uses"), "must be at least 1");
            }
        } else if tool.max_uses.is_some()
            || tool.allowed_domains.is_some()
            || tool.blocked_domains.is_some()
            || tool.user_location.is_some()
            || tool.response_inclusion.is_some()
        {
            return invalid(
                format!("tools.{index}"),
                "web search configuration fields are only valid on web_search server tools",
            );
        }
        if tool.defer_loading && tool.cache_control.is_some() {
            return invalid(
                format!("tools.{index}.cache_control"),
                "deferred tools cannot define cache_control",
            );
        }
        if search_tool && tool.defer_loading {
            return invalid(
                format!("tools.{index}.defer_loading"),
                "the Tool Search tool must be loaded immediately",
            );
        }
        if search_tool && tool.input_examples.is_some() {
            return invalid(
                format!("tools.{index}.input_examples"),
                "input_examples is not supported on Claude server tools",
            );
        }
        if tool
            .input_examples
            .as_ref()
            .is_some_and(|examples| examples.iter().any(|example| !example.is_object()))
        {
            return invalid(
                format!("tools.{index}.input_examples"),
                "every input example must be an object",
            );
        }
    }
    validate_tool_references(request, &seen_tool_names)?;
    if let Some(choice) = &request.tool_choice {
        if !matches!(choice.r#type.as_str(), "auto" | "any" | "tool" | "none") {
            return invalid("tool_choice.type", "expected auto, any, tool, or none");
        }
        if choice.r#type == "tool" {
            let Some(name) = choice
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
            else {
                return invalid("tool_choice.name", "a tool name is required");
            };
            let found = request.tools.iter().any(|tool| {
                tool.name == name
                    || tool.r#type.as_deref() == Some(name)
                    || (name == "web_search"
                        && tool
                            .r#type
                            .as_deref()
                            .is_some_and(|kind| matches_type_family(kind, "web_search")))
                    || (name == "web_fetch"
                        && tool
                            .r#type
                            .as_deref()
                            .is_some_and(|kind| matches_type_family(kind, "web_fetch")))
            });
            if !found {
                return invalid("tool_choice.name", format!("tool '{name}' was not found"));
            }
        } else if choice.name.is_some() {
            return invalid(
                "tool_choice.name",
                "name is only valid when tool_choice.type is tool",
            );
        }
        if choice.r#type == "any" && request.tools.is_empty() {
            return invalid("tool_choice.type", "any requires at least one tool");
        }
        if matches!(choice.r#type.as_str(), "any" | "tool")
            && request
                .thinking
                .as_ref()
                .is_some_and(|thinking| thinking.r#type == "enabled")
        {
            return invalid(
                "tool_choice.type",
                "forced tool choice is incompatible with enabled thinking",
            );
        }
    }
    validate_thinking(request.thinking.as_ref())?;
    validate_context_management(request.context_management.as_ref())?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerToolKind {
    ToolSearch,
    WebSearch,
}

fn server_tool_kind(name: &str) -> Option<ServerToolKind> {
    if name == "web_search" {
        Some(ServerToolKind::WebSearch)
    } else if is_tool_search_type(name) {
        Some(ServerToolKind::ToolSearch)
    } else {
        None
    }
}

/// Validate the cross-block protocol that cannot be checked one content block
/// at a time. Server-tool IDs are globally unique in a request, result blocks
/// must match the kind of their pending call, and clients must never answer a
/// `srvtoolu_*` call with an ordinary client `tool_result`.
fn validate_server_tool_history(request: &ClaudeRequest) -> Result<(), ValidationError> {
    let mut all_tool_uses = std::collections::HashMap::<String, String>::new();
    let mut server_tool_uses = std::collections::HashMap::<String, (ServerToolKind, String)>::new();

    for (message_index, message) in request.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            let field = format!("messages.{message_index}.content.{block_index}");
            let kind = block.get("type").and_then(Value::as_str);
            if !matches!(kind, Some("tool_use" | "server_tool_use")) {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            if let Some(previous) = all_tool_uses.insert(id.to_owned(), field.clone()) {
                return invalid(
                    format!("{field}.id"),
                    format!("tool-use id '{id}' is already defined at {previous}"),
                );
            }
            if kind == Some("server_tool_use") {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(server_kind) = server_tool_kind(name) else {
                    return invalid(
                        format!("{field}.name"),
                        format!("unsupported server tool '{name}'"),
                    );
                };
                server_tool_uses.insert(id.to_owned(), (server_kind, field));
            }
        }
    }

    let mut pending = std::collections::HashMap::<String, (ServerToolKind, String)>::new();
    let mut completed = HashSet::<String>::new();
    for (message_index, message) in request.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            let field = format!("messages.{message_index}.content.{block_index}");
            let kind = block.get("type").and_then(Value::as_str);
            if kind == Some("server_tool_use") {
                let Some(id) = block.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if let Some((server_kind, use_field)) = server_tool_uses.get(id) {
                    pending.insert(id.to_owned(), (*server_kind, use_field.clone()));
                }
                continue;
            }
            let Some(result_kind) = (match kind {
                Some("tool_search_tool_result") => Some(ServerToolKind::ToolSearch),
                Some("web_search_tool_result") => Some(ServerToolKind::WebSearch),
                _ => None,
            }) else {
                if kind == Some("tool_result") {
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        if server_tool_uses.contains_key(id) {
                            return invalid(
                                format!("{field}.tool_use_id"),
                                format!(
                                    "server-tool call '{id}' must be completed by its server result block, not tool_result"
                                ),
                            );
                        }
                    }
                }
                continue;
            };
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            if completed.contains(id) {
                return invalid(
                    format!("{field}.tool_use_id"),
                    format!("server-tool call '{id}' has more than one result"),
                );
            }
            let Some((expected_kind, use_field)) = pending.get(id) else {
                let message = if server_tool_uses.contains_key(id) {
                    format!("server-tool result '{id}' appears before its server_tool_use")
                } else {
                    format!("server-tool result '{id}' has no matching server_tool_use")
                };
                return invalid(format!("{field}.tool_use_id"), message);
            };
            if *expected_kind != result_kind {
                return invalid(
                    format!("{field}.tool_use_id"),
                    format!("server-tool result '{id}' does not match the call at {use_field}"),
                );
            }
            completed.insert(id.to_owned());
            pending.remove(id);
        }
    }
    Ok(())
}

fn validate_tool_references(
    request: &ClaudeRequest,
    tool_names: &HashSet<&str>,
) -> Result<(), ValidationError> {
    for (message_index, message) in request.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            let references = match block.get("type").and_then(Value::as_str) {
                Some("tool_search_tool_result") => block
                    .pointer("/content/tool_references")
                    .and_then(Value::as_array),
                Some("tool_result") => block.get("content").and_then(Value::as_array),
                _ => None,
            };
            let Some(references) = references else {
                continue;
            };
            for (reference_index, reference) in references.iter().enumerate() {
                if reference.get("type").and_then(Value::as_str) != Some("tool_reference") {
                    continue;
                }
                let Some(name) = reference.get("tool_name").and_then(Value::as_str) else {
                    continue;
                };
                if !tool_names.contains(name) {
                    return invalid(
                        format!(
                            "messages.{message_index}.content.{block_index}.tool_reference.{reference_index}.tool_name"
                        ),
                        format!("referenced tool '{name}' is not defined in the top-level tools array"),
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_claude_system(value: Option<&Value>) -> Result<(), ValidationError> {
    let Some(value) = value else {
        return Ok(());
    };
    match value {
        Value::String(_) => Ok(()),
        Value::Array(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                if block.get("type").and_then(Value::as_str) != Some("text")
                    || !block.get("text").is_some_and(Value::is_string)
                {
                    return invalid(format!("system.{index}"), "expected a text content block");
                }
            }
            Ok(())
        }
        _ => invalid("system", "expected a string or content array"),
    }
}

fn validate_claude_content(
    content: &Value,
    role: &str,
    field: String,
) -> Result<(), ValidationError> {
    match content {
        Value::String(_) => return Ok(()),
        Value::Array(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                validate_claude_block(block, role, format!("{field}.{index}"), false)?;
            }
            return Ok(());
        }
        _ => {}
    }
    invalid(field, "expected a string or content array")
}

fn validate_claude_block(
    block: &Value,
    role: &str,
    field: String,
    tool_result_content: bool,
) -> Result<(), ValidationError> {
    let Some(kind) = block.get("type").and_then(Value::as_str) else {
        return invalid(format!("{field}.type"), "is required");
    };
    match kind {
        "text" => {
            if !block.get("text").is_some_and(Value::is_string) {
                return invalid(format!("{field}.text"), "expected a string");
            }
        }
        "image" => {
            if role != "user" {
                return invalid(format!("{field}.type"), "image blocks require a user role");
            }
            validate_claude_image(block, &field)?;
        }
        "tool_result" if !tool_result_content => {
            if role != "user" {
                return invalid(
                    format!("{field}.type"),
                    "tool_result blocks require a user role",
                );
            }
            if block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(format!("{field}.tool_use_id"), "is required");
            }
            if block
                .get("is_error")
                .is_some_and(|value| !value.is_boolean())
            {
                return invalid(format!("{field}.is_error"), "expected a boolean");
            }
            match block.get("content") {
                Some(Value::String(_)) => {}
                Some(Value::Array(blocks)) => {
                    for (index, nested) in blocks.iter().enumerate() {
                        validate_claude_block(
                            nested,
                            "user",
                            format!("{field}.content.{index}"),
                            true,
                        )?;
                    }
                }
                _ => {
                    return invalid(
                        format!("{field}.content"),
                        "expected a string or an array of text/image blocks",
                    )
                }
            }
        }
        "tool_use" if !tool_result_content => {
            if role != "assistant" {
                return invalid(
                    format!("{field}.type"),
                    "tool_use blocks require an assistant role",
                );
            }
            for name in ["id", "name"] {
                if block
                    .get(name)
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return invalid(format!("{field}.{name}"), "is required");
                }
            }
            if !block.get("input").is_some_and(Value::is_object) {
                return invalid(format!("{field}.input"), "expected an object");
            }
        }
        "server_tool_use" if !tool_result_content => {
            if role != "assistant" {
                return invalid(
                    format!("{field}.type"),
                    "server_tool_use blocks require an assistant role",
                );
            }
            for name in ["id", "name"] {
                if block
                    .get(name)
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return invalid(format!("{field}.{name}"), "is required");
                }
            }
            if !block.get("input").is_some_and(Value::is_object) {
                return invalid(format!("{field}.input"), "expected an object");
            }
        }
        "tool_search_tool_result" if !tool_result_content => {
            if role != "assistant" {
                return invalid(
                    format!("{field}.type"),
                    "tool_search_tool_result blocks require an assistant role",
                );
            }
            if block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(format!("{field}.tool_use_id"), "is required");
            }
            validate_tool_search_result(block.get("content"), &format!("{field}.content"))?;
        }
        "web_search_tool_result" if !tool_result_content => {
            if role != "assistant" {
                return invalid(
                    format!("{field}.type"),
                    "web_search_tool_result blocks require an assistant role",
                );
            }
            if block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(format!("{field}.tool_use_id"), "is required");
            }
            if block
                .get("caller")
                .is_some_and(|caller| caller.get("type").and_then(Value::as_str) != Some("direct"))
            {
                return invalid(
                    format!("{field}.caller"),
                    "only direct Web Search callers are supported",
                );
            }
            validate_web_search_result(block.get("content"), &format!("{field}.content"))?;
        }
        "tool_reference" if tool_result_content => {
            if block
                .get("tool_name")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(format!("{field}.tool_name"), "is required");
            }
        }
        "thinking" if !tool_result_content => {
            if role != "assistant" || !block.get("thinking").is_some_and(Value::is_string) {
                return invalid(
                    format!("{field}.thinking"),
                    "thinking blocks require an assistant role and string content",
                );
            }
        }
        "compaction" if !tool_result_content => {
            if role != "assistant" {
                return invalid(
                    format!("{field}.type"),
                    "compaction blocks require an assistant role",
                );
            }
            match block.get("content") {
                None | Some(Value::Null) => {}
                Some(Value::String(content)) if content.is_empty() => {
                    return invalid(format!("{field}.content"), "must not be empty");
                }
                Some(Value::String(_)) => {}
                _ => {
                    return invalid(format!("{field}.content"), "expected a string or null");
                }
            }
        }
        "document" => {
            return invalid(
                format!("{field}.type"),
                "document blocks are not supported by the Kiro upstream",
            )
        }
        _ => {
            return invalid(
                format!("{field}.type"),
                format!("unsupported Claude content block '{kind}'"),
            )
        }
    }
    Ok(())
}

fn validate_web_search_result(value: Option<&Value>, field: &str) -> Result<(), ValidationError> {
    match value {
        Some(Value::Array(results)) => {
            for (index, result) in results.iter().enumerate() {
                if result.get("type").and_then(Value::as_str) != Some("web_search_result") {
                    return invalid(
                        format!("{field}.{index}.type"),
                        "expected web_search_result",
                    );
                }
                for name in ["url", "title"] {
                    if result
                        .get(name)
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                    {
                        return invalid(format!("{field}.{index}.{name}"), "is required");
                    }
                }
                let Some(encrypted) = result
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                else {
                    return invalid(
                        format!("{field}.{index}.encrypted_content"),
                        "is required so web search results can be replayed on later turns",
                    );
                };
                // Anthropic-originated opaque values and kproxy-originated
                // replay payloads are both valid here. Only kproxy values can
                // be decoded locally; foreign opaque content is preserved.
                let _ = encrypted;
                if result
                    .get("page_age")
                    .is_some_and(|value| !value.is_null() && !value.is_string())
                {
                    return invalid(
                        format!("{field}.{index}.page_age"),
                        "expected a string or null",
                    );
                }
            }
            Ok(())
        }
        Some(Value::Object(error))
            if error.get("type").and_then(Value::as_str)
                == Some("web_search_tool_result_error") =>
        {
            if error
                .get("error_code")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(format!("{field}.error_code"), "is required");
            }
            Ok(())
        }
        _ => invalid(
            field,
            "expected web search result blocks or a web_search_tool_result_error object",
        ),
    }
}

fn validate_tool_search_result(value: Option<&Value>, field: &str) -> Result<(), ValidationError> {
    let Some(value) = value else {
        return invalid(field, "is required");
    };
    match value.get("type").and_then(Value::as_str) {
        Some("tool_search_tool_search_result") => {
            let Some(references) = value.get("tool_references").and_then(Value::as_array) else {
                return invalid(format!("{field}.tool_references"), "expected an array");
            };
            for (index, reference) in references.iter().enumerate() {
                if reference.get("type").and_then(Value::as_str) != Some("tool_reference")
                    || reference
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                {
                    return invalid(
                        format!("{field}.tool_references.{index}"),
                        "expected a tool_reference with tool_name",
                    );
                }
            }
            Ok(())
        }
        Some("tool_search_tool_result_error") => {
            if !value.get("error_code").is_some_and(Value::is_string) {
                return invalid(format!("{field}.error_code"), "is required");
            }
            if value
                .get("error_message")
                .is_some_and(|message| !message.is_null() && !message.is_string())
            {
                return invalid(
                    format!("{field}.error_message"),
                    "expected a string or null",
                );
            }
            Ok(())
        }
        _ => invalid(field, "expected a Tool Search result or error"),
    }
}

fn validate_claude_image(block: &Value, field: &str) -> Result<(), ValidationError> {
    let source = block.get("source").unwrap_or(&Value::Null);
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return invalid(
            format!("{field}.source.type"),
            "only base64 images are supported",
        );
    }
    if !source
        .get("media_type")
        .and_then(Value::as_str)
        .is_some_and(|media| {
            matches!(
                media,
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            )
        })
    {
        return invalid(
            format!("{field}.source.media_type"),
            "expected image/jpeg, image/png, image/gif, or image/webp",
        );
    }
    let Some(data) = source.get("data").and_then(Value::as_str) else {
        return invalid(format!("{field}.source.data"), "expected a string");
    };
    if base64::engine::general_purpose::STANDARD
        .decode(data)
        .is_err()
    {
        return invalid(format!("{field}.source.data"), "invalid base64 image data");
    }
    Ok(())
}

fn validate_context_management(value: Option<&Value>) -> Result<(), ValidationError> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(object) = value.as_object() else {
        return invalid("context_management", "expected an object");
    };
    let Some(edits) = object.get("edits").and_then(Value::as_array) else {
        return invalid("context_management.edits", "expected an array");
    };
    if edits.is_empty() {
        return invalid("context_management.edits", "must not be empty");
    }
    for (index, edit) in edits.iter().enumerate() {
        let Some(edit) = edit.as_object() else {
            return invalid(
                format!("context_management.edits.{index}"),
                "expected an object",
            );
        };
        let Some(edit_type) = edit.get("type").and_then(Value::as_str) else {
            return invalid(
                format!("context_management.edits.{index}.type"),
                "expected a string",
            );
        };
        if edit_type.trim().is_empty() {
            return invalid(
                format!("context_management.edits.{index}.type"),
                "must not be empty",
            );
        }
        // Context editing is an extensible Anthropic protocol. Kiro-account-manager
        // accepts unknown edit families and locally emulates only the strategies it
        // understands. Do the same here so a Claude Code upgrade cannot make every
        // request fail before routing. Compact edits retain strict validation because
        // kproxy actively implements their trigger and response semantics; other
        // families are handled by context normalization or remain safe no-ops.
        if !crate::is_compact_edit_type(edit_type) {
            continue;
        }
        if edit.keys().any(|key| {
            !matches!(
                key.as_str(),
                "type" | "trigger" | "pause_after_compaction" | "instructions"
            )
        }) {
            return invalid(
                format!("context_management.edits.{index}"),
                "contains an unsupported field",
            );
        }
        if edit
            .get("pause_after_compaction")
            .is_some_and(|value| !value.is_boolean())
        {
            return invalid(
                format!("context_management.edits.{index}.pause_after_compaction"),
                "expected a boolean",
            );
        }
        if edit.get("pause_after_compaction").and_then(Value::as_bool) == Some(true) {
            return invalid(
                format!("context_management.edits.{index}.pause_after_compaction"),
                "pausing after compaction is not supported by the Kiro upstream",
            );
        }
        if edit
            .get("instructions")
            .is_some_and(|value| !value.is_string())
        {
            return invalid(
                format!("context_management.edits.{index}.instructions"),
                "expected a string",
            );
        }
        if edit
            .get("instructions")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
        {
            return invalid(
                format!("context_management.edits.{index}.instructions"),
                "custom compaction instructions are not supported by the Kiro upstream",
            );
        }
        if let Some(trigger) = edit.get("trigger") {
            let Some(trigger) = trigger.as_object() else {
                return invalid(
                    format!("context_management.edits.{index}.trigger"),
                    "expected an object",
                );
            };
            if trigger.get("type").and_then(Value::as_str) != Some("input_tokens") {
                return invalid(
                    format!("context_management.edits.{index}.trigger.type"),
                    "expected input_tokens",
                );
            }
            let minimum = crate::MIN_COMPACT_TRIGGER_TOKENS;
            if trigger
                .get("value")
                .and_then(Value::as_u64)
                .is_none_or(|value| value < minimum)
            {
                return invalid(
                    format!("context_management.edits.{index}.trigger.value"),
                    format!("must be an integer of at least {minimum}"),
                );
            }
            if trigger
                .keys()
                .any(|key| !matches!(key.as_str(), "type" | "value"))
            {
                return invalid(
                    format!("context_management.edits.{index}.trigger"),
                    "contains an unsupported field",
                );
            }
        }
    }
    Ok(())
}

pub fn validate_openai(request: &OpenAiRequest) -> Result<(), ValidationError> {
    common(&request.model, request.messages.is_empty())?;
    if request
        .temperature
        .is_some_and(|value| !(0.0..=2.0).contains(&value))
    {
        return invalid("temperature", "must be in 0..=2");
    }
    if request
        .top_p
        .is_some_and(|value| !(0.0..=1.0).contains(&value))
    {
        return invalid("top_p", "must be in 0..=1");
    }
    if request.max_tokens == Some(0) || request.max_completion_tokens == Some(0) {
        return invalid("max_tokens", "token limits must be positive");
    }
    if request.max_tokens.is_some() && request.max_completion_tokens.is_some() {
        return invalid(
            "max_completion_tokens",
            "use either max_tokens or max_completion_tokens, not both",
        );
    }
    if let Some(options) = &request.stream_options {
        if !request.stream {
            return invalid("stream_options", "is only valid when stream is true");
        }
        let Some(options) = options.as_object() else {
            return invalid("stream_options", "expected an object");
        };
        if options.keys().any(|field| field != "include_usage")
            || options
                .get("include_usage")
                .is_some_and(|value| !value.is_boolean())
        {
            return invalid(
                "stream_options",
                "only the boolean include_usage field is supported",
            );
        }
    }
    if request
        .response_format
        .as_ref()
        .is_some_and(|format| format.get("type").and_then(Value::as_str) != Some("text"))
    {
        return invalid(
            "response_format",
            "only response_format.type=text is supported",
        );
    }
    for (index, message) in request.messages.iter().enumerate() {
        if !matches!(
            message.role.as_str(),
            "system" | "developer" | "user" | "assistant" | "tool"
        ) {
            return Err(ValidationError::InvalidRole(message.role.clone()));
        }
        validate_openai_message(message, index)?;
    }
    if request.tools.len() > MAX_TOOLS {
        return Err(ValidationError::TooManyTools);
    }
    let mut docs = 0usize;
    for (index, tool) in request.tools.iter().enumerate() {
        if !matches!(tool.r#type.as_str(), "function" | "custom") {
            return invalid(format!("tools.{index}.type"), "expected function or custom");
        }
        let definition = tool.body.get(&tool.r#type).unwrap_or(&Value::Null);
        if !definition.is_object() {
            return invalid(
                format!("tools.{index}.{}", tool.r#type),
                "expected an object",
            );
        }
        let name = definition
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name.trim().is_empty() {
            return invalid(
                format!("tools.{index}.{}.name", tool.r#type),
                "must not be empty",
            );
        }
        let empty_schema = Value::Object(serde_json::Map::new());
        let schema = definition.get("parameters").unwrap_or(&empty_schema);
        docs += definition
            .get("description")
            .and_then(Value::as_str)
            .map_or(0, |description| description.chars().count());
        if definition
            .get("description")
            .is_some_and(|description| !description.is_string())
        {
            return invalid(
                format!("tools.{index}.{}.description", tool.r#type),
                "expected a string",
            );
        }
        validate_tool(name, schema).map_err(|error| {
            if error == ValidationError::ToolNameTooLong {
                error
            } else {
                ValidationError::InvalidField {
                    field: format!("tools.{index}.{}.parameters", tool.r#type),
                    message: error.to_string(),
                }
            }
        })?;
        if tool.r#type == "function" {
            if definition
                .get("parameters")
                .is_some_and(|value| !value.is_object())
            {
                return invalid(
                    format!("tools.{index}.function.parameters"),
                    "expected an object",
                );
            }
            if definition
                .get("strict")
                .is_some_and(|strict| !strict.is_boolean())
            {
                return invalid(
                    format!("tools.{index}.function.strict"),
                    "expected a boolean",
                );
            }
            if definition.get("strict").and_then(Value::as_bool) == Some(true) {
                return invalid(
                    format!("tools.{index}.function.strict"),
                    "strict schemas are not supported by the Kiro upstream",
                );
            }
        } else {
            validate_custom_format(definition.get("format"), index)?;
        }
    }
    if docs > MAX_TOOL_DOC_CHARS {
        return Err(ValidationError::ToolDocumentationTooLarge);
    }
    validate_openai_tool_choice(request)?;
    validate_thinking(request.thinking.as_ref())?;
    Ok(())
}

fn validate_openai_message(
    message: &crate::OpenAiMessage,
    index: usize,
) -> Result<(), ValidationError> {
    if message.role == "tool"
        && message
            .tool_call_id
            .as_deref()
            .is_none_or(|id| id.trim().is_empty())
    {
        return invalid(
            format!("messages.{index}.tool_call_id"),
            "is required for tool messages",
        );
    }
    if message.content.is_none() && (message.role != "assistant" || message.tool_calls.is_empty()) {
        return invalid(format!("messages.{index}.content"), "is required");
    }
    if let Some(content) = &message.content {
        match content {
            Value::String(_) => {}
            Value::Array(parts) => {
                for (part_index, part) in parts.iter().enumerate() {
                    let kind = part.get("type").and_then(Value::as_str);
                    let valid = match kind {
                        Some("text") => part.get("text").is_some_and(Value::is_string),
                        Some("image_url") => {
                            part.pointer("/image_url/url").is_some_and(Value::is_string)
                        }
                        _ => false,
                    };
                    if !valid {
                        return invalid(
                            format!("messages.{index}.content.{part_index}"),
                            "expected a text or image_url content part",
                        );
                    }
                }
            }
            _ => {
                return invalid(
                    format!("messages.{index}.content"),
                    "expected a string or content array",
                )
            }
        }
    }
    for (tool_index, call) in message.tool_calls.iter().enumerate() {
        let field = format!("messages.{index}.tool_calls.{tool_index}");
        let kind = call.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("function" | "custom"))
            || call
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return invalid(
                field,
                "expected a named function or custom tool call with an id",
            );
        }
        let definition = call.get(kind.unwrap_or_default()).unwrap_or(&Value::Null);
        if definition
            .get("name")
            .and_then(Value::as_str)
            .is_none_or(|name| name.trim().is_empty())
        {
            return invalid(format!("{field}.name"), "tool name is required");
        }
        if kind == Some("function") {
            let Some(arguments) = definition.get("arguments").and_then(Value::as_str) else {
                return invalid(
                    format!("{field}.function.arguments"),
                    "expected a JSON string",
                );
            };
            if serde_json::from_str::<Value>(arguments).is_err() {
                return invalid(
                    format!("{field}.function.arguments"),
                    "contains invalid JSON",
                );
            }
        } else if !definition.get("input").is_some_and(Value::is_string) {
            return invalid(format!("{field}.custom.input"), "expected a string");
        }
    }
    Ok(())
}

fn validate_custom_format(format: Option<&Value>, index: usize) -> Result<(), ValidationError> {
    let Some(format) = format else {
        return Ok(());
    };
    match format.get("type").and_then(Value::as_str) {
        Some("text") => Ok(()),
        Some("grammar") => {
            let grammar = format.get("grammar").unwrap_or(&Value::Null);
            if grammar.get("definition").is_some_and(Value::is_string)
                && grammar
                    .get("syntax")
                    .and_then(Value::as_str)
                    .is_some_and(|syntax| matches!(syntax, "lark" | "regex"))
            {
                Ok(())
            } else {
                invalid(
                    format!("tools.{index}.custom.format.grammar"),
                    "requires a definition and lark or regex syntax",
                )
            }
        }
        _ => invalid(
            format!("tools.{index}.custom.format.type"),
            "expected text or grammar",
        ),
    }
}

fn validate_thinking(thinking: Option<&crate::ThinkingConfig>) -> Result<(), ValidationError> {
    let Some(thinking) = thinking else {
        return Ok(());
    };
    if !matches!(
        thinking.r#type.as_str(),
        "enabled" | "adaptive" | "disabled"
    ) {
        return invalid("thinking.type", "expected enabled, adaptive, or disabled");
    }
    if thinking.r#type == "enabled" && thinking.budget_tokens.unwrap_or_default() < 1_024 {
        return invalid(
            "thinking.budget_tokens",
            "enabled thinking requires at least 1024 tokens",
        );
    }
    if thinking.r#type == "disabled" && thinking.budget_tokens.is_some() {
        return invalid(
            "thinking.budget_tokens",
            "budget is not valid for disabled thinking",
        );
    }
    Ok(())
}

fn validate_openai_tool_choice(request: &OpenAiRequest) -> Result<(), ValidationError> {
    let Some(choice) = request.tool_choice.as_ref() else {
        return Ok(());
    };
    if choice
        .as_str()
        .is_some_and(|value| matches!(value, "none" | "auto"))
    {
        return Ok(());
    }
    if choice.as_str() == Some("required") {
        return if request.tools.is_empty() {
            invalid("tool_choice", "required requires at least one tool")
        } else {
            Ok(())
        };
    }
    for kind in ["function", "custom"] {
        if let Some(name) = choice
            .get(kind)
            .and_then(|definition| definition.get("name"))
            .and_then(Value::as_str)
        {
            if choice.get("type").and_then(Value::as_str) != Some(kind) {
                return invalid(
                    "tool_choice.type",
                    format!("expected '{kind}' for a {kind} tool choice"),
                );
            }
            let found = request.tools.iter().any(|tool| {
                tool.r#type == kind
                    && tool
                        .body
                        .get(kind)
                        .and_then(|definition| definition.get("name"))
                        .and_then(Value::as_str)
                        == Some(name)
            });
            return if found {
                Ok(())
            } else {
                invalid(
                    format!("tool_choice.{kind}.name"),
                    format!("tool '{name}' was not found"),
                )
            };
        }
    }
    if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") {
        let allowed = choice.get("allowed_tools").unwrap_or(&Value::Null);
        if !allowed
            .get("mode")
            .and_then(Value::as_str)
            .is_some_and(|mode| matches!(mode, "auto" | "required"))
        {
            return invalid(
                "tool_choice.allowed_tools.mode",
                "expected auto or required",
            );
        }
        let Some(tools) = allowed.get("tools").and_then(Value::as_array) else {
            return invalid(
                "tool_choice.allowed_tools.tools",
                "expected a non-empty array",
            );
        };
        if tools.is_empty() {
            return invalid(
                "tool_choice.allowed_tools.tools",
                "expected a non-empty array",
            );
        }
        let mut seen = std::collections::HashSet::new();
        for (index, reference) in tools.iter().enumerate() {
            let Some(kind) = reference
                .get("type")
                .and_then(Value::as_str)
                .filter(|kind| matches!(*kind, "function" | "custom"))
            else {
                return invalid(
                    format!("tool_choice.allowed_tools.tools.{index}.type"),
                    "expected function or custom",
                );
            };
            let Some(name) = reference
                .get(kind)
                .and_then(|definition| definition.get("name"))
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
            else {
                return invalid(
                    format!("tool_choice.allowed_tools.tools.{index}.{kind}.name"),
                    "tool name is required",
                );
            };
            let identity = format!("{kind}:{name}");
            if !seen.insert(identity) {
                return invalid(
                    format!("tool_choice.allowed_tools.tools.{index}.{kind}.name"),
                    "duplicate allowed tool",
                );
            }
            let found = request.tools.iter().any(|tool| {
                tool.r#type == kind
                    && tool
                        .body
                        .get(kind)
                        .and_then(|definition| definition.get("name"))
                        .and_then(Value::as_str)
                        == Some(name)
            });
            if !found {
                return invalid(
                    format!("tool_choice.allowed_tools.tools.{index}.{kind}.name"),
                    format!("tool '{name}' was not found"),
                );
            }
        }
        return Ok(());
    }
    invalid("tool_choice", "unsupported tool choice")
}

fn invalid<T>(field: impl Into<String>, message: impl Into<String>) -> Result<T, ValidationError> {
    Err(ValidationError::InvalidField {
        field: field.into(),
        message: message.into(),
    })
}

fn common(model: &str, messages_empty: bool) -> Result<(), ValidationError> {
    if model.trim().is_empty() {
        return Err(ValidationError::MissingModel);
    }
    if messages_empty {
        return Err(ValidationError::MissingMessages);
    }
    Ok(())
}

fn validate_tool(name: &str, schema: &Value) -> Result<(), ValidationError> {
    if name.chars().count() > MAX_TOOL_NAME_CHARS {
        return Err(ValidationError::ToolNameTooLong);
    }
    let mut stack = vec![(schema, 1usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = stack.pop() {
        nodes += 1;
        if nodes > MAX_SCHEMA_NODES {
            return Err(ValidationError::SchemaTooLarge);
        }
        if depth > MAX_SCHEMA_DEPTH {
            return Err(ValidationError::SchemaTooDeep);
        }
        match value {
            Value::Array(values) => {
                stack.extend(values.iter().map(|child| (child, depth + 1)));
            }
            Value::Object(values) => {
                stack.extend(values.values().map(|child| (child, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

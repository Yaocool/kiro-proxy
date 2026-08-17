//! Request validation and bounded tool-schema traversal.

use base64::Engine as _;
use serde_json::Value;
use thiserror::Error;

use crate::{ClaudeRequest, OpenAiRequest};

pub const MAX_SCHEMA_DEPTH: usize = 64;
pub const MAX_SCHEMA_NODES: usize = 50_000;
pub const MAX_TOOL_DOC_CHARS: usize = 512_000;
pub const MAX_TOOLS: usize = 256;
pub const MAX_TOOL_NAME_CHARS: usize = 1_024;

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
    if request.tools.len() > MAX_TOOLS {
        return Err(ValidationError::TooManyTools);
    }
    let docs = request
        .tools
        .iter()
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
    for (index, tool) in request.tools.iter().enumerate() {
        if tool.name.trim().is_empty() {
            return invalid(format!("tools.{index}.name"), "must not be empty");
        }
        let kind = tool.r#type.as_deref();
        let web_tool = kind
            .is_some_and(|kind| kind.starts_with("web_search") || kind.starts_with("web_fetch"));
        if kind.is_some_and(|kind| kind != "custom") && !web_tool {
            return invalid(
                format!("tools.{index}.type"),
                format!(
                    "unsupported Claude server tool '{}'",
                    kind.unwrap_or_default()
                ),
            );
        }
        if !web_tool {
            validate_tool(&tool.name, &tool.input_schema).map_err(|error| {
                ValidationError::InvalidField {
                    field: format!("tools.{index}.input_schema"),
                    message: error.to_string(),
                }
            })?;
        }
        if tool.strict == Some(true) {
            return invalid(
                format!("tools.{index}.strict"),
                "strict tool schemas are not supported by the Kiro upstream",
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
                            .is_some_and(|kind| kind.starts_with("web_search")))
                    || (name == "web_fetch"
                        && tool
                            .r#type
                            .as_deref()
                            .is_some_and(|kind| kind.starts_with("web_fetch")))
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
        "thinking" if !tool_result_content => {
            if role != "assistant" || !block.get("thinking").is_some_and(Value::is_string) {
                return invalid(
                    format!("{field}.thinking"),
                    "thinking blocks require an assistant role and string content",
                );
            }
        }
        "compaction" if !tool_result_content => {
            if role != "assistant" || !block.get("content").is_some_and(Value::is_string) {
                return invalid(
                    format!("{field}.content"),
                    "compaction blocks require an assistant role and string content",
                );
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
    if object.keys().any(|key| key != "edits") {
        return invalid("context_management", "only the edits field is supported");
    }
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
        if edit.get("type").and_then(Value::as_str) != Some("compact_20260112") {
            return invalid(
                format!("context_management.edits.{index}.type"),
                "only compact_20260112 is supported",
            );
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
        validate_tool(name, schema).map_err(|error| ValidationError::InvalidField {
            field: format!("tools.{index}.{}.parameters", tool.r#type),
            message: error.to_string(),
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
mod tests {
    use super::*;
    use crate::{ClaudeMessage, ClaudeTool};

    fn request() -> ClaudeRequest {
        ClaudeRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![ClaudeMessage {
                role: "user".into(),
                content: Value::String("hi".into()),
            }],
            max_tokens: 100,
            temperature: None,
            top_p: None,
            stream: false,
            system: None,
            tools: vec![],
            tool_choice: None,
            thinking: None,
            context_management: None,
        }
    }

    fn tool(index: usize) -> ClaudeTool {
        ClaudeTool {
            r#type: None,
            name: format!("tool_{index}"),
            description: String::new(),
            input_schema: serde_json::json!({"type":"object"}),
            cache_control: None,
            strict: None,
            input_examples: None,
        }
    }

    #[test]
    fn enforces_tool_count_boundary() {
        let mut input = request();
        input.tools = (0..MAX_TOOLS).map(tool).collect();
        validate_claude(&input).expect("maximum tool count should be accepted");

        input.tools.push(tool(MAX_TOOLS));
        assert_eq!(validate_claude(&input), Err(ValidationError::TooManyTools));
    }

    #[test]
    fn rejects_excessive_schema_depth() {
        let mut schema = serde_json::json!({});
        for _ in 0..MAX_SCHEMA_DEPTH {
            schema = serde_json::json!({"child": schema});
        }
        let mut input = request();
        input.tools.push(ClaudeTool {
            r#type: None,
            name: "deep".into(),
            description: String::new(),
            input_schema: schema,
            cache_control: None,
            strict: None,
            input_examples: None,
        });
        assert_eq!(
            validate_claude(&input),
            Err(ValidationError::InvalidField {
                field: "tools.0.input_schema".into(),
                message: ValidationError::SchemaTooDeep.to_string(),
            })
        );
    }

    #[test]
    fn rejects_strict_claude_tools_and_invalid_input_examples() {
        let mut input = request();
        input.tools.push(ClaudeTool {
            r#type: Some("custom".into()),
            name: "strict".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type":"object"}),
            cache_control: None,
            strict: Some(true),
            input_examples: Some(vec![Value::String("not an object".into())]),
        });
        assert!(validate_claude(&input)
            .expect_err("strict")
            .to_string()
            .contains("strict"));
    }

    #[test]
    fn validates_supported_compaction_configuration() {
        let mut input = request();
        input.context_management = Some(serde_json::json!({"edits":[{
            "type":"compact_20260112",
            "trigger":{"type":"input_tokens","value":75_000},
            "pause_after_compaction":false
        }]}));
        validate_claude(&input).expect("supported compact configuration");

        input.context_management = Some(serde_json::json!({"edits":[{
            "type":"clear_tool_uses_20250919"
        }]}));
        assert!(validate_claude(&input)
            .expect_err("unsupported context edit")
            .to_string()
            .contains("compact_20260112"));
    }

    #[test]
    fn validates_nested_tool_result_images_and_rejects_unsupported_blocks() {
        let mut input = request();
        input.messages[0].content = serde_json::json!([{
            "type":"tool_result","tool_use_id":"tool_1","content":[{
                "type":"image","source":{
                    "type":"base64","media_type":"image/png","data":"aGVsbG8="
                }
            }]
        }]);
        validate_claude(&input).expect("nested image");

        input.messages[0].content = serde_json::json!([{
            "type":"document","source":{"type":"base64","data":"aGVsbG8="}
        }]);
        assert!(validate_claude(&input)
            .expect_err("document")
            .to_string()
            .contains("not supported"));

        input.messages[0].content = serde_json::json!([{"type":"future_block"}]);
        assert!(validate_claude(&input)
            .expect_err("unknown block")
            .to_string()
            .contains("unsupported Claude content block"));
    }

    #[test]
    fn openai_business_fields_and_tool_call_json_are_validated() {
        let mut input: OpenAiRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4",
            "messages":[{"role":"assistant","tool_calls":[{
                "id":"call_1","type":"function",
                "function":{"name":"lookup","arguments":"not-json"}
            }]}],
            "temperature":1.0
        }))
        .expect("request");
        assert!(validate_openai(&input)
            .expect_err("arguments")
            .to_string()
            .contains("invalid JSON"));
        input.messages = vec![crate::OpenAiMessage {
            role: "user".into(),
            content: Some(Value::String("hi".into())),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }];
        input.max_tokens = Some(1);
        input.max_completion_tokens = Some(1);
        assert!(validate_openai(&input)
            .expect_err("exclusive limits")
            .to_string()
            .contains("either"));
    }
}

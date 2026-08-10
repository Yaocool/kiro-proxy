use base64::Engine;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{
    KiroImage, KiroImageSource, KiroInferenceConfig, KiroMessageContext, KiroText, KiroTool,
    KiroToolResult, KiroToolSpecification,
};

pub const SIGNATURE_PLACEHOLDER: &str = "kiro-proxy-placeholder-signature";
const MAX_KIRO_TOOL_NAME: usize = 64;
const MAX_KIRO_DESCRIPTION: usize = 1024;

pub fn system_text(system: Option<&Value>) -> String {
    match system {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") | Some("thinking") | Some("compaction") => block
                    .get("text")
                    .or_else(|| block.get("thinking"))
                    .and_then(Value::as_str),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn extract_images(content: &Value) -> Vec<KiroImage> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            let source = block.get("source")?;
            if block.get("type")?.as_str()? != "image" || source.get("type")?.as_str()? != "base64"
            {
                return None;
            }
            let media = source.get("media_type")?.as_str()?;
            let format = media.strip_prefix("image/").unwrap_or(media);
            let bytes = source.get("data")?.as_str()?.to_string();
            base64::engine::general_purpose::STANDARD
                .decode(&bytes)
                .ok()?;
            Some(KiroImage {
                format: format.into(),
                source: KiroImageSource { bytes },
            })
        })
        .collect()
}

pub fn extract_openai_images(content: &Value) -> Vec<KiroImage> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block.get("type")?.as_str()? != "image_url" {
                return None;
            }
            let url = block.get("image_url")?.get("url")?.as_str()?;
            let encoded = url.strip_prefix("data:image/")?;
            let (format, bytes) = encoded.split_once(";base64,")?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(bytes)
                .ok()?;
            if decoded.len() > 10 * 1024 * 1024 {
                return None;
            }
            Some(KiroImage {
                format: normalize_image_format(format).into(),
                source: KiroImageSource {
                    bytes: bytes.into(),
                },
            })
        })
        .collect()
}

fn normalize_image_format(format: &str) -> &str {
    match format.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "jpeg",
        "png" => "png",
        "gif" => "gif",
        "webp" => "webp",
        _ => "png",
    }
}

pub fn extract_tool_results(content: &Value) -> Vec<KiroToolResult> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| {
            let tool_use_id = block.get("tool_use_id")?.as_str()?.to_string();
            let result = block.get("content").map(content_text).unwrap_or_default();
            Some(KiroToolResult {
                content: vec![KiroText {
                    text: if result.is_empty() {
                        "(empty result)".into()
                    } else {
                        result
                    },
                }],
                status: if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                    "error"
                } else {
                    "success"
                }
                .into(),
                tool_use_id,
            })
        })
        .collect()
}

pub fn tool_name(name: &str) -> String {
    use std::fmt::Write as _;

    let normalized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if normalized.len() <= MAX_KIRO_TOOL_NAME && !normalized.is_empty() {
        return normalized;
    }
    let digest = Sha256::digest(name.as_bytes());
    let suffix = digest[..6].iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    });
    let prefix_length = MAX_KIRO_TOOL_NAME - suffix.len() - 1;
    let mut prefix = normalized.chars().take(prefix_length).collect::<String>();
    if prefix.is_empty() {
        prefix.push_str("tool");
    }
    format!("{prefix}_{suffix}")
}

pub fn kiro_tool(name: &str, description: &str, schema: &Value) -> (KiroTool, Option<String>) {
    let kiro_name = tool_name(name);
    let (description, documentation) = if description.chars().count() > MAX_KIRO_DESCRIPTION {
        (
            format!("[Full documentation in system prompt under '## Tool: {name}']"),
            Some(format!("## Tool: {name}\n\n{description}")),
        )
    } else {
        (description.to_string(), None)
    };
    (
        KiroTool {
            tool_specification: KiroToolSpecification {
                name: kiro_name,
                description,
                input_schema: crate::KiroInputSchema {
                    json: sanitize_schema(schema),
                },
            },
        },
        documentation,
    )
}

fn sanitize_schema(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(sanitize_schema).collect()),
        Value::Object(values) => {
            let mut output = Map::new();
            for (key, child) in values {
                if matches!(key.as_str(), "additionalProperties" | "$schema" | "strict") {
                    continue;
                }
                if key == "required" && child.as_array().is_some_and(Vec::is_empty) {
                    continue;
                }
                output.insert(key.clone(), sanitize_schema(child));
            }
            Value::Object(output)
        }
        other => other.clone(),
    }
}

pub fn context(
    tools: Vec<KiroTool>,
    tool_results: Vec<KiroToolResult>,
) -> Option<KiroMessageContext> {
    (!tools.is_empty() || !tool_results.is_empty()).then_some(KiroMessageContext {
        tools,
        tool_results,
    })
}

pub fn inference(
    requested: Option<u32>,
    has_tools: bool,
    temperature: Option<f64>,
    top_p: Option<f64>,
) -> KiroInferenceConfig {
    let maximum = requested.unwrap_or(8192).clamp(1, 64_000);
    KiroInferenceConfig {
        max_tokens: if has_tools {
            maximum.max(4096)
        } else {
            maximum
        },
        temperature,
        top_p,
    }
}

pub fn enhance_system(mut system: String, has_write: bool) -> String {
    let rich = system.to_ascii_lowercase().contains("claude code");
    if !rich {
        system.push_str(
            "\n\nExecute the user's request directly and use provided tools when needed.",
        );
    }
    if has_write && !system.contains("Write/create/edit tool arguments must stay small") {
        system.push_str(
            "\n\nWrite/create/edit tool arguments must stay small; split large writes into chunks.",
        );
    }
    system.trim().to_string()
}

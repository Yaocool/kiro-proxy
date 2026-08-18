use base64::Engine;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};

use crate::{
    KiroImage, KiroImageSource, KiroInferenceConfig, KiroMessageContext, KiroText, KiroTool,
    KiroToolResult, KiroToolSpecification, KiroUserInputMessage,
};

pub const SIGNATURE_PLACEHOLDER: &str = "kiro-proxy-placeholder-signature";
const MAX_KIRO_TOOL_NAME: usize = 64;
const MAX_KIRO_DESCRIPTION: usize = 1024;

/// Tool definitions belong only on Kiro's current message. Keeping them on
/// copied history turns multiplies a large catalog on every internal server
/// tool continuation and can exceed the upstream payload budget.
pub(crate) fn history_user_without_tools(
    mut message: KiroUserInputMessage,
) -> KiroUserInputMessage {
    if let Some(context) = message.user_input_message_context.as_mut() {
        context.tools.clear();
        if context.tool_results.is_empty() {
            message.user_input_message_context = None;
        }
    }
    message
}

/// Request-scoped, reversible mapping between client tool names and Kiro's
/// restricted 64-byte name space. Building it for the whole catalog avoids
/// silent aliasing when two legal client names normalize to the same value.
#[derive(Debug, Clone, Default)]
pub struct ToolNameRegistry {
    original_to_kiro: HashMap<String, String>,
    kiro_to_original: HashMap<String, String>,
}

impl ToolNameRegistry {
    pub fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let originals = names
            .into_iter()
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let mut base_counts = HashMap::<String, usize>::new();
        for original in &originals {
            *base_counts.entry(tool_name(original)).or_default() += 1;
        }

        let mut registry = Self::default();
        for original in originals {
            let base = tool_name(&original);
            let needs_hash = base_counts.get(&base).copied().unwrap_or_default() > 1
                || registry.kiro_to_original.contains_key(&base);
            let mut nonce = 0u32;
            let kiro = loop {
                let candidate = if needs_hash || nonce > 0 {
                    collision_tool_name(&original, nonce)
                } else {
                    base.clone()
                };
                if !registry.kiro_to_original.contains_key(&candidate) {
                    break candidate;
                }
                nonce = nonce.saturating_add(1);
            };
            registry
                .original_to_kiro
                .insert(original.clone(), kiro.clone());
            registry.kiro_to_original.insert(kiro, original);
        }
        registry
    }

    pub fn kiro_name(&self, original: &str) -> String {
        self.original_to_kiro
            .get(original)
            .cloned()
            .unwrap_or_else(|| tool_name(original))
    }

    pub fn restore_map(&self) -> HashMap<String, String> {
        self.kiro_to_original.clone()
    }
}

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
                    .or_else(|| block.get("content"))
                    .and_then(Value::as_str),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn extract_images(content: &Value) -> Vec<KiroImage> {
    let mut images = Vec::new();
    collect_claude_images(content, &mut images);
    images
}

fn collect_claude_images(content: &Value, images: &mut Vec<KiroImage>) {
    let Some(blocks) = content.as_array() else {
        return;
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("image") => {
                if let Some(image) = claude_image(block) {
                    images.push(image);
                }
            }
            Some("tool_result") => {
                if let Some(content) = block.get("content") {
                    collect_claude_images(content, images);
                }
            }
            _ => {}
        }
    }
}

fn claude_image(block: &Value) -> Option<KiroImage> {
    let source = block.get("source")?;
    if source.get("type")?.as_str()? != "base64" {
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
            let has_images = block
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|block| block.get("type").and_then(Value::as_str) == Some("image"))
                });
            Some(KiroToolResult {
                content: vec![KiroText {
                    text: if result.is_empty() {
                        if has_images {
                            "(image result attached)"
                        } else {
                            "(empty result)"
                        }
                        .into()
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

fn collision_tool_name(name: &str, nonce: u32) -> String {
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
    let digest = Sha256::digest(format!("{name}\0{nonce}").as_bytes());
    let suffix = digest[..8].iter().fold(String::new(), |mut output, byte| {
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
    kiro_tool_named(name, &tool_name(name), description, schema)
}

pub fn kiro_tool_named(
    original_name: &str,
    kiro_name: &str,
    description: &str,
    schema: &Value,
) -> (KiroTool, Option<String>) {
    let (description, documentation) = if description.chars().count() > MAX_KIRO_DESCRIPTION {
        (
            format!("[Full documentation in system prompt under '## Tool: {original_name}']"),
            Some(format!("## Tool: {original_name}\n\n{description}")),
        )
    } else {
        (description.to_string(), None)
    };
    (
        KiroTool {
            tool_specification: KiroToolSpecification {
                name: kiro_name.to_owned(),
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
    _has_tools: bool,
    temperature: Option<f64>,
    top_p: Option<f64>,
) -> KiroInferenceConfig {
    let maximum = requested.unwrap_or(8192).clamp(1, 64_000);
    KiroInferenceConfig {
        // The client limit applies to the whole assistant turn, including
        // internal server-tool continuations. Inflating tool requests to 4096
        // lets the first upstream round violate that contract.
        max_tokens: maximum,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_registry_resolves_normalization_collisions_reversibly() {
        let registry = ToolNameRegistry::new(["mcp.a/read", "mcp_a/read"]);
        let first = registry.kiro_name("mcp.a/read");
        let second = registry.kiro_name("mcp_a/read");
        assert_ne!(first, second);
        assert!(first.len() <= MAX_KIRO_TOOL_NAME);
        assert!(second.len() <= MAX_KIRO_TOOL_NAME);
        let reverse = registry.restore_map();
        assert_eq!(reverse.get(&first).map(String::as_str), Some("mcp.a/read"));
        assert_eq!(reverse.get(&second).map(String::as_str), Some("mcp_a/read"));
    }
}

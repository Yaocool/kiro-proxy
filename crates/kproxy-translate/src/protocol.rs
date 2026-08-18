//! Wire models. Optional and unknown fields intentionally remain forward compatible.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Matches an unversioned protocol type or any versioned/aliased member of
/// that type family without assuming a date-shaped suffix.
pub fn matches_type_family(kind: &str, base: &str) -> bool {
    kind == base
        || kind
            .strip_prefix(base)
            .is_some_and(|suffix| suffix.starts_with('_'))
}

/// Anthropic Messages request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeRequest {
    pub model: String,
    pub messages: Vec<ClaudeMessage>,
    pub max_tokens: u32,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub tools: Vec<ClaudeTool>,
    #[serde(default)]
    pub tool_choice: Option<ClaudeToolChoice>,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub context_management: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeMessage {
    pub role: String,
    pub content: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeTool {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "object_schema")]
    pub input_schema: Value,
    #[serde(default)]
    pub cache_control: Option<Value>,
    #[serde(default)]
    pub strict: Option<bool>,
    #[serde(default)]
    pub input_examples: Option<Vec<Value>>,
    /// Anthropic Tool Search hint. Deferred definitions stay in the request
    /// catalog but are not loaded into the model context until discovered.
    #[serde(default)]
    pub defer_loading: bool,
    /// Anthropic programmatic-tool-calling policy. Only direct calls can be
    /// represented by the Kiro upstream today; validation rejects the other
    /// values instead of silently weakening the contract.
    #[serde(default)]
    pub allowed_callers: Option<Vec<String>>,
    /// Fine-grained tool-input streaming is an execution guarantee, not a
    /// schema hint, so it must be validated explicitly.
    #[serde(default)]
    pub eager_input_streaming: Option<bool>,
    /// Server-side web search controls.
    #[serde(default)]
    pub max_uses: Option<u32>,
    #[serde(default)]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    pub blocked_domains: Option<Vec<String>>,
    #[serde(default)]
    pub user_location: Option<Value>,
    #[serde(default)]
    pub response_inclusion: Option<String>,
    /// Keep unknown tool-level fields visible to validation. Silently dropping
    /// a future caller, execution, or safety control could weaken the contract
    /// the client asked Anthropic to enforce.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeToolChoice {
    pub r#type: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub disable_parallel_tool_use: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub budget_tokens: Option<u32>,
}

/// OpenAI Chat Completions request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stream_options: Option<Value>,
    #[serde(default)]
    pub tools: Vec<OpenAiTool>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default = "default_parallel")]
    pub parallel_tool_calls: bool,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub response_format: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub tool_calls: Vec<Value>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiTool {
    pub r#type: String,
    #[serde(flatten)]
    pub body: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroPayload {
    pub conversation_state: KiroConversationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_config: Option<KiroInferenceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroConversationState {
    pub chat_trigger_type: String,
    pub conversation_id: String,
    pub current_message: KiroCurrentMessage,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub history: Vec<KiroHistoryMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroCurrentMessage {
    pub user_input_message: KiroUserInputMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroUserInputMessage {
    pub content: String,
    pub model_id: String,
    pub origin: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub images: Vec<KiroImage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_input_message_context: Option<KiroMessageContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroImage {
    pub format: String,
    pub source: KiroImageSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroImageSource {
    pub bytes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KiroMessageContext {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_results: Vec<KiroToolResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<KiroTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroToolResult {
    pub content: Vec<KiroText>,
    pub status: String,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroText {
    pub text: String,
}

/// Normalized Kiro MCP web search payload (the JSON string nested inside the
/// MCP `result.content[].text` block).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResults {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub total_results: u64,
    #[serde(default)]
    pub results: Vec<WebSearchResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResult {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub published_date: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroTool {
    pub tool_specification: KiroToolSpecification,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroToolSpecification {
    pub name: String,
    pub description: String,
    pub input_schema: KiroInputSchema,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroInputSchema {
    pub json: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroHistoryMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_input_message: Option<KiroUserInputMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_response_message: Option<KiroAssistantMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroAssistantMessage {
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_uses: Vec<KiroToolUse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroToolUse {
    pub tool_use_id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroInferenceConfig {
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

fn object_schema() -> Value {
    serde_json::json!({"type":"object","properties":{}})
}

fn default_parallel() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::matches_type_family;

    #[test]
    fn protocol_type_families_do_not_assume_a_version_format() {
        assert!(matches_type_family("web_search", "web_search"));
        assert!(matches_type_family("web_search_20260318", "web_search"));
        assert!(matches_type_family("web_search_next", "web_search"));
        assert!(matches_type_family("web_search_", "web_search"));
        assert!(!matches_type_family("web_searcher", "web_search"));
        assert!(!matches_type_family("other_web_search_next", "web_search"));
    }
}

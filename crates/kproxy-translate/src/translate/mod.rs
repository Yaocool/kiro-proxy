//! Request translators.

mod auto_continue;
mod claude;
mod common;
mod openai;
mod tool_search;
mod web_search;

pub use auto_continue::{
    auto_continue_payload, resume_tool_search_payload, tool_search_continue_payload,
    tool_search_continue_payload_batch,
};
pub use claude::{
    claude_loaded_tools, claude_pending_server_tool_uses, claude_to_kiro, claude_tool_name_map,
};
pub use common::{tool_name, ToolNameRegistry, SIGNATURE_PLACEHOLDER};
pub use openai::openai_to_kiro;
pub use tool_search::{
    is_tool_search_tool, is_tool_search_type, tool_search_kiro_tool, ClaudeToolSearchBudget,
    ClaudeToolSearchCatalog, ClaudeToolSearchError, ClaudeToolSearchOutcome, ClaudeToolSearchTrace,
};
pub use web_search::{
    format_web_search_results, resume_web_search_payload, validate_web_search_replay_content,
    web_search_continue_payload, web_search_continue_payload_batch, ClaudeServerToolEmission,
    ClaudeWebSearchError, ClaudeWebSearchTrace, WebSearchReplayCodec,
};

#[derive(Debug, Clone)]
pub struct TranslationOptions {
    pub model_id: String,
    pub origin: String,
    pub profile_arn: Option<String>,
    pub enhance_system_prompt: bool,
    pub compact_mode: bool,
    pub web_search_replay: Option<WebSearchReplayCodec>,
}

impl TranslationOptions {
    pub fn new(model_id: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            origin: origin.into(),
            profile_arn: None,
            enhance_system_prompt: true,
            compact_mode: false,
            web_search_replay: None,
        }
    }
}

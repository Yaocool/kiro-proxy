//! Request translators.

mod auto_continue;
mod claude;
mod common;
mod openai;
mod tool_search;

pub use auto_continue::{auto_continue_payload, tool_search_continue_payload};
pub use claude::{claude_loaded_tools, claude_to_kiro};
pub use common::{tool_name, SIGNATURE_PLACEHOLDER};
pub use openai::openai_to_kiro;
pub use tool_search::{
    is_tool_search_tool, is_tool_search_type, tool_search_kiro_tool, ClaudeToolSearchCatalog,
    ClaudeToolSearchError, ClaudeToolSearchOutcome, ClaudeToolSearchTrace,
};

#[derive(Debug, Clone)]
pub struct TranslationOptions {
    pub model_id: String,
    pub origin: String,
    pub profile_arn: Option<String>,
    pub enhance_system_prompt: bool,
    pub compact_mode: bool,
}

impl TranslationOptions {
    pub fn new(model_id: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            origin: origin.into(),
            profile_arn: None,
            enhance_system_prompt: true,
            compact_mode: false,
        }
    }
}

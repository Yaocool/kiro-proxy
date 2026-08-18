//! Claude/OpenAI 与 Kiro 协议之间的纯转换层。

pub mod context;
pub mod error;
pub mod model;
pub mod protocol;
pub mod tokenizer;
pub mod translate;
pub mod validate;

pub use context::{
    apply_compaction_boundary, compact_trigger_tokens, DEFAULT_COMPACT_TRIGGER_TOKENS,
    MIN_COMPACT_TRIGGER_TOKENS,
};
pub use error::{error_envelope, sanitize_error_message, ErrorFormat};
pub use protocol::*;
pub use tokenizer::{ContextCompactionStats, TokenCountCache, TokenCountStats};
pub use translate::{
    auto_continue_payload, claude_loaded_tools, claude_pending_server_tool_uses, claude_to_kiro,
    claude_tool_name_map, format_web_search_results, is_tool_search_tool, is_tool_search_type,
    openai_to_kiro, resume_tool_search_payload, resume_web_search_payload, tool_name,
    tool_search_continue_payload, tool_search_continue_payload_batch, tool_search_kiro_tool,
    validate_web_search_replay_content, web_search_continue_payload,
    web_search_continue_payload_batch, ClaudeServerToolEmission, ClaudeToolSearchBudget,
    ClaudeToolSearchCatalog, ClaudeToolSearchError, ClaudeToolSearchOutcome, ClaudeToolSearchTrace,
    ClaudeWebSearchError, ClaudeWebSearchTrace, ToolNameRegistry, TranslationOptions,
    WebSearchReplayCodec, SIGNATURE_PLACEHOLDER,
};
pub use validate::{validate_claude, validate_openai, ValidationError};

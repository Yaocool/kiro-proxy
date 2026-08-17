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
    auto_continue_payload, claude_to_kiro, openai_to_kiro, tool_name, TranslationOptions,
    SIGNATURE_PLACEHOLDER,
};
pub use validate::{validate_claude, validate_openai, ValidationError};

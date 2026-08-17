//! Claude/OpenAI 与 Kiro 协议之间的纯转换层。

pub mod error;
pub mod model;
pub mod protocol;
pub mod tokenizer;
pub mod translate;
pub mod validate;

pub use error::{error_envelope, sanitize_error_message, ErrorFormat};
pub use protocol::*;
pub use tokenizer::{TokenCountCache, TokenCountStats};
pub use translate::{
    auto_continue_payload, claude_to_kiro, openai_to_kiro, tool_name, TranslationOptions,
    SIGNATURE_PLACEHOLDER,
};
pub use validate::{validate_claude, validate_openai, ValidationError};

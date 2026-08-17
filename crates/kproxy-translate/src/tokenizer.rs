//! Async tokenizer with a bounded content-hash LRU.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use sha2::{Digest, Sha256};
use tiktoken_rs::{cl100k_base, CoreBPE};

use crate::{KiroAssistantMessage, KiroPayload, KiroUserInputMessage};

const MIN_CACHE_CHARS: usize = 512;
const MESSAGE_OVERHEAD_TOKENS: usize = 8;
const CONVERSATION_OVERHEAD_TOKENS: usize = 12;
const TOOL_SCHEMA_OVERHEAD_TOKENS: usize = 18;
const TOOL_USE_OVERHEAD_TOKENS: usize = 10;
const TOOL_RESULT_OVERHEAD_TOKENS: usize = 10;
const IMAGE_BASE_TOKENS: usize = 85;
const IMAGE_MAX_TOKENS: usize = 1_600;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCountStats {
    pub size: usize,
    pub hits: u64,
    pub misses: u64,
    pub max_entries: usize,
}

#[derive(Clone)]
pub struct TokenCountCache {
    tokenizer: Arc<CoreBPE>,
    state: Arc<Mutex<State>>,
}

struct State {
    cache: LruCache<[u8; 32], usize>,
    hits: u64,
    misses: u64,
}

impl TokenCountCache {
    pub fn new(max_entries: usize) -> Result<Self, String> {
        let capacity = NonZeroUsize::new(max_entries.max(1))
            .ok_or_else(|| "token cache capacity must be positive".to_owned())?;
        let tokenizer = cl100k_base().map_err(|error| error.to_string())?;
        Ok(Self {
            tokenizer: Arc::new(tokenizer),
            state: Arc::new(Mutex::new(State {
                cache: LruCache::new(capacity),
                hits: 0,
                misses: 0,
            })),
        })
    }

    pub async fn count(&self, text: String) -> Result<usize, String> {
        let cacheable = text.chars().count() >= MIN_CACHE_CHARS;
        let key = cacheable.then(|| Sha256::digest(text.as_bytes()).into());
        if let Some(key) = key {
            let mut state = lock(&self.state);
            if let Some(count) = state.cache.get(&key).copied() {
                state.hits += 1;
                return Ok(count);
            }
            state.misses += 1;
        }

        let tokenizer = Arc::clone(&self.tokenizer);
        let count = tokio::task::spawn_blocking(move || tokenizer.encode_ordinary(&text).len())
            .await
            .map_err(|error| error.to_string())?;
        if let Some(key) = key {
            lock(&self.state).cache.put(key, count);
        }
        Ok(count)
    }

    pub fn stats(&self) -> TokenCountStats {
        let state = lock(&self.state);
        TokenCountStats {
            size: state.cache.len(),
            hits: state.hits,
            misses: state.misses,
            max_entries: state.cache.cap().get(),
        }
    }

    /// Estimates the semantic Kiro request exactly as it will be sent, while
    /// excluding base64 blobs from the tokenizer and applying structural and
    /// image overhead compatible with the TypeScript proxy.
    pub async fn estimate_kiro_payload(&self, payload: &KiroPayload) -> Result<usize, String> {
        let mut segments = Vec::new();
        let mut fixed = CONVERSATION_OVERHEAD_TOKENS;
        for message in &payload.conversation_state.history {
            if let Some(user) = &message.user_input_message {
                collect_user_message(user, &mut segments, &mut fixed);
            }
            if let Some(assistant) = &message.assistant_response_message {
                collect_assistant_message(assistant, &mut segments, &mut fixed);
            }
        }
        collect_user_message(
            &payload
                .conversation_state
                .current_message
                .user_input_message,
            &mut segments,
            &mut fixed,
        );
        let mut total = fixed;
        for segment in segments {
            total = total.saturating_add(self.count(segment).await?);
        }
        Ok(total.max(1))
    }
}

fn collect_user_message(
    message: &KiroUserInputMessage,
    segments: &mut Vec<String>,
    fixed: &mut usize,
) {
    *fixed = fixed.saturating_add(MESSAGE_OVERHEAD_TOKENS);
    segments.push(message.content.clone());
    for image in &message.images {
        let encoded = image
            .source
            .bytes
            .split_once(',')
            .map_or(image.source.bytes.as_str(), |(_, bytes)| bytes);
        let estimated_bytes = encoded.len().saturating_mul(3) / 4;
        let image_tokens = IMAGE_BASE_TOKENS
            .saturating_add(((estimated_bytes as f64).sqrt() * 1.7).ceil() as usize)
            .clamp(IMAGE_BASE_TOKENS, IMAGE_MAX_TOKENS);
        *fixed = fixed.saturating_add(image_tokens);
    }
    if let Some(context) = &message.user_input_message_context {
        for tool in &context.tools {
            *fixed = fixed.saturating_add(TOOL_SCHEMA_OVERHEAD_TOKENS);
            segments.push(tool.tool_specification.name.clone());
            segments.push(tool.tool_specification.description.clone());
            segments.push(tool.tool_specification.input_schema.json.to_string());
        }
        for result in &context.tool_results {
            *fixed = fixed.saturating_add(TOOL_RESULT_OVERHEAD_TOKENS);
            segments.push(result.tool_use_id.clone());
            segments.push(result.status.clone());
            segments.push(
                result
                    .content
                    .iter()
                    .map(|content| content.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }
}

fn collect_assistant_message(
    message: &KiroAssistantMessage,
    segments: &mut Vec<String>,
    fixed: &mut usize,
) {
    *fixed = fixed.saturating_add(MESSAGE_OVERHEAD_TOKENS);
    segments.push(message.content.clone());
    for tool in &message.tool_uses {
        *fixed = fixed.saturating_add(TOOL_USE_OVERHEAD_TOKENS);
        segments.push(tool.name.clone());
        segments.push(tool.input.to_string());
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cache_is_exact_bounded_and_skips_short_text() {
        let cache = TokenCountCache::new(2).expect("tokenizer");
        let large = "literal <|endoftext|> content ".repeat(100);
        let first = cache.count(large.clone()).await.expect("count");
        let second = cache.count(large).await.expect("count");
        assert_eq!(first, second);
        assert_eq!(cache.stats().hits, 1);
        cache.count("short".into()).await.expect("short");
        assert_eq!(cache.stats().size, 1);
        for index in 0..4 {
            cache
                .count(format!("marker-{index} ").repeat(100))
                .await
                .expect("count");
        }
        assert!(cache.stats().size <= 2);
    }

    #[tokio::test]
    async fn kiro_estimate_counts_structure_tools_and_images_without_tokenizing_base64() {
        let cache = TokenCountCache::new(8).expect("tokenizer");
        let payload: KiroPayload = serde_json::from_value(serde_json::json!({
            "conversationState":{
                "chatTriggerType":"MANUAL",
                "conversationId":"conversation",
                "history":[],
                "currentMessage":{"userInputMessage":{
                    "content":"hello",
                    "modelId":"model",
                    "origin":"CLI",
                    "images":[{"format":"png","source":{"bytes":"a".repeat(40_000)}}],
                    "userInputMessageContext":{"toolResults":[],"tools":[{
                        "toolSpecification":{"name":"read_file","description":"Read a file",
                        "inputSchema":{"json":{"type":"object","properties":{"path":{"type":"string"}}}}}
                    }]}
                }}
            }
        }))
        .expect("payload");
        let estimate = cache
            .estimate_kiro_payload(&payload)
            .await
            .expect("estimate");
        assert!(estimate > CONVERSATION_OVERHEAD_TOKENS + IMAGE_BASE_TOKENS);
        assert!(
            estimate < 5_000,
            "base64 should not be tokenized: {estimate}"
        );
    }
}

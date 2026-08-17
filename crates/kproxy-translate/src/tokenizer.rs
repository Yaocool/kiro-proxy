//! Async tokenizer with a bounded content-hash LRU.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use lru::LruCache;
use sha2::{Digest, Sha256};
use tiktoken_rs::{cl100k_base, CoreBPE};

use crate::{
    ClaudeTool, KiroAssistantMessage, KiroHistoryMessage, KiroPayload, KiroUserInputMessage,
};

const MIN_CACHE_CHARS: usize = 512;
const MESSAGE_OVERHEAD_TOKENS: usize = 8;
const CONVERSATION_OVERHEAD_TOKENS: usize = 12;
const TOOL_SCHEMA_OVERHEAD_TOKENS: usize = 18;
const TOOL_USE_OVERHEAD_TOKENS: usize = 10;
const TOOL_RESULT_OVERHEAD_TOKENS: usize = 10;
const IMAGE_BASE_TOKENS: usize = 85;
const IMAGE_MAX_TOKENS: usize = 4_096;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCountStats {
    pub size: usize,
    pub hits: u64,
    pub misses: u64,
    pub max_entries: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextCompactionStats {
    pub original_tokens: usize,
    pub compacted_tokens: usize,
    pub removed_messages: usize,
    pub summary: Option<String>,
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

    /// Estimates only loaded tool definitions as serialized in the Kiro
    /// request. Deferred Claude tools are intentionally absent from this value.
    pub async fn estimate_kiro_tools(&self, payload: &KiroPayload) -> Result<usize, String> {
        let mut segments = Vec::new();
        let mut fixed = 0usize;
        for message in &payload.conversation_state.history {
            if let Some(user) = &message.user_input_message {
                collect_tools(user, &mut segments, &mut fixed);
            }
        }
        collect_tools(
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
        Ok(total)
    }

    /// Estimates full Claude definitions before long documentation is moved
    /// into the Kiro system prompt and descriptions are replaced by markers.
    pub async fn estimate_claude_tools(&self, tools: &[&ClaudeTool]) -> Result<usize, String> {
        let mut total = tools.len().saturating_mul(TOOL_SCHEMA_OVERHEAD_TOKENS);
        for tool in tools {
            total = total.saturating_add(self.count(tool.name.clone()).await?);
            total = total.saturating_add(self.count(tool.description.clone()).await?);
            total = total.saturating_add(self.count(tool.input_schema.to_string()).await?);
            if let Some(examples) = &tool.input_examples {
                total = total.saturating_add(
                    self.count(serde_json::to_string(examples).unwrap_or_default())
                        .await?,
                );
            }
        }
        Ok(total)
    }

    /// Replaces complete oldest conversation turns with a bounded extractive
    /// summary until the translated Kiro payload fits the requested budget.
    /// The current turn (including system instructions and tool definitions)
    /// is never modified.
    pub async fn compact_kiro_payload(
        &self,
        payload: &mut KiroPayload,
        target_tokens: usize,
    ) -> Result<ContextCompactionStats, String> {
        let original_tokens = self.estimate_kiro_payload(payload).await?;
        let original_history = payload.conversation_state.history.clone();
        let mut compacted_tokens = original_tokens;
        let mut removed_messages = 0;
        let mut compaction_summary = None;
        let summary_token_budget = (target_tokens / 8).clamp(256, 8_192);
        let history_target = target_tokens.saturating_sub(summary_token_budget);
        let mut removed = Vec::new();
        while compacted_tokens > history_target && !payload.conversation_state.history.is_empty() {
            let history = &mut payload.conversation_state.history;
            let remove = if history[0].user_input_message.is_some()
                && history
                    .get(1)
                    .is_some_and(|message| message.assistant_response_message.is_some())
            {
                2
            } else {
                1
            };
            let removed_now = history
                .drain(..remove.min(history.len()))
                .collect::<Vec<_>>();
            removed_messages += removed_now.len();
            removed.extend(removed_now);
            // Kiro expects history to begin on a user turn. Remove an orphaned
            // assistant message if malformed client history left one behind.
            while history
                .first()
                .is_some_and(|message| message.assistant_response_message.is_some())
            {
                removed.push(history.remove(0));
                removed_messages += 1;
            }
            compacted_tokens = self.estimate_kiro_payload(payload).await?;
        }
        if !removed.is_empty() {
            let current = &payload
                .conversation_state
                .current_message
                .user_input_message;
            let model_id = current.model_id.clone();
            let origin = current.origin.clone();
            let mut summary_char_budget = summary_token_budget.saturating_mul(3).max(256);
            loop {
                let summary = render_compaction_summary(&removed, summary_char_budget);
                let pair = compaction_summary_pair(summary, &model_id, &origin);
                payload.conversation_state.history.splice(0..0, pair);
                compacted_tokens = self.estimate_kiro_payload(payload).await?;
                if compacted_tokens <= target_tokens || summary_char_budget <= 256 {
                    compaction_summary = Some(render_compaction_summary(
                        &original_history,
                        summary_char_budget,
                    ));
                    break;
                }
                payload.conversation_state.history.drain(..2);
                summary_char_budget = summary_char_budget.saturating_mul(3) / 4;
            }
        }
        Ok(ContextCompactionStats {
            original_tokens,
            compacted_tokens,
            removed_messages,
            summary: compaction_summary,
        })
    }
}

fn compaction_summary_pair(
    summary: String,
    model_id: &str,
    origin: &str,
) -> [KiroHistoryMessage; 2] {
    [
        KiroHistoryMessage {
            user_input_message: Some(KiroUserInputMessage {
                content: summary,
                model_id: model_id.to_owned(),
                origin: origin.to_owned(),
                images: Vec::new(),
                user_input_message_context: None,
            }),
            assistant_response_message: None,
        },
        KiroHistoryMessage {
            user_input_message: None,
            assistant_response_message: Some(KiroAssistantMessage {
                content: "I will preserve and use the compacted conversation context above.".into(),
                tool_uses: Vec::new(),
            }),
        },
    ]
}

fn render_compaction_summary(messages: &[KiroHistoryMessage], char_budget: usize) -> String {
    const HEADER: &str = "[Earlier conversation compacted by kproxy]\n";
    const FOOTER: &str = "\n[End of compacted conversation]";
    let available = char_budget.saturating_sub(HEADER.chars().count() + FOOTER.chars().count());
    let mut remaining = available;
    let mut sections = Vec::new();
    // Prefer the most recent removed turns when the summary budget is full.
    for message in messages.iter().rev() {
        if remaining == 0 {
            break;
        }
        let rendered = render_history_message(message);
        if rendered.is_empty() {
            continue;
        }
        let excerpt = compact_excerpt(&rendered, remaining.min(1_600));
        remaining = remaining.saturating_sub(excerpt.chars().count() + 2);
        sections.push(excerpt);
    }
    sections.reverse();
    format!("{HEADER}{}{FOOTER}", sections.join("\n\n"))
}

fn render_history_message(message: &KiroHistoryMessage) -> String {
    if let Some(user) = &message.user_input_message {
        let mut parts = vec![format!(
            "User: {}",
            normalize_compaction_text(&user.content)
        )];
        if !user.images.is_empty() {
            parts.push(format!("[{} image(s)]", user.images.len()));
        }
        if let Some(context) = &user.user_input_message_context {
            for result in &context.tool_results {
                let text = result
                    .content
                    .iter()
                    .map(|content| content.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                parts.push(format!(
                    "Tool result {} ({}): {}",
                    result.tool_use_id,
                    result.status,
                    normalize_compaction_text(&text)
                ));
            }
        }
        return parts.join("\n");
    }
    if let Some(assistant) = &message.assistant_response_message {
        let mut parts = vec![format!(
            "Assistant: {}",
            normalize_compaction_text(&assistant.content)
        )];
        for tool in &assistant.tool_uses {
            parts.push(format!(
                "Tool call {} {}: {}",
                tool.tool_use_id,
                tool.name,
                normalize_compaction_text(&tool.input.to_string())
            ));
        }
        return parts.join("\n");
    }
    String::new()
}

fn normalize_compaction_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compact_excerpt(value: &str, maximum: usize) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() <= maximum {
        return value.to_owned();
    }
    if maximum < 32 {
        return characters.into_iter().take(maximum).collect();
    }
    let marker = " … [compressed] … ";
    let content = maximum.saturating_sub(marker.chars().count());
    let head = content.saturating_mul(2) / 3;
    let tail = content.saturating_sub(head);
    format!(
        "{}{}{}",
        characters[..head].iter().collect::<String>(),
        marker,
        characters[characters.len() - tail..]
            .iter()
            .collect::<String>()
    )
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
            // Kiro does not expose an image token-count endpoint. Use a
            // deliberately conservative size proxy so compressed, high-detail
            // images do not get treated like a few dozen text tokens.
            .saturating_add(((estimated_bytes as f64).sqrt() * 2.5).ceil() as usize)
            .clamp(IMAGE_BASE_TOKENS, IMAGE_MAX_TOKENS);
        *fixed = fixed.saturating_add(image_tokens);
    }
    if let Some(context) = &message.user_input_message_context {
        collect_tools(message, segments, fixed);
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

fn collect_tools(message: &KiroUserInputMessage, segments: &mut Vec<String>, fixed: &mut usize) {
    let Some(context) = &message.user_input_message_context else {
        return;
    };
    for tool in &context.tools {
        *fixed = fixed.saturating_add(TOOL_SCHEMA_OVERHEAD_TOKENS);
        segments.push(tool.tool_specification.name.clone());
        segments.push(tool.tool_specification.description.clone());
        segments.push(tool.tool_specification.input_schema.json.to_string());
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
        segments.push(tool.tool_use_id.clone());
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

    #[tokio::test]
    async fn kiro_estimate_counts_assistant_tool_use_ids() {
        let cache = TokenCountCache::new(8).expect("tokenizer");
        let payload: KiroPayload = serde_json::from_value(serde_json::json!({
            "conversationState":{
                "chatTriggerType":"MANUAL",
                "conversationId":"conversation",
                "history":[{
                    "assistantResponseMessage":{
                        "content":"calling tool",
                        "toolUses":[{"toolUseId":"short","name":"lookup","input":{"q":"x"}}]
                    }
                }],
                "currentMessage":{"userInputMessage":{
                    "content":"continue","modelId":"model","origin":"CLI","images":[]
                }}
            }
        }))
        .expect("payload");
        let short = cache.estimate_kiro_payload(&payload).await.expect("short");
        let mut expanded = payload;
        expanded.conversation_state.history[0]
            .assistant_response_message
            .as_mut()
            .expect("assistant")
            .tool_uses[0]
            .tool_use_id = "long-tool-use-id-".repeat(1_000);
        let long = cache.estimate_kiro_payload(&expanded).await.expect("long");
        assert!(long > short + 1_000);
    }

    #[tokio::test]
    async fn compaction_removes_old_turns_but_preserves_current_tools() {
        let cache = TokenCountCache::new(8).expect("tokenizer");
        let mut payload: KiroPayload = serde_json::from_value(serde_json::json!({
            "conversationState":{
                "chatTriggerType":"MANUAL",
                "conversationId":"conversation",
                "history":[
                    {"userInputMessage":{"content":"old user ".repeat(4000),"modelId":"model","origin":"CLI","images":[]}},
                    {"assistantResponseMessage":{"content":"old answer ".repeat(4000)}},
                    {"userInputMessage":{"content":"recent user","modelId":"model","origin":"CLI","images":[]}},
                    {"assistantResponseMessage":{"content":"recent answer"}}
                ],
                "currentMessage":{"userInputMessage":{
                    "content":"system and current request","modelId":"model","origin":"CLI","images":[],
                    "userInputMessageContext":{"toolResults":[],"tools":[{
                        "toolSpecification":{"name":"read_file","description":"Read a file","inputSchema":{"json":{"type":"object"}}}
                    }]}
                }}
            }
        }))
        .expect("payload");
        let original = cache.estimate_kiro_payload(&payload).await.expect("count");
        let result = cache
            .compact_kiro_payload(&mut payload, original / 2)
            .await
            .expect("compact");
        assert!(result.removed_messages >= 2);
        assert!(result.compacted_tokens < result.original_tokens);
        assert!(payload.conversation_state.history[0]
            .user_input_message
            .as_ref()
            .expect("compaction summary")
            .content
            .contains("Earlier conversation compacted"));
        assert_eq!(
            payload
                .conversation_state
                .current_message
                .user_input_message
                .user_input_message_context
                .expect("context")
                .tools
                .len(),
            1
        );
    }
}

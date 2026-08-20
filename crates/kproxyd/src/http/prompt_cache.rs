//! Per-account prompt-cache usage simulation for compatible cache-control blocks.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kproxy_kiro::UsageInfo;
use kproxy_translate::{ClaudeRequest, OpenAiRequest, TokenCountCache};
use serde_json::Value;
use sha2::{Digest, Sha256};

const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);
const ONE_HOUR_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_CACHE_RATIO: f64 = 0.85;
const MAX_ENTRIES_PER_ACCOUNT: usize = 200;

#[derive(Debug, Clone)]
pub struct PromptCacheProfile {
    fingerprint: String,
    tokens: u64,
    ttl: Duration,
}

#[derive(Debug, Clone)]
struct Entry {
    expires_at: Instant,
    ttl: Duration,
}

#[derive(Debug, Default)]
pub struct PromptCacheTracker {
    entries: Mutex<HashMap<String, HashMap<String, Entry>>>,
}

impl PromptCacheTracker {
    pub fn claude_profile(
        &self,
        request: &ClaudeRequest,
        total_tokens: u64,
    ) -> Option<PromptCacheProfile> {
        let mut blocks = Vec::new();
        append_value(&mut blocks, request.system.as_ref());
        for tool in request.tools.iter().filter(|tool| !tool.defer_loading) {
            append_owned(&mut blocks, serde_json::to_value(tool).ok());
        }
        for message in &request.messages {
            append_value(&mut blocks, Some(&message.content));
        }
        build_profile(blocks, total_tokens, &request.model)
    }

    /// Builds a conservative profile after proxy-side compaction. Historical
    /// message breakpoints no longer describe the payload sent upstream, but
    /// an explicit system breakpoint remains a stable independent prefix.
    pub async fn claude_compacted_profile(
        &self,
        tokenizer: &TokenCountCache,
        request: &ClaudeRequest,
        total_tokens: u64,
    ) -> Option<PromptCacheProfile> {
        let mut blocks = Vec::new();
        append_value(&mut blocks, request.system.as_ref());
        let last_breakpoint = blocks.iter().rposition(|block| block.ttl.is_some())?;
        let cacheable_text = blocks[..=last_breakpoint]
            .iter()
            .map(|block| stable_json(&block.value))
            .collect::<Vec<_>>()
            .join("\n");
        let cacheable_tokens = tokenizer.count(cacheable_text).await.ok()? as u64;
        build_profile_with_tokens(blocks, total_tokens, cacheable_tokens, &request.model)
    }

    pub fn openai_profile(
        &self,
        request: &OpenAiRequest,
        total_tokens: u64,
    ) -> Option<PromptCacheProfile> {
        let mut blocks = Vec::new();
        for tool in &request.tools {
            append_owned(&mut blocks, serde_json::to_value(tool).ok());
        }
        for message in &request.messages {
            append_value(&mut blocks, message.content.as_ref());
        }
        build_profile(blocks, total_tokens, &request.model)
    }

    pub fn apply(
        &self,
        account_id: &str,
        profile: Option<&PromptCacheProfile>,
        usage: &mut UsageInfo,
    ) {
        let Some(profile) = profile else {
            return;
        };
        if usage.cache_read_tokens > 0 || usage.cache_write_tokens > 0 {
            return;
        }
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.retain(|_, account| {
            account.retain(|_, entry| entry.expires_at > now);
            !account.is_empty()
        });
        let account = entries.entry(account_id.into()).or_default();
        if let Some(entry) = account.get_mut(&profile.fingerprint) {
            entry.expires_at = now + entry.ttl;
            usage.cache_read_tokens = profile.tokens;
        } else {
            usage.cache_write_tokens = profile.tokens;
            account.insert(
                profile.fingerprint.clone(),
                Entry {
                    expires_at: now + profile.ttl,
                    ttl: profile.ttl,
                },
            );
            if account.len() > MAX_ENTRIES_PER_ACCOUNT {
                if let Some(oldest) = account
                    .iter()
                    .min_by_key(|(_, entry)| entry.expires_at)
                    .map(|(fingerprint, _)| fingerprint.clone())
                {
                    account.remove(&oldest);
                }
            }
        }
    }
}

#[derive(Debug)]
struct CacheBlock {
    value: Value,
    ttl: Option<Duration>,
}

fn append_owned(output: &mut Vec<CacheBlock>, value: Option<Value>) {
    if let Some(value) = value {
        append_value(output, Some(&value));
    }
}

fn append_value(output: &mut Vec<CacheBlock>, value: Option<&Value>) {
    let Some(value) = value else {
        return;
    };
    match value {
        Value::Array(items) => {
            for item in items {
                append_value(output, Some(item));
            }
        }
        _ => output.push(CacheBlock {
            value: value.clone(),
            ttl: cache_ttl(value),
        }),
    }
}

fn build_profile(
    blocks: Vec<CacheBlock>,
    total_tokens: u64,
    model: &str,
) -> Option<PromptCacheProfile> {
    let last_breakpoint = blocks.iter().rposition(|block| block.ttl.is_some())?;
    let cacheable = &blocks[..=last_breakpoint];
    let cacheable_chars = cacheable
        .iter()
        .map(|block| block.value.to_string().len())
        .sum::<usize>();
    let all_chars = blocks
        .iter()
        .map(|block| block.value.to_string().len())
        .sum::<usize>()
        .max(1);
    let estimated = ((total_tokens as f64) * cacheable_chars as f64 / all_chars as f64) as u64;
    build_profile_with_tokens(blocks, total_tokens, estimated, model)
}

fn build_profile_with_tokens(
    blocks: Vec<CacheBlock>,
    total_tokens: u64,
    cacheable_tokens: u64,
    model: &str,
) -> Option<PromptCacheProfile> {
    let last_breakpoint = blocks.iter().rposition(|block| block.ttl.is_some())?;
    let cacheable = &blocks[..=last_breakpoint];
    let tokens = cacheable_tokens.min((total_tokens as f64 * MAX_CACHE_RATIO) as u64);
    let minimum = if model.to_ascii_lowercase().contains("opus") {
        4096
    } else {
        1024
    };
    if tokens < minimum {
        return None;
    }
    let mut hasher = Sha256::new();
    for block in cacheable {
        let encoded = stable_json(&block.value);
        hasher.update(encoded.len().to_le_bytes());
        hasher.update(encoded.as_bytes());
    }
    let fingerprint = hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        });
    Some(PromptCacheProfile {
        fingerprint,
        tokens,
        ttl: cacheable
            .iter()
            .filter_map(|block| block.ttl)
            .next_back()
            .unwrap_or(DEFAULT_TTL),
    })
}

fn cache_ttl(value: &Value) -> Option<Duration> {
    let control = value.get("cache_control")?;
    if !control
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("ephemeral"))
    {
        return None;
    }
    match control.get("ttl") {
        Some(Value::String(value)) if value.eq_ignore_ascii_case("1h") => Some(ONE_HOUR_TTL),
        Some(Value::Number(value)) => value
            .as_u64()
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs),
        _ => Some(DEFAULT_TTL),
    }
}

fn stable_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            let values = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        stable_json(&map[key])
                    )
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", values.join(","))
        }
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(stable_json).collect::<Vec<_>>().join(",")
        ),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repeated_prefix_is_a_read_and_accounts_are_isolated() {
        let tracker = PromptCacheTracker::default();
        let request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4","max_tokens":128,
            "system":[{"type":"text","text":"cacheable context ".repeat(1500),
                "cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":"summarize"}]
        }))
        .expect("request");
        let profile = tracker
            .claude_profile(&request, 8_000)
            .expect("cache profile");
        let mut first = UsageInfo::default();
        tracker.apply("one", Some(&profile), &mut first);
        assert!(first.cache_write_tokens >= 1024);
        let mut second = UsageInfo::default();
        tracker.apply("one", Some(&profile), &mut second);
        assert_eq!(second.cache_read_tokens, first.cache_write_tokens);
        let mut other = UsageInfo::default();
        tracker.apply("two", Some(&profile), &mut other);
        assert_eq!(other.cache_write_tokens, first.cache_write_tokens);
    }

    #[test]
    fn deferred_tools_do_not_change_prompt_cache_prefix() {
        let tracker = PromptCacheTracker::default();
        let base = json!({
            "model":"claude-sonnet-4-5","max_tokens":128,
            "tools":[{
                "name":"Read","description":"cacheable tool ".repeat(1000),
                "input_schema":{"type":"object"},
                "cache_control":{"type":"ephemeral"}
            }],
            "messages":[{"role":"user","content":"summarize"}]
        });
        let first: ClaudeRequest = serde_json::from_value(base.clone()).expect("request");
        let mut second: ClaudeRequest = serde_json::from_value(base).expect("request");
        second.tools.push(
            serde_json::from_value(json!({
                "name":"mcp__large__schema",
                "description":"different deferred catalog",
                "input_schema":{"type":"object"},
                "defer_loading":true
            }))
            .expect("tool"),
        );
        let first = tracker.claude_profile(&first, 8_000).expect("profile");
        let second = tracker.claude_profile(&second, 8_000).expect("profile");
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.tokens, second.tokens);
    }

    #[tokio::test]
    async fn compacted_profiles_keep_only_the_explicit_system_breakpoint() {
        let tracker = PromptCacheTracker::default();
        let tokenizer = TokenCountCache::new(32).expect("tokenizer");
        let request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4","max_tokens":128,
            "system":[{"type":"text","text":"stable system ".repeat(1500),
                "cache_control":{"type":"ephemeral"}}],
            "messages":[
                {"role":"user","content":[{"type":"text","text":"stale history ".repeat(2000),
                    "cache_control":{"type":"ephemeral"}}]},
                {"role":"assistant","content":"old response"},
                {"role":"user","content":"continue"}
            ]
        }))
        .expect("request");
        let compacted = tracker
            .claude_compacted_profile(&tokenizer, &request, 8_000)
            .await
            .expect("system cache profile");
        let system_only: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4","max_tokens":128,
            "system":[{"type":"text","text":"stable system ".repeat(1500),
                "cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":"different future request"}]
        }))
        .expect("request");
        let expected = tracker
            .claude_compacted_profile(&tokenizer, &system_only, 8_000)
            .await
            .expect("system cache profile");
        assert_eq!(compacted.fingerprint, expected.fingerprint);
        assert_eq!(compacted.tokens, expected.tokens);
        assert!(compacted.tokens < 6_800);
    }

    #[tokio::test]
    async fn compacted_profile_does_not_invent_a_large_cache_for_a_small_system() {
        let tracker = PromptCacheTracker::default();
        let tokenizer = TokenCountCache::new(32).expect("tokenizer");
        let request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4","max_tokens":128,
            "system":[{"type":"text","text":"small stable prefix",
                "cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":"large compacted checkpoint"}]
        }))
        .expect("request");

        assert!(tracker
            .claude_compacted_profile(&tokenizer, &request, 50_000)
            .await
            .is_none());
    }
}

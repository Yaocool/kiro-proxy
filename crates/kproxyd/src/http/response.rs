use std::collections::BTreeMap;

use kproxy_kiro::{KiroEvent, UsageInfo};
use kproxy_translate::ClaudeToolSearchTrace;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiToolIdentity {
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct DecodedResponse {
    pub text: String,
    pub reasoning: String,
    pub tools: BTreeMap<String, ToolBuffer>,
    pub tool_searches: Vec<ClaudeToolSearchTrace>,
    pub usage: UsageInfo,
}

#[derive(Debug, Default)]
pub struct ToolBuffer {
    pub id: String,
    pub name: String,
    pub input: String,
    pub complete: bool,
}

const MAX_CUMULATIVE_TOOL_INPUT_BYTES: usize = 16 * 1024 * 1024;

/// Holds split XML-like tool calls until they can be recovered or proven to be text.
#[derive(Debug)]
pub struct ToolLeakFilter {
    enabled: bool,
    pending: String,
    recovered: Vec<KiroEvent>,
    structured: BTreeMap<String, (String, String)>,
}

impl ToolLeakFilter {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pending: String::new(),
            recovered: Vec::new(),
            structured: BTreeMap::new(),
        }
    }

    pub fn push(&mut self, event: KiroEvent) -> Vec<KiroEvent> {
        if !self.enabled {
            return vec![event];
        }
        match event {
            KiroEvent::AssistantResponse { content } => self.push_text(content),
            KiroEvent::ToolUse {
                id,
                name,
                input_delta,
                stop,
            } => {
                let buffer = self
                    .structured
                    .entry(id.clone())
                    .or_insert_with(|| (name.clone(), String::new()));
                if buffer.0.is_empty() {
                    buffer.0 = name.clone();
                }
                buffer.1.push_str(&input_delta);
                vec![KiroEvent::ToolUse {
                    id,
                    name,
                    input_delta,
                    stop,
                }]
            }
            other => vec![other],
        }
    }

    pub fn finish(mut self) -> Vec<KiroEvent> {
        let mut output = Vec::new();
        if !self.pending.is_empty() {
            output.push(KiroEvent::AssistantResponse {
                content: std::mem::take(&mut self.pending),
            });
        }
        output.extend(self.recovered.into_iter().filter(|event| match event {
            KiroEvent::ToolUse {
                name, input_delta, ..
            } => {
                let input = repair_json(input_delta);
                !self.structured.values().any(|(seen_name, seen_input)| {
                    seen_name == name && repair_json(seen_input) == input
                })
            }
            _ => true,
        }));
        output
    }

    fn push_text(&mut self, content: String) -> Vec<KiroEvent> {
        self.pending.push_str(&content);
        if self.pending.len() > 1024 * 1024 {
            return vec![KiroEvent::AssistantResponse {
                content: std::mem::take(&mut self.pending),
            }];
        }
        let mut visible = String::new();
        loop {
            let Some((open, marker)) = first_marker(&self.pending) else {
                let keep = marker_prefix_suffix(&self.pending);
                let split = self.pending.len().saturating_sub(keep);
                visible.push_str(&self.pending[..split]);
                self.pending = self.pending[split..].into();
                break;
            };
            visible.push_str(&self.pending[..open]);
            let close = if marker == "<function_calls" {
                "</function_calls>"
            } else {
                "</tool_use>"
            };
            let lower = self.pending.to_ascii_lowercase();
            let Some(relative) = lower[open..].find(close) else {
                self.pending = self.pending[open..].into();
                break;
            };
            let end = open + relative + close.len();
            let xml = self.pending[open..end].to_string();
            self.recover(&xml);
            self.pending = self.pending[end..].into();
        }
        if visible.is_empty() {
            Vec::new()
        } else {
            vec![KiroEvent::AssistantResponse { content: visible }]
        }
    }

    fn recover(&mut self, xml: &str) {
        let mut cursor = 0;
        while let Some(open) = find_insensitive_from(xml, "<invoke", cursor) {
            let Some(tag_end) = xml[open..].find('>').map(|index| open + index) else {
                break;
            };
            let Some(close) = find_insensitive_from(xml, "</invoke>", tag_end) else {
                break;
            };
            if let Some(name) = attribute(&xml[open..=tag_end], "name") {
                let input = parse_parameters(&xml[tag_end + 1..close]);
                self.recovered.push(KiroEvent::ToolUse {
                    id: format!("recovered_tool_{}", self.recovered.len() + 1),
                    name,
                    input_delta: input.to_string(),
                    stop: true,
                });
            }
            cursor = close + "</invoke>".len();
        }
        if cursor > 0 {
            return;
        }
        let Some(open) = find_insensitive_from(xml, "<tool_use", 0) else {
            return;
        };
        let Some(tag_end) = xml[open..].find('>').map(|index| open + index) else {
            return;
        };
        let Some(close) = find_insensitive_from(xml, "</tool_use>", tag_end) else {
            return;
        };
        let tag = &xml[open..=tag_end];
        let Some(name) = attribute(tag, "name")
            .or_else(|| attribute(tag, "tool"))
            .or_else(|| attribute(tag, "function"))
        else {
            return;
        };
        let body = decode_xml(xml[tag_end + 1..close].trim());
        let input = serde_json::from_str::<Value>(&body).unwrap_or_else(|_| json!({"value":body}));
        self.recovered.push(KiroEvent::ToolUse {
            id: attribute(tag, "id")
                .unwrap_or_else(|| format!("recovered_tool_{}", self.recovered.len() + 1)),
            name,
            input_delta: input.to_string(),
            stop: true,
        });
    }
}

fn first_marker(text: &str) -> Option<(usize, &'static str)> {
    let lower = text.to_ascii_lowercase();
    ["<function_calls", "<tool_use"]
        .into_iter()
        .filter_map(|marker| lower.find(marker).map(|index| (index, marker)))
        .min_by_key(|(index, _)| *index)
}

fn marker_prefix_suffix(text: &str) -> usize {
    let lower = text.to_ascii_lowercase();
    let mut longest = 0;
    for marker in ["<function_calls", "<tool_use"] {
        for length in 1..marker.len() {
            if lower.ends_with(&marker[..length]) {
                longest = longest.max(length);
            }
        }
    }
    longest.min(text.len())
}

fn find_insensitive_from(text: &str, needle: &str, from: usize) -> Option<usize> {
    text.get(from..)?
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
        .map(|index| from + index)
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(index) = lower[cursor..].find(name) {
        let index = cursor + index;
        let before_ok = index == 0 || !lower.as_bytes()[index - 1].is_ascii_alphanumeric();
        let mut equals = index + name.len();
        while lower
            .as_bytes()
            .get(equals)
            .is_some_and(u8::is_ascii_whitespace)
        {
            equals += 1;
        }
        if before_ok && lower.as_bytes().get(equals) == Some(&b'=') {
            equals += 1;
            while lower
                .as_bytes()
                .get(equals)
                .is_some_and(u8::is_ascii_whitespace)
            {
                equals += 1;
            }
            let quote = *tag.as_bytes().get(equals)?;
            if quote == b'\'' || quote == b'"' {
                let start = equals + 1;
                let end = tag[start..].find(quote as char)? + start;
                return Some(decode_xml(&tag[start..end]));
            }
        }
        cursor = index + name.len();
    }
    None
}

fn parse_parameters(xml: &str) -> Value {
    let mut map = serde_json::Map::new();
    let mut cursor = 0;
    while let Some(open) = find_insensitive_from(xml, "<parameter", cursor) {
        let Some(tag_end) = xml[open..].find('>').map(|index| open + index) else {
            break;
        };
        let Some(close) = find_insensitive_from(xml, "</parameter>", tag_end) else {
            break;
        };
        if let Some(name) = attribute(&xml[open..=tag_end], "name") {
            let value = decode_xml(xml[tag_end + 1..close].trim());
            map.insert(name, coerce(&value));
        }
        cursor = close + "</parameter>".len();
    }
    Value::Object(map)
}

fn coerce(value: &str) -> Value {
    match value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.into())),
    }
}

fn decode_xml(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

impl DecodedResponse {
    pub fn restore_tool_names(&mut self, names: &std::collections::HashMap<String, String>) {
        for tool in self.tools.values_mut() {
            if let Some(original) = names.get(&tool.name) {
                tool.name.clone_from(original);
            }
        }
    }

    pub fn push(&mut self, event: KiroEvent) -> Result<(), String> {
        match event {
            KiroEvent::AssistantResponse { content } => {
                self.text.push_str(&sanitize_text(&content))
            }
            KiroEvent::Reasoning { content } => self.reasoning.push_str(&content),
            KiroEvent::ToolUse {
                id,
                name,
                input_delta,
                stop,
            } => {
                let current_bytes = self
                    .tools
                    .values()
                    .map(|tool| tool.input.len())
                    .sum::<usize>();
                if current_bytes.saturating_add(input_delta.len()) > MAX_CUMULATIVE_TOOL_INPUT_BYTES
                {
                    return Err("tool input exceeded the 16 MiB response limit".into());
                }
                let tool = self.tools.entry(id.clone()).or_insert_with(|| ToolBuffer {
                    id,
                    name: name.clone(),
                    input: String::new(),
                    complete: false,
                });
                if tool.name.is_empty() {
                    tool.name = name;
                }
                tool.input.push_str(&input_delta);
                tool.complete |= stop;
            }
            KiroEvent::MessageMetadata { usage } | KiroEvent::Usage { usage } => {
                merge_usage(&mut self.usage, &usage)
            }
            KiroEvent::Error { message, .. } => return Err(message),
            KiroEvent::Other { .. } => {}
        }
        Ok(())
    }

    pub fn validate_tool_inputs(&self) -> Result<(), String> {
        for tool in self.tools.values() {
            let parsed = serde_json::from_str::<Value>(tool.input.trim());
            if parsed.is_err() && (!tool.complete && is_write_tool(&tool.name)) {
                return Err(format!(
                    "upstream ended before write tool {} produced complete JSON input",
                    tool.name
                ));
            }
            if parsed.is_err() && repair_json(&tool.input).get("raw").is_some() {
                return Err(format!("tool {} produced invalid JSON input", tool.name));
            }
        }
        Ok(())
    }

    pub fn claude_json(
        &self,
        id: &str,
        model: &str,
        max_tokens: u32,
        current_round_output_tokens: u64,
        compaction_summary: Option<&str>,
    ) -> Value {
        let mut content = Vec::new();
        if let Some(summary) = compaction_summary {
            content.push(json!({"type":"compaction","content":summary}));
        }
        if !self.reasoning.is_empty() {
            content.push(json!({
                "type":"thinking",
                "thinking":self.reasoning,
                "signature":kproxy_translate::SIGNATURE_PLACEHOLDER
            }));
        }
        if !self.text.is_empty() {
            content.push(json!({"type":"text","text":self.text}));
        }
        for search in &self.tool_searches {
            content.push(json!({
                "type":"server_tool_use",
                "id":search.id,
                "name":search.name,
                "input":search.input
            }));
            let result = if let Some(error) = &search.error {
                json!({
                    "type":"tool_search_tool_result_error",
                    "error_code":error.code,
                    "error_message":error.message
                })
            } else {
                json!({
                    "type":"tool_search_tool_search_result",
                    "tool_references":search.references.iter().map(|name| json!({
                        "type":"tool_reference","tool_name":name
                    })).collect::<Vec<_>>()
                })
            };
            content.push(json!({
                "type":"tool_search_tool_result",
                "tool_use_id":search.id,
                "content":result
            }));
        }
        for tool in self.tools.values() {
            content.push(json!({
                "type":"tool_use",
                "id":tool.id,
                "name":tool.name,
                "input":repair_json(&tool.input)
            }));
        }
        let stop = if !self.tools.is_empty() {
            "tool_use"
        } else if current_round_output_tokens >= u64::from(max_tokens) {
            "max_tokens"
        } else {
            "end_turn"
        };
        let uncached_input_tokens = self
            .usage
            .input_tokens
            .saturating_sub(self.usage.cache_read_tokens)
            .saturating_sub(self.usage.cache_write_tokens);
        json!({
            "id":id,"type":"message","role":"assistant","content":content,
            "model":model,"stop_reason":stop,"stop_sequence":Value::Null,
            "usage":{
                "input_tokens":uncached_input_tokens,
                "output_tokens":self.usage.output_tokens,
                "cache_creation_input_tokens":self.usage.cache_write_tokens,
                "cache_read_input_tokens":self.usage.cache_read_tokens
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn openai_json(
        &self,
        id: &str,
        model: &str,
        created: i64,
        max_tokens: u32,
        current_round_output_tokens: u64,
        thinking_format: kproxy_core::config::ThinkingOutputFormat,
        tool_identities: &std::collections::HashMap<String, OpenAiToolIdentity>,
    ) -> Value {
        let tools = self
            .tools
            .values()
            .map(|tool| {
                let identity = tool_identities.get(&tool.name);
                let name = identity.map_or(tool.name.as_str(), |identity| identity.name.as_str());
                if identity.is_some_and(|identity| identity.kind == "custom") {
                    let input = repair_json(&tool.input);
                    let input = input
                        .get("input")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| input.to_string());
                    json!({"id":tool.id,"type":"custom","custom":{"name":name,"input":input}})
                } else {
                    json!({
                        "id":tool.id,"type":"function",
                        "function":{"name":name,"arguments":repair_json(&tool.input).to_string()}
                    })
                }
            })
            .collect::<Vec<_>>();
        let finish = if !tools.is_empty() {
            "tool_calls"
        } else if current_round_output_tokens >= u64::from(max_tokens) {
            "length"
        } else {
            "stop"
        };
        let content = match thinking_format {
            kproxy_core::config::ThinkingOutputFormat::Claude if !self.reasoning.is_empty() => {
                let mut tagged = format!("<thinking>{}</thinking>", self.reasoning);
                tagged.push_str(&self.text);
                json!(tagged)
            }
            _ if self.text.is_empty() => Value::Null,
            _ => json!(self.text),
        };
        let reasoning_content = match thinking_format {
            kproxy_core::config::ThinkingOutputFormat::Openai if !self.reasoning.is_empty() => {
                json!(self.reasoning)
            }
            _ => Value::Null,
        };
        json!({
            "id":id,"object":"chat.completion","created":created,"model":model,
            "choices":[{"index":0,"message":{
                "role":"assistant",
                "content":content,
                "reasoning_content":reasoning_content,
                "tool_calls":tools
            },"finish_reason":finish}],
            "usage":{
                "prompt_tokens":self.usage.input_tokens,
                "completion_tokens":self.usage.output_tokens,
                "total_tokens":self.usage.input_tokens+self.usage.output_tokens,
                "prompt_tokens_details":{"cached_tokens":self.usage.cache_read_tokens},
                "completion_tokens_details":{"reasoning_tokens":self.usage.reasoning_tokens}
            }
        })
    }
}

fn merge_usage(output: &mut UsageInfo, value: &Value) {
    let uncached = find_number(value, &["uncachedInputTokens"]);
    let total_input = find_number(value, &["inputTokens", "input_tokens"]);
    let cache_read = find_number(
        value,
        &[
            "cacheReadInputTokens",
            "cacheReadTokens",
            "cache_read_input_tokens",
            "cache_read_tokens",
        ],
    );
    let cache_write = find_number(
        value,
        &[
            "cacheWriteInputTokens",
            "cacheWriteTokens",
            "cache_creation_input_tokens",
            "cache_write_tokens",
        ],
    );
    let cache_tokens = cache_read
        .unwrap_or_default()
        .saturating_add(cache_write.unwrap_or_default());
    if let Some(uncached) = uncached {
        output.input_tokens = uncached.saturating_add(cache_tokens);
    } else if let Some(input) = total_input {
        // Kiro's generic inputTokens field is the total prompt size. Cache
        // counters describe subsets of it and must not be added again.
        output.input_tokens = input.max(cache_tokens);
    } else if cache_tokens > 0 {
        output.input_tokens = cache_tokens;
    }
    output.output_tokens =
        find_number(value, &["outputTokens", "output_tokens"]).unwrap_or(output.output_tokens);
    if output.input_tokens == 0 {
        if let Some(total) = find_number(value, &["totalTokens", "total_tokens"]) {
            output.input_tokens = total.saturating_sub(output.output_tokens);
        }
    }
    output.cache_read_tokens = cache_read.unwrap_or(output.cache_read_tokens);
    output.cache_write_tokens = cache_write.unwrap_or(output.cache_write_tokens);
    output.reasoning_tokens = find_number(value, &["reasoningTokens", "reasoning_tokens"])
        .unwrap_or(output.reasoning_tokens);
    if output.credits <= 0.0 {
        output.credits = find_float(
            value,
            &[
                "credits",
                "creditsConsumed",
                "creditsUsed",
                "creditUsage",
                "billedCredits",
                "usedCredits",
                "charge",
                "cost",
                "meteredUsage",
                "usage",
            ],
        )
        .filter(|credits| *credits > 0.0)
        .unwrap_or(output.credits);
    }
}

fn find_number(value: &Value, keys: &[&str]) -> Option<u64> {
    find_numeric_value(value, keys).and_then(|value| match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value as u64)
        }),
        Value::String(value) => value
            .parse::<f64>()
            .ok()
            .and_then(|value| (value.is_finite() && value >= 0.0).then_some(value as u64)),
        _ => None,
    })
}

fn find_float(value: &Value, keys: &[&str]) -> Option<f64> {
    find_numeric_value(value, keys).and_then(|value| match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    })
}

fn find_numeric_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    match value {
        Value::Object(map) => keys
            .iter()
            .filter_map(|key| map.get(*key))
            .find(|candidate| candidate.is_number() || candidate.is_string())
            .or_else(|| {
                map.values()
                    .find_map(|child| find_numeric_value(child, keys))
            }),
        Value::Array(array) => array
            .iter()
            .find_map(|child| find_numeric_value(child, keys)),
        _ => None,
    }
}

pub fn repair_json(input: &str) -> Value {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| {
        let mut repaired = normalize_partial_json(trimmed);
        let mut stack = Vec::new();
        let mut in_string = false;
        let mut escaped = false;
        for character in repaired.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
                continue;
            }
            match character {
                '"' => in_string = true,
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' if stack.last() == Some(&character) => {
                    stack.pop();
                }
                _ => {}
            }
        }
        if in_string {
            repaired.push('"');
        }
        while repaired.ends_with(char::is_whitespace) {
            repaired.pop();
        }
        if repaired.ends_with(',') {
            repaired.pop();
        }
        repaired.extend(stack.into_iter().rev());
        serde_json::from_str(&repaired).unwrap_or_else(|_| json!({"raw":trimmed}))
    })
}

fn normalize_partial_json(input: &str) -> String {
    let characters = input.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in characters.iter().copied().enumerate() {
        if in_string {
            match character {
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                _ => output.push(character),
            }
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            output.push(character);
            continue;
        }
        if character == ','
            && characters[index + 1..]
                .iter()
                .find(|character| !character.is_whitespace())
                .is_some_and(|character| matches!(*character, '}' | ']'))
        {
            continue;
        }
        output.push(character);
    }
    output
}

fn is_write_tool(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "write", "create", "delete", "remove", "edit", "patch", "update", "move", "rename",
        "execute", "command", "shell", "terminal",
    ]
    .iter()
    .any(|marker| name.contains(marker))
}

fn sanitize_text(text: &str) -> String {
    text.replace('\0', "")
        .replace("<tool_use>", "")
        .replace("</tool_use>", "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_split_function_calls_and_keeps_visible_text() {
        let mut filter = ToolLeakFilter::new(true);
        let first = filter.push(KiroEvent::AssistantResponse {
            content: "Before <function_calls><invoke name=\"write_file\"><parameter name=\"path\">"
                .into(),
        });
        let second = filter.push(KiroEvent::AssistantResponse {
            content: "/tmp/a.txt</parameter><parameter name=\"overwrite\">true</parameter>".into(),
        });
        let third = filter.push(KiroEvent::AssistantResponse {
            content: "</invoke></function_calls> after".into(),
        });
        assert_eq!(
            first,
            vec![KiroEvent::AssistantResponse {
                content: "Before ".into()
            }]
        );
        assert!(second.is_empty());
        assert_eq!(
            third,
            vec![KiroEvent::AssistantResponse {
                content: " after".into()
            }]
        );
        let recovered = filter.finish();
        assert_eq!(recovered.len(), 1);
        match &recovered[0] {
            KiroEvent::ToolUse {
                name, input_delta, ..
            } => {
                assert_eq!(name, "write_file");
                assert_eq!(repair_json(input_delta)["path"], "/tmp/a.txt");
                assert_eq!(repair_json(input_delta)["overwrite"], true);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn flushes_incomplete_marker_as_literal_text() {
        let mut filter = ToolLeakFilter::new(true);
        let visible = filter.push(KiroEvent::AssistantResponse {
            content: "Use <function_calls as literal text".into(),
        });
        assert_eq!(visible.len(), 1);
        let tail = filter.finish();
        assert_eq!(tail.len(), 1);
    }

    #[test]
    fn response_format_and_stop_reason_use_config_and_current_round() {
        let decoded = DecodedResponse {
            text: "answer".into(),
            reasoning: "thought".into(),
            usage: UsageInfo {
                output_tokens: 100,
                reasoning_tokens: 7,
                ..UsageInfo::default()
            },
            ..DecodedResponse::default()
        };
        let claude = decoded.claude_json("msg", "model", 50, 10, None);
        assert_eq!(claude["stop_reason"], "end_turn");
        let tagged = decoded.openai_json(
            "chat",
            "model",
            1,
            50,
            10,
            kproxy_core::config::ThinkingOutputFormat::Claude,
            &std::collections::HashMap::new(),
        );
        assert_eq!(
            tagged["choices"][0]["message"]["content"],
            "<thinking>thought</thinking>answer"
        );
        assert!(tagged["choices"][0]["message"]["reasoning_content"].is_null());
        let openai = decoded.openai_json(
            "chat",
            "model",
            1,
            50,
            10,
            kproxy_core::config::ThinkingOutputFormat::Openai,
            &std::collections::HashMap::new(),
        );
        assert_eq!(
            openai["choices"][0]["message"]["reasoning_content"],
            "thought"
        );
    }

    #[test]
    fn claude_response_preserves_tool_search_reference_blocks() {
        let decoded = DecodedResponse {
            tool_searches: vec![ClaudeToolSearchTrace {
                id: "srvtoolu_1".into(),
                name: "tool_search_tool_regex".into(),
                input: json!({"pattern":"github"}),
                references: vec!["mcp__github__list_issues".into()],
                error: None,
            }],
            ..DecodedResponse::default()
        };
        let response = decoded.claude_json("msg", "model", 100, 1, None);
        assert_eq!(response["content"][0]["type"], "server_tool_use");
        assert_eq!(response["content"][1]["type"], "tool_search_tool_result");
        assert_eq!(
            response["content"][1]["content"]["tool_references"][0]["tool_name"],
            "mcp__github__list_issues"
        );
        assert_eq!(response["stop_reason"], "end_turn");
    }

    #[test]
    fn usage_understands_kiro_cache_and_credit_fields() {
        let mut decoded = DecodedResponse::default();
        decoded
            .push(KiroEvent::MessageMetadata {
                usage: json!({"messageMetadataEvent":{"tokenUsage":{
                    "uncachedInputTokens":"10",
                    "cacheReadInputTokens":20,
                    "cacheWriteInputTokens":5,
                    "outputTokens":7,
                    "creditsConsumed":"1.25"
                }}}),
            })
            .expect("usage");
        assert_eq!(decoded.usage.input_tokens, 35);
        assert_eq!(decoded.usage.cache_read_tokens, 20);
        assert_eq!(decoded.usage.cache_write_tokens, 5);
        assert_eq!(decoded.usage.output_tokens, 7);
        assert_eq!(decoded.usage.credits, 1.25);
        let response = decoded.claude_json("msg", "model", 100, 7, None);
        assert_eq!(response["usage"]["input_tokens"], 10);
        assert_eq!(response["usage"]["cache_read_input_tokens"], 20);
        assert_eq!(response["usage"]["cache_creation_input_tokens"], 5);
    }

    #[test]
    fn usage_does_not_add_cache_subsets_to_generic_total_input() {
        let mut decoded = DecodedResponse::default();
        decoded
            .push(KiroEvent::MessageMetadata {
                usage: json!({"messageMetadataEvent":{"tokenUsage":{
                    "inputTokens":100,
                    "cacheReadInputTokens":60,
                    "cacheWriteInputTokens":10,
                    "outputTokens":20
                }}}),
            })
            .expect("usage");

        assert_eq!(decoded.usage.input_tokens, 100);
        let response = decoded.claude_json("msg", "model", 100, 20, None);
        assert_eq!(response["usage"]["input_tokens"], 30);
        assert_eq!(response["usage"]["cache_read_input_tokens"], 60);
        assert_eq!(response["usage"]["cache_creation_input_tokens"], 10);
    }

    #[test]
    fn claude_compaction_block_precedes_the_continued_response() {
        let decoded = DecodedResponse {
            text: "continued answer".into(),
            ..DecodedResponse::default()
        };
        let response = decoded.claude_json(
            "msg",
            "model",
            100,
            10,
            Some("compacted conversation summary"),
        );

        assert_eq!(response["content"][0]["type"], "compaction");
        assert_eq!(
            response["content"][0]["content"],
            "compacted conversation summary"
        );
        assert_eq!(response["content"][1]["type"], "text");
    }

    #[test]
    fn custom_openai_tools_roundtrip_their_original_shape() {
        let mut decoded = DecodedResponse::default();
        decoded
            .push(KiroEvent::ToolUse {
                id: "call_1".into(),
                name: "shell_tool".into(),
                input_delta: r#"{"input":"echo ok"}"#.into(),
                stop: true,
            })
            .expect("tool");
        let registry = std::collections::HashMap::from([(
            "shell_tool".into(),
            OpenAiToolIdentity {
                kind: "custom".into(),
                name: "shell.tool".into(),
            },
        )]);
        let response = decoded.openai_json(
            "chat",
            "model",
            1,
            10,
            1,
            kproxy_core::config::ThinkingOutputFormat::Openai,
            &registry,
        );
        let call = &response["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["type"], "custom");
        assert_eq!(call["custom"]["name"], "shell.tool");
        assert_eq!(call["custom"]["input"], "echo ok");
    }

    #[test]
    fn rejects_truncated_write_tools_and_cumulative_tool_overflow() {
        assert_eq!(repair_json("{\"ok\":true,}"), json!({"ok":true}));
        assert_eq!(
            repair_json("{\"line\":\"one\ntwo\"}"),
            json!({"line":"one\ntwo"})
        );
        let mut truncated = DecodedResponse::default();
        truncated
            .push(KiroEvent::ToolUse {
                id: "write".into(),
                name: "write_file".into(),
                input_delta: r#"{"path":"/tmp/a""#.into(),
                stop: false,
            })
            .expect("buffer");
        assert!(truncated.validate_tool_inputs().is_err());

        let mut oversized = DecodedResponse::default();
        assert!(oversized
            .push(KiroEvent::ToolUse {
                id: "large".into(),
                name: "read".into(),
                input_delta: "x".repeat(MAX_CUMULATIVE_TOOL_INPUT_BYTES + 1),
                stop: false,
            })
            .is_err());
    }
}

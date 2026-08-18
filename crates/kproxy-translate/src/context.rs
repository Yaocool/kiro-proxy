//! Claude context-management parsing and compaction-boundary handling.

use serde_json::Value;

use crate::ClaudeRequest;

pub const DEFAULT_COMPACT_TRIGGER_TOKENS: u64 = 150_000;
pub const MIN_COMPACT_TRIGGER_TOKENS: u64 = 50_000;

/// Returns the configured server-compaction trigger, if the request contains
/// the supported `compact_20260112` edit.
pub fn compact_trigger_tokens(context_management: Option<&Value>) -> Option<u64> {
    context_management?
        .get("edits")?
        .as_array()?
        .iter()
        .find(|edit| edit.get("type").and_then(Value::as_str) == Some("compact_20260112"))
        .map(|edit| {
            edit.pointer("/trigger/value")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_COMPACT_TRIGGER_TOKENS)
        })
}

/// Claude ignores every content block before the most recent compaction block.
/// Apply that boundary before translating the request so previously compacted
/// conversations do not grow back to their pre-summary size.
pub fn apply_compaction_boundary(request: &mut ClaudeRequest) -> bool {
    let boundary =
        request
            .messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(message_index, message)| {
                message
                    .content
                    .as_array()?
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, block)| {
                        block.get("type").and_then(Value::as_str) == Some("compaction")
                    })
                    .map(|(block_index, _)| (message_index, block_index))
            });
    let Some((message_index, block_index)) = boundary else {
        return false;
    };

    request.messages.drain(..message_index);
    if let Some(blocks) = request
        .messages
        .first_mut()
        .and_then(|message| message.content.as_array_mut())
    {
        blocks.drain(..block_index);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_compact_trigger_and_applies_latest_boundary() {
        let mut request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4.6",
            "max_tokens":1024,
            "context_management":{"edits":[{
                "type":"compact_20260112",
                "trigger":{"type":"input_tokens","value":75000}
            }]},
            "messages":[
                {"role":"user","content":"old"},
                {"role":"assistant","content":[
                    {"type":"text","text":"discard"},
                    {"type":"compaction","content":"summary"}
                ]},
                {"role":"user","content":"new"}
            ]
        }))
        .expect("request");

        assert_eq!(
            compact_trigger_tokens(request.context_management.as_ref()),
            Some(75_000)
        );
        assert!(apply_compaction_boundary(&mut request));
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].content[0]["type"], "compaction");
    }

    #[test]
    fn compaction_discards_stale_pending_server_calls() {
        let mut request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4.6",
            "max_tokens":1024,
            "messages":[
                {"role":"assistant","content":[{
                    "type":"server_tool_use",
                    "id":"srvtoolu_stale",
                    "name":"web_search",
                    "input":{"query":"stale"}
                }]},
                {"role":"assistant","content":[{
                    "type":"compaction",
                    "content":"The stale search is no longer part of active context."
                }]},
                {"role":"user","content":"continue"}
            ]
        }))
        .expect("request");

        assert!(apply_compaction_boundary(&mut request));
        assert!(crate::claude_pending_server_tool_uses(&request).is_empty());
    }

    #[test]
    fn compaction_boundary_removes_stale_invalid_server_history_before_validation() {
        let mut request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4.6",
            "max_tokens":1024,
            "messages":[
                {"role":"assistant","content":[
                    {"type":"web_search_tool_result","tool_use_id":"orphan","content":[{
                        "type":"web_search_result","url":"https://example.com","title":"stale"
                    }]}
                ]},
                {"role":"assistant","content":[{
                    "type":"compaction","content":"The stale result is outside active context."
                }]},
                {"role":"user","content":"continue"}
            ]
        }))
        .expect("request");

        assert!(crate::validate_claude(&request).is_err());
        assert!(apply_compaction_boundary(&mut request));
        crate::validate_claude(&request).expect("effective compacted history is valid");
    }
}

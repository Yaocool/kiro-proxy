//! Claude context-management parsing and local compatibility handling.

use serde_json::Value;
use std::collections::{HashMap, HashSet};

use crate::{matches_type_family, ClaudeRequest};

pub const DEFAULT_COMPACT_TRIGGER_TOKENS: u64 = 150_000;
pub const MIN_COMPACT_TRIGGER_TOKENS: u64 = 50_000;
pub const DEFAULT_TOOL_USES_TO_KEEP: usize = 3;
pub const CLEARED_TOOL_RESULT_TEXT: &str =
    "[Older tool result omitted by Claude context management.]";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeContextEditStats {
    pub cleared_tool_results: usize,
    pub cleared_tool_inputs: usize,
}

impl ClaudeContextEditStats {
    pub fn changed(&self) -> bool {
        self.cleared_tool_results > 0 || self.cleared_tool_inputs > 0
    }
}

/// Returns whether an edit type belongs to Claude's compaction strategy family.
///
/// The suffix is intentionally opaque: clients may send date versions, aliases,
/// or future version schemes while the locally emulated fields remain stable.
pub fn is_compact_edit_type(edit_type: &str) -> bool {
    edit_type.starts_with("compact_")
}

/// Returns the configured server-compaction trigger, if the request contains
/// an edit in the `compact_*` type family.
pub fn compact_trigger_tokens(context_management: Option<&Value>) -> Option<u64> {
    context_management?
        .get("edits")?
        .as_array()?
        .iter()
        .find(|edit| {
            edit.get("type")
                .and_then(Value::as_str)
                .is_some_and(is_compact_edit_type)
        })
        .map(|edit| {
            edit.pointer("/trigger/value")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_COMPACT_TRIGGER_TOKENS)
        })
}

/// Returns whether a request contains at least one context-management edit.
///
/// Token-count responses use this to preserve Anthropic's response envelope
/// even when an edit is a safe local no-op (for example Claude Code's
/// `clear_thinking_*` with `keep: "all"`).
pub fn has_context_management_edits(context_management: Option<&Value>) -> bool {
    context_management
        .and_then(|value| value.get("edits"))
        .and_then(Value::as_array)
        .is_some_and(|edits| !edits.is_empty())
}

#[derive(Debug)]
struct ClearToolUsesPlan {
    keep: usize,
    exclude_tools: HashSet<String>,
    clear_tool_inputs: bool,
}

fn clear_tool_uses_plan(context_management: Option<&Value>) -> Option<ClearToolUsesPlan> {
    let edit = context_management?
        .get("edits")?
        .as_array()?
        .iter()
        .find(|edit| {
            edit.get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches_type_family(kind, "clear_tool_uses"))
        })?;
    let keep = if edit.pointer("/keep/type").and_then(Value::as_str) == Some("tool_uses") {
        edit.pointer("/keep/value")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_TOOL_USES_TO_KEEP)
    } else {
        DEFAULT_TOOL_USES_TO_KEEP
    };
    let exclude_tools = edit
        .get("exclude_tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    Some(ClearToolUsesPlan {
        keep,
        exclude_tools,
        clear_tool_inputs: edit
            .get("clear_tool_inputs")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Applies context edits that can be represented safely before translating to
/// Kiro. Unsupported and future edit families remain forward-compatible no-ops.
/// In particular, Claude Code currently sends `clear_thinking_*` with
/// `keep: "all"`; retaining those blocks already satisfies that contract.
pub fn apply_context_management_edits(request: &mut ClaudeRequest) -> ClaudeContextEditStats {
    let Some(plan) = clear_tool_uses_plan(request.context_management.as_ref()) else {
        return ClaudeContextEditStats::default();
    };

    let tool_names = request
        .messages
        .iter()
        .filter_map(|message| message.content.as_array())
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| {
            Some((
                block.get("id")?.as_str()?.to_owned(),
                block.get("name")?.as_str()?.to_owned(),
            ))
        })
        .collect::<HashMap<_, _>>();

    let mut remaining_to_keep = plan.keep;
    let mut cleared_ids = HashSet::new();
    for message in request.messages.iter_mut().rev() {
        let Some(blocks) = message.content.as_array_mut() else {
            continue;
        };
        for block in blocks.iter_mut().rev() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(tool_use_id) = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            if tool_names
                .get(&tool_use_id)
                .is_some_and(|name| plan.exclude_tools.contains(name))
            {
                continue;
            }
            if remaining_to_keep > 0 {
                remaining_to_keep -= 1;
                continue;
            }
            if let Some(object) = block.as_object_mut() {
                object.insert(
                    "content".into(),
                    Value::String(CLEARED_TOOL_RESULT_TEXT.into()),
                );
                cleared_ids.insert(tool_use_id);
            }
        }
    }

    let mut cleared_tool_inputs = 0;
    if plan.clear_tool_inputs && !cleared_ids.is_empty() {
        for message in &mut request.messages {
            let Some(blocks) = message.content.as_array_mut() else {
                continue;
            };
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_use")
                    || !block
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| cleared_ids.contains(id))
                {
                    continue;
                }
                if let Some(object) = block.as_object_mut() {
                    object.insert("input".into(), Value::Object(Default::default()));
                    cleared_tool_inputs += 1;
                }
            }
        }
    }

    ClaudeContextEditStats {
        cleared_tool_results: cleared_ids.len(),
        cleared_tool_inputs,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaudeCompactionNormalizationStats {
    pub boundary_applied: bool,
    pub removed_noop_blocks: usize,
    pub removed_noop_messages: usize,
}

impl ClaudeCompactionNormalizationStats {
    pub fn changed(self) -> bool {
        self.boundary_applied || self.removed_noop_blocks > 0 || self.removed_noop_messages > 0
    }
}

/// Claude ignores every content block before the most recent completed
/// compaction block. A missing or `null` compaction content value is a failed
/// compaction/no-op and must not replace an earlier completed boundary.
///
/// Apply the completed boundary before translating the request so previously
/// compacted conversations do not grow back to their pre-summary size. No-op
/// assistant compaction blocks are removed after the boundary is applied.
pub fn normalize_compaction_boundary(
    request: &mut ClaudeRequest,
) -> ClaudeCompactionNormalizationStats {
    let boundary = request
        .messages
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.role == "assistant")
        .find_map(|(message_index, message)| {
            message
                .content
                .as_array()?
                .iter()
                .enumerate()
                .rev()
                .find(|(_, block)| {
                    block.get("type").and_then(Value::as_str) == Some("compaction")
                        && block
                            .get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|content| !content.is_empty())
                })
                .map(|(block_index, _)| (message_index, block_index))
        });
    if let Some((message_index, block_index)) = boundary {
        request.messages.drain(..message_index);
        if let Some(blocks) = request
            .messages
            .first_mut()
            .and_then(|message| message.content.as_array_mut())
        {
            blocks.drain(..block_index);
        }
    }

    let mut removed_noop_blocks = 0;
    let mut removed_noop_messages = 0;
    request.messages.retain_mut(|message| {
        if message.role != "assistant" {
            return true;
        }
        let Some(blocks) = message.content.as_array_mut() else {
            return true;
        };
        let original_len = blocks.len();
        blocks.retain(|block| {
            if block.get("type").and_then(Value::as_str) != Some("compaction") {
                return true;
            }
            match block.get("content") {
                None | Some(Value::Null) => {
                    removed_noop_blocks += 1;
                    false
                }
                _ => true,
            }
        });
        let keep = original_len == 0 || !blocks.is_empty();
        if !keep {
            removed_noop_messages += 1;
        }
        keep
    });

    ClaudeCompactionNormalizationStats {
        boundary_applied: boundary.is_some(),
        removed_noop_blocks,
        removed_noop_messages,
    }
}

/// Backward-compatible helper for callers interested only in whether a valid
/// compaction boundary discarded prior history.
pub fn apply_compaction_boundary(request: &mut ClaudeRequest) -> bool {
    normalize_compaction_boundary(request).boundary_applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognizes_compaction_edit_types_without_parsing_the_suffix() {
        assert!(is_compact_edit_type("compact_20260112"));
        assert!(is_compact_edit_type("compact_next"));
        assert!(is_compact_edit_type("compact_202601120"));
        assert!(is_compact_edit_type("compact_"));
        assert!(!is_compact_edit_type("compact"));
        assert!(!is_compact_edit_type("compaction_next"));
        assert!(!is_compact_edit_type("clear_tool_uses_20250919"));
    }

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
    fn reads_trigger_from_an_opaque_compaction_version() {
        let context_management = json!({"edits":[{
            "type":"compact_next",
            "trigger":{"type":"input_tokens","value":80_000}
        }]});

        assert_eq!(
            compact_trigger_tokens(Some(&context_management)),
            Some(80_000)
        );
    }

    #[test]
    fn accepts_clear_thinking_keep_all_as_a_noop() {
        let mut request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-opus-5",
            "max_tokens":1024,
            "context_management":{"edits":[{
                "type":"clear_thinking_20251015",
                "keep":"all"
            }]},
            "messages":[{"role":"assistant","content":[{
                "type":"thinking","thinking":"retain me"
            }]}]
        }))
        .expect("request");

        assert!(has_context_management_edits(
            request.context_management.as_ref()
        ));
        assert_eq!(
            apply_context_management_edits(&mut request),
            ClaudeContextEditStats::default()
        );
        assert_eq!(request.messages[0].content[0]["thinking"], "retain me");
    }

    #[test]
    fn clears_old_tool_results_and_inputs_but_honors_exclusions() {
        let mut request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4.6",
            "max_tokens":1024,
            "context_management":{"edits":[{
                "type":"clear_tool_uses_20250919",
                "keep":{"type":"tool_uses","value":1},
                "exclude_tools":["Preserve"],
                "clear_tool_inputs":true
            }]},
            "messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"old","name":"Read","input":{"path":"old"}},
                    {"type":"tool_use","id":"excluded","name":"Preserve","input":{"key":"safe"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"old","content":"old output"},
                    {"type":"tool_result","tool_use_id":"excluded","content":"preserved output"}
                ]},
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"recent","name":"Read","input":{"path":"new"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"recent","content":"recent output"}
                ]}
            ]
        }))
        .expect("request");

        let stats = apply_context_management_edits(&mut request);

        assert_eq!(stats.cleared_tool_results, 1);
        assert_eq!(stats.cleared_tool_inputs, 1);
        assert_eq!(request.messages[0].content[0]["input"], json!({}));
        assert_eq!(
            request.messages[1].content[0]["content"],
            CLEARED_TOOL_RESULT_TEXT
        );
        assert_eq!(
            request.messages[1].content[1]["content"],
            "preserved output"
        );
        assert_eq!(request.messages[3].content[0]["content"], "recent output");
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

    #[test]
    fn null_compaction_is_a_noop_and_does_not_discard_history() {
        let mut request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-opus-5",
            "max_tokens":1024,
            "messages":[
                {"role":"user","content":"retain me"},
                {"role":"assistant","content":[{
                    "type":"compaction","content":null
                }]},
                {"role":"user","content":"continue"}
            ]
        }))
        .expect("request");

        assert!(!apply_compaction_boundary(&mut request));
        assert_eq!(request.messages.len(), 2);
        assert_eq!(request.messages[0].content, json!("retain me"));
        assert_eq!(request.messages[1].content, json!("continue"));
    }

    #[test]
    fn null_compaction_does_not_supersede_the_latest_completed_boundary() {
        let mut request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-opus-5",
            "max_tokens":1024,
            "messages":[
                {"role":"user","content":"discard me"},
                {"role":"assistant","content":[{
                    "type":"compaction","content":"completed summary"
                }]},
                {"role":"user","content":"retain me"},
                {"role":"assistant","content":[{
                    "type":"compaction","content":null
                }]},
                {"role":"user","content":"continue"}
            ]
        }))
        .expect("request");

        assert!(apply_compaction_boundary(&mut request));
        assert_eq!(request.messages.len(), 3);
        assert_eq!(
            request.messages[0].content[0]["content"],
            "completed summary"
        );
        assert_eq!(request.messages[1].content, json!("retain me"));
        assert_eq!(request.messages[2].content, json!("continue"));
    }

    #[test]
    fn empty_compaction_is_preserved_for_validation() {
        let mut request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-opus-5",
            "max_tokens":1024,
            "messages":[
                {"role":"user","content":"retain me"},
                {"role":"assistant","content":[
                    {"type":"text","text":"also retain me"},
                    {"type":"compaction","content":""}
                ]}
            ]
        }))
        .expect("request");

        assert!(!apply_compaction_boundary(&mut request));
        assert_eq!(request.messages.len(), 2);
        assert!(crate::validate_claude(&request)
            .expect_err("empty compaction summary must be rejected")
            .to_string()
            .contains("must not be empty"));
    }

    #[test]
    fn missing_compaction_content_is_a_noop_and_reports_normalization() {
        let mut request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-opus-5",
            "max_tokens":1024,
            "messages":[
                {"role":"user","content":"retain me"},
                {"role":"assistant","content":[{"type":"compaction"}]},
                {"role":"user","content":"continue"}
            ]
        }))
        .expect("request");

        let stats = normalize_compaction_boundary(&mut request);
        assert_eq!(
            stats,
            ClaudeCompactionNormalizationStats {
                boundary_applied: false,
                removed_noop_blocks: 1,
                removed_noop_messages: 1,
            }
        );
        assert!(stats.changed());
        assert_eq!(request.messages.len(), 2);
    }
}

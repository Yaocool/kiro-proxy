use kproxy_translate::{
    claude_to_kiro, openai_to_kiro, validate_claude, validate_openai, ClaudeRequest, OpenAiRequest,
    TranslationOptions,
};
use serde_json::{json, Value};

fn claude_request(control: &Value) -> ClaudeRequest {
    serde_json::from_value(json!({
        "model":"claude-opus-5", "max_tokens":128,
        "cache_control":control,
        "system":[{"type":"text","text":"System context","cache_control":control}],
        "tools":[{"name":"lookup","input_schema":{"type":"object"},"cache_control":control}],
        "messages":[
            {"role":"user","content":"Hello","cache_control":control},
            {"role":"assistant","content":[{"type":"text","text":"Ready","cache_control":control}]},
            {"role":"user","content":[{"type":"text","text":"Continue","cache_control":control}]}
        ]
    }))
    .unwrap()
}

fn openai_request(control: &Value) -> OpenAiRequest {
    serde_json::from_value(json!({
        "model":"claude-opus-5", "max_tokens":128,
        "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}},"cache_control":control}],
        "messages":[
            {"role":"system","content":[{"type":"text","text":"System context","cache_control":control}]},
            {"role":"user","content":"Hello","cache_control":control},
            {"role":"assistant","content":[{"type":"text","text":"Ready","cache_control":control}]},
            {"role":"user","content":[{"type":"text","text":"Continue","cache_control":control}],"cache_control":control}
        ]
    })).unwrap()
}

fn options() -> TranslationOptions {
    let mut options = TranslationOptions::new("claude-opus-5", "AI_EDITOR");
    options.enhance_system_prompt = false;
    options.enable_prompt_cache = true;
    options
}

#[test]
fn assistant_history_cache_hint_does_not_block_generation() {
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-opus-5", "max_tokens":128,
        "messages":[
            {"role":"user","content":"Hello"},
            {"role":"assistant","content":[{"type":"text","text":"Ready",
                "cache_control":{"type":"future_cache_hint"}}]},
            {"role":"user","content":"Continue"}
        ]
    }))
    .unwrap();
    validate_claude(&request).unwrap();
}

#[test]
fn cache_hints_and_null_do_not_reject_messages_or_create_kiro_breakpoints() {
    for control in [
        Value::Null,
        json!({"type":"disabled"}),
        json!({"type":"default"}),
        json!({"type":"future_cache_hint","ttl":"future_ttl","scope":"global"}),
        json!({"type":"Ephemeral"}),
    ] {
        let claude = claude_request(&control);
        validate_claude(&claude).unwrap_or_else(|error| panic!("{control}: {error}"));
        let openai = openai_request(&control);
        validate_openai(&openai).unwrap_or_else(|error| panic!("{control}: {error}"));
        for payload in [
            claude_to_kiro(&claude, &options()),
            openai_to_kiro(&openai, &options()),
        ] {
            let wire = serde_json::to_string(&payload).unwrap();
            assert!(wire.contains("Continue"), "{wire}");
            assert!(!wire.contains("cachePoint"), "{wire}");
            assert!(!wire.contains("cache_control"), "{wire}");
            assert!(!wire.contains("future_cache_hint"), "{wire}");
        }
    }
}

#[test]
fn ephemeral_extensions_map_only_to_native_default_markers() {
    let control = json!({"type":"ephemeral","ttl":"1h","scope":"global","evict_on_complete":true});
    let mut claude = claude_request(&Value::Null);
    claude.messages[1].content[0]["cache_control"] = control.clone();
    validate_claude(&claude).unwrap();
    let mut openai = openai_request(&Value::Null);
    openai.messages[1].cache_control = Some(control);
    validate_openai(&openai).unwrap();
    for payload in [
        claude_to_kiro(&claude, &options()),
        openai_to_kiro(&openai, &options()),
    ] {
        let wire = serde_json::to_string(&payload).unwrap();
        assert!(
            wire.contains(r#""cachePoint":{"type":"default"}"#),
            "{wire}"
        );
        for excluded in [
            "cache_control",
            "ephemeral",
            "ttl",
            "scope",
            "evict_on_complete",
        ] {
            assert!(!wire.contains(excluded), "{excluded} leaked into {wire}");
        }
    }
}

#[test]
fn only_ephemeral_markers_use_the_four_breakpoint_budget() {
    for hint in [Value::Null, json!({"type":"future_cache_hint"})] {
        let marker = json!({"type":"ephemeral","ttl":"5m","scope":"global"});
        let mut claude = claude_request(&hint);
        claude.cache_control = Some(marker.clone());
        claude.system.as_mut().unwrap()[0]["cache_control"] = marker.clone();
        claude.tools[0].cache_control = Some(marker.clone());
        claude.messages[2].content[0]["cache_control"] = marker.clone();
        validate_claude(&claude).unwrap();
        claude.messages[1].content[0]["cache_control"] = marker.clone();
        assert!(validate_claude(&claude)
            .unwrap_err()
            .to_string()
            .contains("at most 4"));

        let mut openai = openai_request(&hint);
        openai.tools[0].body["cache_control"] = marker.clone();
        openai.messages[0].content.as_mut().unwrap()[0]["cache_control"] = marker.clone();
        openai.messages[1].cache_control = Some(marker.clone());
        openai.messages[3].content.as_mut().unwrap()[0]["cache_control"] = marker.clone();
        validate_openai(&openai).unwrap();
        openai.messages[2].cache_control = Some(marker);
        assert!(validate_openai(&openai)
            .unwrap_err()
            .to_string()
            .contains("at most 4"));
    }
}

#[test]
fn ignored_hints_do_not_erase_ephemeral_markers_when_merging_adjacent_messages() {
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-opus-5","max_tokens":128,
        "messages":[
            {"role":"user","content":"Cache this","cache_control":{"type":"ephemeral"}},
            {"role":"user","content":"Continue","cache_control":{"type":"future_cache_hint"}}
        ]
    }))
    .unwrap();
    validate_claude(&request).unwrap();
    let payload = claude_to_kiro(&request, &options());
    let current = payload
        .conversation_state
        .current_message
        .user_input_message;
    assert_eq!(current.content, "Cache this\nContinue");
    assert!(current.cache_point.is_some());
}

#[test]
fn malformed_cache_controls_still_report_the_exact_field() {
    for (control, suffix) in [
        (json!(false), ""),
        (json!([]), ""),
        (json!("ephemeral"), ""),
        (json!({}), ".type"),
        (json!({"type":null}), ".type"),
        (json!({"type":42}), ".type"),
        (json!({"type":"  "}), ".type"),
        (json!({"type":"ephemeral","ttl":"30m"}), ".ttl"),
        (json!({"type":"ephemeral","ttl":null}), ".ttl"),
    ] {
        let mut claude = claude_request(&Value::Null);
        claude.messages[1].content[0]["cache_control"] = control.clone();
        let mut openai = openai_request(&Value::Null);
        openai.messages[1].content =
            Some(json!([{"type":"text","text":"Hello","cache_control":control}]));
        for result in [validate_claude(&claude), validate_openai(&openai)] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains(&format!("messages.1.content.0.cache_control{suffix}:")),
                "{control}"
            );
        }
    }
}

#[test]
fn nested_cache_hints_are_ignored_without_losing_tool_results_or_documents() {
    let mut request = claude_request(&Value::Null);
    request.messages[1].content =
        json!([{"type":"tool_use","id":"call_1","name":"lookup","input":{}}]);
    request.messages[2].content = json!([{
        "type":"tool_result","tool_use_id":"call_1","cache_control":{"type":"future_cache_hint"},
        "content":[
            {"type":"text","text":"Tool result","cache_control":null},
            {"type":"document","cache_control":null,"source":{"type":"content","content":[
                {"type":"text","text":"Document text","cache_control":{"type":"future_cache_hint"}}
            ]}}
        ]
    }]);
    validate_claude(&request).unwrap();
    let payload = claude_to_kiro(&request, &options());
    let wire = serde_json::to_string(&payload).unwrap();
    assert!(wire.contains("Tool result"), "{wire}");
    use base64::Engine as _;
    assert_eq!(
        payload
            .conversation_state
            .current_message
            .user_input_message
            .documents[0]
            .source
            .bytes,
        base64::engine::general_purpose::STANDARD.encode("Document text")
    );
    assert!(!wire.contains("cachePoint"), "{wire}");
    request.messages[2].content[0]["content"][1]["source"]["content"][0]["cache_control"] =
        json!({"type":"ephemeral","ttl":"30m"});
    assert!(validate_claude(&request)
        .unwrap_err()
        .to_string()
        .contains("messages.2.content.0.content.1.source.content.0.cache_control.ttl"));
}

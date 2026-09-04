//! Compatibility baseline: jwadow a5292ca, hj01857655 c5c4776 and chaogei 447adcd.
//! Their Claude/Kiro paths accept format and extra controls without enforcing
//! official structured-output guarantees. Compare actual serialized Kiro input.
use kproxy_translate::{
    claude_to_kiro, openai_to_kiro, responses_to_openai, validate_claude, validate_openai,
    ClaudeRequest, OpenAiRequest, ResponsesRequest, TranslationOptions,
};
use serde_json::{json, Value};

fn output_format() -> Value {
    json!({"type":"json_schema","schema":{
        "type":"object","properties":{"output_format_sentinel":{"type":"string"}},
        "required":["output_format_sentinel"],"additionalProperties":false
    }})
}

fn assert_kiro_projection(wire: Value) {
    assert!(wire.get("additionalModelRequestFields").is_none(), "{wire}");
    let serialized = wire.to_string();
    for ignored in [
        "output_format_sentinel",
        "strict",
        "future_hint",
        "output_config",
        "response_format",
    ] {
        assert!(
            !serialized.contains(ignored),
            "unexpected {ignored}: {wire}"
        );
    }
    assert!(
        serialized.contains("tool_argument"),
        "tool input schema was lost: {wire}"
    );
    assert!(
        serialized.contains("Reply pong"),
        "user content was lost: {wire}"
    );
}

#[test]
fn claude_format_and_additive_hints_do_not_reject_a_kiro_request() {
    for format in [output_format(), json!({"type":"json_object"}), Value::Null] {
        let request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-haiku-4-5-20251001","max_tokens":4096,
            "messages":[{"role":"user","content":"Reply pong","future_hint":true}],
            "output_config":{"format":format,"effort":"future_effort","future_hint":true},
            "service_tier":"standard_only","future_hint":true,
            "tools":[{"name":"lookup","description":"Find a record","strict":true,
                "eager_input_streaming":true,"future_hint":true,
                "input_schema":{"type":"object","properties":{"tool_argument":{"type":"string"}}}}]
        }))
        .unwrap();
        validate_claude(&request).expect("reference-compatible Claude controls");
        let wire = serde_json::to_value(claude_to_kiro(
            &request,
            &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
        ))
        .unwrap();
        assert_kiro_projection(wire);
    }
}

#[test]
fn openai_format_and_additive_hints_do_not_reject_a_kiro_request() {
    for stream in [false, true] {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model":"claude-haiku-4.5","max_tokens":4096,"stream":stream,
            "messages":[{"role":"user","content":"Reply pong","future_hint":true}],
            "response_format":{"type":"json_schema","json_schema":{
                "name":"answer","strict":true,"schema":output_format()["schema"]}},
            "stream_options":{"include_usage":true,"future_hint":true},
            "service_tier":"priority","future_hint":true,
            "tools":[{"type":"function","function":{"name":"lookup","strict":true,
                "parameters":{"type":"object","properties":{"tool_argument":{"type":"string"}}}}}]
        }))
        .expect("additive OpenAI request fields");
        validate_openai(&request).expect("reference-compatible OpenAI controls");
        assert_kiro_projection(
            serde_json::to_value(openai_to_kiro(
                &request,
                &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
            ))
            .unwrap(),
        );
    }
}

#[test]
fn responses_format_and_additive_hints_use_the_same_kiro_projection() {
    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"claude-haiku-4.5","input":"Reply pong","max_output_tokens":4096,
        "text":{"format":output_format(),"verbosity":"future_verbosity","future_hint":true},
        "reasoning":{"summary":"future_summary","context":"future_context","future_hint":true},
        "include":["file_search_call.results"],"prompt_cache_retention":"24h",
        "service_tier":"priority","future_hint":true,
        "stream_options":{"include_obfuscation":true,"future_hint":true},
        "tools":[{"type":"function","name":"lookup","strict":true,
            "parameters":{"type":"object","properties":{"tool_argument":{"type":"string"}}}}]
    }))
    .expect("additive Responses request fields");
    let normalized =
        responses_to_openai(&request).expect("reference-compatible Responses controls");
    assert_kiro_projection(
        serde_json::to_value(openai_to_kiro(
            &normalized.request,
            &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
        ))
        .unwrap(),
    );
}

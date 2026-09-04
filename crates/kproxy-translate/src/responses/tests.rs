use super::*;
use crate::{
    openai_to_kiro, sanitize_kiro_tool_history, validate_kiro_tool_history, TranslationOptions,
};

fn normalize(value: Value) -> ResponsesTranslation {
    responses_to_openai(&serde_json::from_value(value).expect("Responses request"))
        .expect("supported request")
}

#[test]
fn codex_request_preserves_instructions_controls_and_namespace_tools() {
    let translated = normalize(json!({
        "model":"claude-sonnet-4.5", "instructions":"Follow repository instructions.",
        "input":[{"role":"developer","content":"Use safe commands."},{"role":"user","content":[{"type":"input_text","text":"Inspect the project."}]}],
        "store":false,"stream":true,"parallel_tool_calls":false,
        "stream_options":{"reasoning_summary_delivery":"sequential_cutoff"},
        "include":["reasoning.encrypted_content"],"prompt_cache_key":"thread-1",
        "client_metadata":{"turn_id":"turn-1"},
        "max_output_tokens":2048,"temperature":0.4,"top_p":0.9,
        "reasoning":{"effort":"high","summary":"auto"},"text":{"verbosity":"low"},
        "tools":[{"type":"namespace","name":"functions","description":"Repository tools.","tools":[
            {"type":"function","name":"read_file","parameters":{"type":"object","properties":{"path":{"type":"string"}}},"strict":false},
            {"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}}
        ]}],
        "tool_choice":{"type":"function","namespace":"functions","name":"read_file"}
    }));
    let request = &translated.request;
    assert_eq!(
        request.messages[0].content,
        Some(json!("Follow repository instructions."))
    );
    assert_eq!(request.messages[2].role, "developer");
    assert_eq!(request.max_tokens, Some(2048));
    assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
    assert!(!request.parallel_tool_calls);
    assert_eq!(request.stream_options, Some(json!({"include_usage":true})));
    assert_eq!(
        request.tools[0].body["function"]["name"],
        "functions.read_file"
    );
    assert!(request.tools[0].body["function"]["description"]
        .as_str()
        .unwrap()
        .contains("Repository tools."));
    assert_eq!(
        request.tools[1].body["custom"]["format"]["grammar"]["syntax"],
        "lark"
    );
    assert_eq!(
        request.tool_choice.as_ref().unwrap()["function"]["name"],
        "functions.read_file"
    );
    assert_eq!(
        translated.tool_names["functions.apply_patch"].name,
        "apply_patch"
    );
    assert_eq!(
        translated.tool_names["functions.apply_patch"]
            .namespace
            .as_deref(),
        Some("functions")
    );
}

#[test]
fn stateless_tool_roundtrip_keeps_call_ids_inputs_reasoning_and_results() {
    let translated = normalize(json!({
        "model":"claude-sonnet-4.5", "input":[
            {"type":"message","role":"user","content":"Update the file."},
            {"type":"reasoning","id":"rs_previous","summary":[{"type":"summary_text","text":"Read the file before editing."}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Checking the file."}],"status":"completed","phase":"commentary"},
            {"type":"function_call","id":"fc_item","call_id":"call_read","namespace":"functions","name":"read_file","arguments":"{\"path\":\"README.md\"}"},
            {"type":"custom_tool_call","id":"ctc_item","call_id":"call_patch","name":"apply_patch","input":"*** Begin Patch\n*** End Patch"},
            {"type":"function_call_output","call_id":"call_read","output":[{"type":"input_text","text":"read result"}]},
            {"type":"custom_tool_call_output","call_id":"call_patch","output":"patched"}
        ], "tools":[
            {"type":"namespace","name":"functions","tools":[{"type":"function","name":"read_file","parameters":{"type":"object"}}]},
            {"type":"custom","name":"apply_patch"}
        ]
    }));
    let mut payload = openai_to_kiro(
        &translated.request,
        &TranslationOptions::new("claude-sonnet-4.5", "AI_EDITOR"),
    );
    sanitize_kiro_tool_history(&mut payload);
    validate_kiro_tool_history(&payload).expect("paired tool history");
    let assistant = payload
        .conversation_state
        .history
        .iter()
        .filter_map(|item| item.assistant_response_message.as_ref())
        .next_back()
        .unwrap();
    assert!(assistant.content.contains("Read the file before editing."));
    assert!(assistant.content.contains("Checking the file."));
    assert_eq!(assistant.tool_uses.len(), 2);
    assert_eq!(assistant.tool_uses[0].tool_use_id, "call_read");
    assert_eq!(assistant.tool_uses[0].input["path"], "README.md");
    assert_eq!(
        assistant.tool_uses[1].input["input"],
        "*** Begin Patch\n*** End Patch"
    );
    let results = &payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .unwrap()
        .tool_results;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].tool_use_id, "call_read");
    assert_eq!(results[0].content[0].text, "read result");
    assert_eq!(results[1].content[0].text, "patched");
}

#[test]
fn image_tool_output_remains_in_the_kiro_tool_result_turn() {
    let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jB9sAAAAASUVORK5CYII=";
    for next_user in [false, true] {
        let mut input = vec![
            json!({"role":"user","content":"Look at the image."}),
            json!({"type":"function_call","call_id":"image","name":"view_image","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"image","output":[
                {"type":"input_text","text":"Image follows."},
                {"type":"input_image","image_url":image}
            ]}),
        ];
        if next_user {
            input.push(json!({"role":"user","content":"Describe it."}));
        }
        let translated = normalize(
            json!({"model":"test","input":input,"tools":[{"type":"function","name":"view_image","parameters":{"type":"object"}}]}),
        );
        let mut payload = openai_to_kiro(
            &translated.request,
            &TranslationOptions::new("test", "AI_EDITOR"),
        );
        sanitize_kiro_tool_history(&mut payload);
        validate_kiro_tool_history(&payload).expect("paired image tool");
        let user = if next_user {
            payload
                .conversation_state
                .history
                .iter()
                .filter_map(|m| m.user_input_message.as_ref())
                .next_back()
                .unwrap()
        } else {
            &payload
                .conversation_state
                .current_message
                .user_input_message
        };
        assert_eq!(user.images.len(), 1);
        assert_eq!(
            user.user_input_message_context
                .as_ref()
                .unwrap()
                .tool_results[0]
                .tool_use_id,
            "image"
        );
    }
}

#[test]
fn omitted_token_limit_and_none_reasoning_are_preserved() {
    let translated =
        normalize(json!({"model":"test","input":"hello","reasoning":{"effort":"none"}}));
    assert_eq!(translated.request.max_tokens, None);
    assert_eq!(
        translated.request.thinking.as_ref().unwrap().r#type,
        "disabled"
    );
    assert_eq!(translated.request.reasoning_effort, None);
    let translated = normalize(json!({"model":"test","input":"hello"}));
    assert!(translated.request.thinking.is_none());
}

#[test]
fn nullable_response_controls_use_their_omitted_defaults() {
    for field in ["stream", "include"] {
        let mut request = json!({"model":"test","input":"hello"});
        request[field] = Value::Null;
        let translated = normalize(request);
        assert!(!translated.request.stream);
        assert!(translated.request.stream_options.is_none());
    }
}

#[test]
fn nullable_function_fields_preserve_a_no_argument_tool() {
    let translated = normalize(json!({
        "model":"test","input":"hello",
        "tools":[{"type":"function","name":"list_files",
            "parameters":null,"strict":null,"description":null}]
    }));
    let payload = openai_to_kiro(
        &translated.request,
        &TranslationOptions::new("test", "AI_EDITOR"),
    );
    let tool = payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .unwrap()
        .tools[0]
        .specification()
        .unwrap();
    assert_eq!(tool.name, "list_files");
    assert_eq!(
        tool.input_schema.json,
        json!({"type":"object","properties":{}})
    );
}

#[test]
fn in_memory_prompt_cache_retention_is_accepted() {
    for retention in ["in_memory", "in-memory"] {
        normalize(json!({"model":"test","input":"hello","prompt_cache_retention":retention}));
    }
}

#[test]
fn unsupported_execution_controls_are_rejected_explicitly() {
    for (field, value) in [
        ("store", json!(true)),
        ("background", json!(true)),
        ("previous_response_id", json!("resp_unknown")),
        ("conversation", json!("conv_unknown")),
        ("truncation", json!("auto")),
        ("max_output_tokens", json!(0)),
        ("max_tool_calls", json!(2)),
        ("context_management", json!([])),
        ("tools", json!([{"type":"web_search"}])),
        (
            "tools",
            json!([{"type":"function","name":"deferred","defer_loading":true}]),
        ),
        (
            "tools",
            json!([{"type":"function","name":"same"},{"type":"custom","name":"same"}]),
        ),
    ] {
        let mut value_request = json!({"model":"test","input":"hello"});
        value_request[field] = value;
        let request = serde_json::from_value(value_request).expect("request JSON");
        assert!(
            responses_to_openai(&request).is_err(),
            "unsupported field: {field}"
        );
    }
}

#[test]
fn unsupported_or_broken_history_is_never_silently_dropped() {
    for input in [
        json!([]),
        json!(null),
        json!({"role":"user"}),
        json!([{"role":"user"}]),
        json!([{"type":123,"role":"user","content":"hello"}]),
        json!([
            {"type":"custom_tool_call","call_id":"call","name":"run","input":"text"},
            {"type":"function_call_output","call_id":"call","output":"wrong kind"}
        ]),
        json!([{"type":"function_call_output","call_id":"missing","output":"ok"}]),
        json!([{"type":"function_call","call_id":"pending","name":"run","arguments":"{}"}]),
        json!([{"type":"reasoning","summary":[],"encrypted_content":"opaque"}]),
        json!([{"type":"item_reference","id":"item_old"}]),
        json!([{"role":"user","content":[{"type":"input_file","file_id":"file_unknown"}]}]),
        json!([{"role":"assistant","content":[{"type":"input_image","image_url":"https://example.com/a.png"}]}]),
        json!([{"type":"unknown_future_action","payload":"do something"}]),
    ] {
        let request = serde_json::from_value(json!({"model":"test","input":input})).unwrap();
        assert!(
            responses_to_openai(&request).is_err(),
            "invalid history: {input}"
        );
    }
}

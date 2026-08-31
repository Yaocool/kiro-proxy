use super::*;
use crate::{ClaudeMessage, ClaudeTool};

fn request() -> ClaudeRequest {
    ClaudeRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![ClaudeMessage {
            role: "user".into(),
            content: Value::String("hi".into()),
        }],
        max_tokens: 100,
        temperature: None,
        top_p: None,
        stop_sequences: vec![],
        stream: false,
        system: None,
        tools: vec![],
        tool_choice: None,
        thinking: None,
        context_management: None,
    }
}

fn tool(index: usize) -> ClaudeTool {
    ClaudeTool {
        r#type: None,
        name: format!("tool_{index}"),
        description: String::new(),
        input_schema: serde_json::json!({"type":"object"}),
        cache_control: None,
        strict: None,
        input_examples: None,
        defer_loading: false,
        allowed_callers: None,
        eager_input_streaming: None,
        max_uses: None,
        allowed_domains: None,
        blocked_domains: None,
        user_location: None,
        response_inclusion: None,
        extra: Default::default(),
    }
}

#[test]
fn claude_stop_sequences_must_not_be_empty() {
    let mut input = request();
    input.stop_sequences = vec!["END".into(), String::new()];
    assert_eq!(
        validate_claude(&input),
        Err(ValidationError::InvalidField {
            field: "stop_sequences".into(),
            message: "sequences must not be empty".into(),
        })
    );

    input.stop_sequences = vec!["END".into()];
    validate_claude(&input).expect("non-empty stop sequence");
}

#[test]
fn claude_stop_sequences_are_bounded() {
    let mut input = request();
    input.stop_sequences = vec!["x".into(); MAX_STOP_SEQUENCES + 1];
    assert!(matches!(
        validate_claude(&input),
        Err(ValidationError::InvalidField { field, .. }) if field == "stop_sequences"
    ));

    input.stop_sequences = vec!["x".repeat(MAX_STOP_SEQUENCE_BYTES + 1)];
    assert!(matches!(
        validate_claude(&input),
        Err(ValidationError::InvalidField { field, .. }) if field == "stop_sequences"
    ));

    input.stop_sequences = vec![
        "x".repeat(MAX_STOP_SEQUENCE_TOTAL_BYTES / MAX_STOP_SEQUENCES + 1);
        MAX_STOP_SEQUENCES
    ];
    assert!(matches!(
        validate_claude(&input),
        Err(ValidationError::InvalidField { field, .. }) if field == "stop_sequences"
    ));
}

#[test]
fn enforces_tool_count_boundary() {
    let mut input = request();
    input.tools = (0..MAX_TOOLS).map(tool).collect();
    validate_claude(&input).expect("maximum tool count should be accepted");

    input.tools.push(tool(MAX_TOOLS));
    assert_eq!(validate_claude(&input), Err(ValidationError::TooManyTools));
}

#[test]
fn rejects_excessive_schema_depth() {
    let mut schema = serde_json::json!({});
    for _ in 0..MAX_SCHEMA_DEPTH {
        schema = serde_json::json!({"child": schema});
    }
    let mut input = request();
    input.tools.push(ClaudeTool {
        r#type: None,
        name: "deep".into(),
        description: String::new(),
        input_schema: schema,
        cache_control: None,
        strict: None,
        input_examples: None,
        defer_loading: false,
        allowed_callers: None,
        eager_input_streaming: None,
        max_uses: None,
        allowed_domains: None,
        blocked_domains: None,
        user_location: None,
        response_inclusion: None,
        extra: Default::default(),
    });
    assert_eq!(
        validate_claude(&input),
        Err(ValidationError::InvalidField {
            field: "tools.0.input_schema".into(),
            message: ValidationError::SchemaTooDeep.to_string(),
        })
    );
}

#[test]
fn rejects_a_single_oversized_tool_definition() {
    let mut input = request();
    let mut oversized = tool(0);
    oversized.description = "x".repeat(MAX_TOOL_BYTES);
    input.tools.push(oversized);
    assert_eq!(
        validate_claude(&input),
        Err(ValidationError::ToolDefinitionTooLarge)
    );
}

#[test]
fn rejects_strict_claude_tools_and_invalid_input_examples() {
    let mut input = request();
    input.tools.push(ClaudeTool {
        r#type: Some("custom".into()),
        name: "strict".into(),
        description: String::new(),
        input_schema: serde_json::json!({"type":"object"}),
        cache_control: None,
        strict: Some(true),
        input_examples: Some(vec![Value::String("not an object".into())]),
        defer_loading: false,
        allowed_callers: None,
        eager_input_streaming: None,
        max_uses: None,
        allowed_domains: None,
        blocked_domains: None,
        user_location: None,
        response_inclusion: None,
        extra: Default::default(),
    });
    assert!(validate_claude(&input)
        .expect_err("strict")
        .to_string()
        .contains("strict"));
}

#[test]
fn validates_supported_compaction_configuration() {
    let mut input = request();
    input.context_management = Some(serde_json::json!({"edits":[{
        "type":"compact_20260112",
        "trigger":{"type":"input_tokens","value":75_000},
        "pause_after_compaction":false
    }]}));
    validate_claude(&input).expect("supported compact configuration");

    input.context_management = Some(serde_json::json!({"edits":[{
        "type":"compact_next",
        "trigger":{"type":"input_tokens","value":80_000}
    }]}));
    validate_claude(&input).expect("opaque compact version");

    input.context_management = Some(serde_json::json!({"edits":[{
        "type":"clear_tool_uses_20250919"
    }]}));
    validate_claude(&input).expect("tool-result clearing edit");

    input.context_management = Some(serde_json::json!({"edits":[{
        "type":"clear_thinking_20251015",
        "keep":"all"
    }]}));
    validate_claude(&input).expect("Claude Code clear-thinking edit");

    input.context_management = Some(serde_json::json!({"edits":[{
        "type":"compaction_latest"
    }]}));
    validate_claude(&input).expect("future context edit family");

    input.context_management = Some(serde_json::json!({"edits":[{}]}));
    assert!(validate_claude(&input)
        .expect_err("missing edit type")
        .to_string()
        .contains("expected a string"));
}

#[test]
fn validates_assistant_compaction_content_contract() {
    let mut input = request();
    input.messages[0].role = "assistant".into();
    input.messages[0].content = serde_json::json!([{
        "type":"compaction","content":null
    }]);
    validate_claude(&input).expect("null assistant compaction is a protocol no-op");

    input.messages[0].content = serde_json::json!([{"type":"compaction"}]);
    validate_claude(&input).expect("missing assistant compaction content is a protocol no-op");

    input.messages[0].role = "user".into();
    let error = validate_claude(&input).expect_err("user compaction must be rejected");
    assert!(error.to_string().contains("require an assistant role"));

    input.messages[0].role = "assistant".into();
    input.messages[0].content = serde_json::json!([{
        "type":"compaction","content":{"summary":"invalid"}
    }]);
    let error = validate_claude(&input).expect_err("object content must be rejected");
    assert!(error.to_string().contains("expected a string or null"));

    input.messages[0].content = serde_json::json!([{
        "type":"compaction","content":""
    }]);
    let error = validate_claude(&input).expect_err("empty content must be rejected");
    assert!(error.to_string().contains("must not be empty"));
}

#[test]
fn validates_nested_tool_result_images_and_rejects_unsupported_blocks() {
    let mut input = request();
    input.messages[0].content = serde_json::json!([{
        "type":"tool_result","tool_use_id":"tool_1","content":[{
            "type":"image","source":{
                "type":"base64","media_type":"image/png","data":"aGVsbG8="
            }
        }]
    }]);
    validate_claude(&input).expect("nested image");

    input.messages[0].content = serde_json::json!([{
        "type":"document","source":{"type":"base64","data":"aGVsbG8="}
    }]);
    assert!(validate_claude(&input)
        .expect_err("document")
        .to_string()
        .contains("not supported"));

    input.messages[0].content = serde_json::json!([{"type":"future_block"}]);
    assert!(validate_claude(&input)
        .expect_err("unknown block")
        .to_string()
        .contains("unsupported Claude content block"));
}

#[test]
fn validates_anthropic_tool_search_contract() {
    let mut input = request();
    input.tools = vec![
        ClaudeTool {
            r#type: Some("tool_search_tool_regex_20251119".into()),
            name: "tool_search_tool_regex".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            cache_control: None,
            strict: None,
            input_examples: None,
            defer_loading: false,
            allowed_callers: None,
            eager_input_streaming: None,
            max_uses: None,
            allowed_domains: None,
            blocked_domains: None,
            user_location: None,
            response_inclusion: None,
            extra: Default::default(),
        },
        ClaudeTool {
            defer_loading: true,
            ..tool(1)
        },
    ];
    input.messages = vec![
        ClaudeMessage {
            role: "assistant".into(),
            content: serde_json::json!([
                {"type":"server_tool_use","id":"srvtoolu_1","name":"tool_search_tool_regex","input":{"pattern":"issue"}},
                {"type":"tool_search_tool_result","tool_use_id":"srvtoolu_1","content":{
                    "type":"tool_search_tool_search_result",
                    "tool_references":[{"type":"tool_reference","tool_name":"tool_1"}]
                }}
            ]),
        },
        ClaudeMessage {
            role: "user".into(),
            content: Value::String("continue".into()),
        },
    ];
    validate_claude(&input).expect("official Tool Search request");

    input.tools[1].cache_control = Some(serde_json::json!({"type":"ephemeral"}));
    assert!(validate_claude(&input)
        .expect_err("deferred cache control")
        .to_string()
        .contains("cache_control"));
    input.tools[1].cache_control = None;

    input.tools.remove(0);
    assert!(validate_claude(&input)
        .expect_err("missing search tool")
        .to_string()
        .contains("defer_loading=false"));
}

#[test]
fn rejects_unemulated_official_tool_execution_contracts_explicitly() {
    let mut input: ClaudeRequest = serde_json::from_value(serde_json::json!({
        "model":"claude-sonnet-4",
        "max_tokens":100,
        "messages":[{"role":"user","content":"hi"}],
        "tools":[{"type":"mcp_toolset","mcp_server_name":"github"}]
    }))
    .expect("parse mcp_toolset");
    assert!(validate_claude(&input)
        .expect_err("mcp_toolset")
        .to_string()
        .contains("mcp_toolset"));

    input = serde_json::from_value(serde_json::json!({
        "model":"claude-sonnet-4",
        "max_tokens":100,
        "messages":[{"role":"user","content":"latest"}],
        "tools":[{"type":"web_search_20260209","name":"web_search"}]
    }))
    .expect("parse dynamic web search");
    assert!(validate_claude(&input)
        .expect_err("dynamic caller default")
        .to_string()
        .contains("allowed_callers"));
    input.tools[0].allowed_callers = Some(vec!["direct".into()]);
    validate_claude(&input).expect("explicit direct caller");
    input.tools[0].r#type = Some("web_search_next".into());
    input.tools[0].response_inclusion = Some("full".into());
    validate_claude(&input).expect("opaque web search version");
    input.tools[0].allowed_domains = Some(vec!["example.com".into()]);
    assert!(validate_claude(&input)
        .expect_err("domain filter")
        .to_string()
        .contains("domain filters"));

    input = serde_json::from_value(serde_json::json!({
        "model":"claude-sonnet-4",
        "max_tokens":100,
        "messages":[{"role":"user","content":"fetch"}],
        "tools":[{"type":"web_fetch_20260318","name":"web_fetch"}]
    }))
    .expect("parse web fetch");
    assert!(validate_claude(&input)
        .expect_err("web fetch must not be exposed as a client tool")
        .to_string()
        .contains("Web Fetch is not implemented"));

    input = serde_json::from_value(serde_json::json!({
        "model":"claude-sonnet-4",
        "max_tokens":100,
        "messages":[{"role":"user","content":"find a tool"}],
        "tools":[{
            "type":"tool_search_tool_regex_next",
            "name":"tool_search_tool_regex"
        }]
    }))
    .expect("parse future search version");
    validate_claude(&input).expect("opaque Tool Search version");

    input.tools[0].r#type = Some("tool_search_tool_vector_next".into());
    assert!(validate_claude(&input)
        .expect_err("unknown search algorithm")
        .to_string()
        .contains("unsupported Claude Tool Search version"));

    input.tools[0].r#type = Some("web_searcher_next".into());
    input.tools[0].name = "web_search".into();
    assert!(validate_claude(&input)
        .expect_err("similar-looking server tool")
        .to_string()
        .contains("unsupported Claude server tool"));
}

#[test]
fn rejects_unknown_tool_controls_instead_of_silently_dropping_them() {
    let input: ClaudeRequest = serde_json::from_value(serde_json::json!({
        "model":"claude-sonnet-4",
        "max_tokens":100,
        "messages":[{"role":"user","content":"hi"}],
        "tools":[{
            "name":"controlled_tool",
            "description":"test",
            "input_schema":{"type":"object"},
            "future_execution_policy":"sandbox_only"
        }]
    }))
    .expect("parse future tool control");

    let error = validate_claude(&input).expect_err("unknown controls must be rejected");
    assert!(error.to_string().contains("future_execution_policy"));
    assert!(error.to_string().contains("cannot be safely ignored"));
}

#[test]
fn accepts_official_web_search_history_blocks() {
    let mut input = request();
    input.messages = vec![
        ClaudeMessage {
            role: "assistant".into(),
            content: serde_json::json!([
                {"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"rust"}},
                {"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{
                    "type":"web_search_result","url":"https://example.com","title":"Example",
                    "encrypted_content":"opaque","page_age":null
                }]}
            ]),
        },
        ClaudeMessage {
            role: "user".into(),
            content: Value::String("continue".into()),
        },
    ];
    validate_claude(&input).expect("web search history");

    input.messages[0].content[1]["content"][0]
        .as_object_mut()
        .expect("result")
        .remove("encrypted_content");
    assert!(validate_claude(&input)
        .expect_err("replay content is required")
        .to_string()
        .contains("encrypted_content"));
}

#[test]
fn custom_search_history_uses_ordinary_tool_result_references() {
    let mut input: ClaudeRequest = serde_json::from_value(serde_json::json!({
        "model":"claude-sonnet-4",
        "max_tokens":100,
        "messages":[
            {"role":"assistant","content":[{
                "type":"tool_use","id":"toolu_search","name":"custom_search",
                "input":{"query":"issues"}
            }]},
            {"role":"user","content":[{
                "type":"tool_result","tool_use_id":"toolu_search","content":[{
                    "type":"tool_reference","tool_name":"mcp__github__issues"
                }]
            }]}
        ],
        "tools":[
            {"name":"custom_search","input_schema":{"type":"object"}},
            {"name":"mcp__github__issues","input_schema":{"type":"object"},"defer_loading":true}
        ]
    }))
    .expect("custom search request");

    validate_claude(&input).expect("ordinary custom tool result format");

    input.messages[1].content[0]["content"][0]["tool_name"] = Value::String("missing_tool".into());
    assert!(validate_claude(&input)
        .expect_err("references must resolve against the catalog")
        .to_string()
        .contains("not defined in the top-level tools array"));
}

#[test]
fn server_tool_results_must_match_unique_server_calls() {
    let valid: ClaudeRequest = serde_json::from_value(serde_json::json!({
        "model":"claude-sonnet-4",
        "max_tokens":100,
        "messages":[
            {"role":"assistant","content":[
                {"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"rust"}},
                {"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","caller":{"type":"direct"},"content":[]}
            ]},
            {"role":"user","content":"continue"}
        ],
        "tools":[{"type":"web_search_20250305","name":"web_search"}]
    }))
    .expect("valid server history");
    validate_claude(&valid).expect("paired server result");

    let mut duplicate = valid.clone();
    duplicate.messages[0].content[1]["tool_use_id"] = Value::String("srvtoolu_2".into());
    assert!(validate_claude(&duplicate)
        .expect_err("orphan result")
        .to_string()
        .contains("no matching server_tool_use"));

    let ordinary_result: ClaudeRequest = serde_json::from_value(serde_json::json!({
        "model":"claude-sonnet-4",
        "max_tokens":100,
        "messages":[
            {"role":"assistant","content":[
                {"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"rust"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"srvtoolu_1","content":"not allowed"}
            ]}
        ],
        "tools":[{"type":"web_search_20250305","name":"web_search"}]
    }))
    .expect("ordinary result request");
    assert!(validate_claude(&ordinary_result)
        .expect_err("client result cannot complete a server call")
        .to_string()
        .contains("not tool_result"));

    let mut result_before_call = valid.clone();
    result_before_call.messages[0]
        .content
        .as_array_mut()
        .expect("blocks")
        .swap(0, 1);
    assert!(validate_claude(&result_before_call)
        .expect_err("server results cannot precede their call")
        .to_string()
        .contains("appears before"));

    let mut duplicate_id = valid;
    duplicate_id.messages[0]
        .content
        .as_array_mut()
        .expect("blocks")
        .insert(
            1,
            serde_json::json!({
                "type":"server_tool_use","id":"srvtoolu_1","name":"web_search",
                "input":{"query":"again"}
            }),
        );
    assert!(validate_claude(&duplicate_id)
        .expect_err("duplicate server id")
        .to_string()
        .contains("already defined"));
}

#[test]
fn enforces_the_official_tool_name_length() {
    let mut input = request();
    let mut definition = tool(0);
    definition.name = "a".repeat(MAX_TOOL_NAME_CHARS + 1);
    input.tools.push(definition);
    assert_eq!(
        validate_claude(&input),
        Err(ValidationError::ToolNameTooLong)
    );
}

#[test]
fn openai_business_fields_and_tool_call_json_are_validated() {
    let mut input: OpenAiRequest = serde_json::from_value(serde_json::json!({
        "model":"claude-sonnet-4",
        "messages":[{"role":"assistant","tool_calls":[{
            "id":"call_1","type":"function",
            "function":{"name":"lookup","arguments":"not-json"}
        }]}],
        "temperature":1.0
    }))
    .expect("request");
    assert!(validate_openai(&input)
        .expect_err("arguments")
        .to_string()
        .contains("invalid JSON"));
    input.messages = vec![crate::OpenAiMessage {
        role: "user".into(),
        content: Some(Value::String("hi".into())),
        tool_calls: Vec::new(),
        tool_call_id: None,
    }];
    input.max_tokens = Some(1);
    input.max_completion_tokens = Some(1);
    assert!(validate_openai(&input)
        .expect_err("exclusive limits")
        .to_string()
        .contains("either"));
}

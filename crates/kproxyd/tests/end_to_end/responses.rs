use super::*;
use serde_json::{json, Value};

const CODEX_AGENT: &str = "codex_cli_rs/0.147.0 (Mac OS 26.0; arm64) Terminal/1.0";

#[derive(Clone)]
struct ToolRoundtrip;

impl wiremock::Respond for ToolRoundtrip {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        let context = &payload["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"];
        let results = context["toolResults"].as_array().map_or(0, Vec::len);
        let body = if results > 0 {
            generation_body("Read the file and applied the patch.")
        } else {
            let names = context["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["toolSpecification"]["name"].as_str())
                .collect::<Vec<_>>();
            assert_eq!(names.len(), 2);
            let mut body = event_stream_frame(
                "assistantResponseEvent",
                json!({"content":"Inspecting the file. "}),
            );
            body.extend(event_stream_frame(
                "toolUseEvent",
                json!({"toolUseId":"call_read","name":names[0],"input":"{\"path\":","stop":false}),
            ));
            body.extend(event_stream_frame("toolUseEvent", json!({"toolUseId":"call_patch","name":names[1],"input":"{\"input\":","stop":false})));
            // Kiro continuation fragments can omit the name supplied at the
            // start of the call, including when parallel calls are interleaved.
            body.extend(event_stream_frame(
                "toolUseEvent",
                json!({"toolUseId":"call_read","input":"\"README.md\"}","stop":true}),
            ));
            body.extend(event_stream_frame("toolUseEvent", json!({"toolUseId":"call_patch","inputDelta":"\"*** Begin Patch\\n*** End Patch\"}","stop":true})));
            body.extend(event_stream_frame("messageMetadataEvent", json!({"messageMetadataEvent":{"usage":{"inputTokens":100,"outputTokens":20,"creditsConsumed":0.25}}})));
            body
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(body)
    }
}

async fn create_response_with_session(
    client: &reqwest::Client,
    daemon: &Daemon,
    port: u16,
    path: &str,
    session_id: Option<&str>,
    request: &Value,
) -> Value {
    let mut builder = client
        .post(format!("http://127.0.0.1:{port}{path}"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .header("user-agent", CODEX_AGENT);
    if let Some(session_id) = session_id {
        builder = builder.header("session-id", session_id);
    }
    let response = builder.json(request).send().await.unwrap();
    let status = response.status();
    assert!(response.headers().contains_key("request-id"));
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    if request["stream"] == true {
        assert!(content_type.starts_with("text/event-stream"));
        assert!(!body.contains("chat.completion.chunk"));
        assert!(!body.contains("[DONE]"));
        let events = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], index);
        }
        assert_eq!(events[0]["type"], "response.created");
        let completed = events.last().unwrap();
        assert_eq!(completed["type"], "response.completed", "{body}");
        assert_eq!(events[0]["response"]["id"], completed["response"]["id"]);
        for (index, item) in completed["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let done = events
                .iter()
                .find(|event| {
                    event["type"] == "response.output_item.done" && event["output_index"] == index
                })
                .unwrap();
            assert_eq!(done["item"], *item);
        }
        completed["response"].clone()
    } else {
        assert!(content_type.starts_with("application/json"));
        serde_json::from_str(&body).unwrap()
    }
}

async fn create_response(
    client: &reqwest::Client,
    daemon: &Daemon,
    port: u16,
    path: &str,
    request: &Value,
) -> Value {
    create_response_with_session(
        client,
        daemon,
        port,
        path,
        Some("codex-response-e2e"),
        request,
    )
    .await
}

#[tokio::test]
async fn responses_tool_roundtrip_works_through_both_aliases_and_buffer_modes() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(ToolRoundtrip)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();

    // Authentication and client filtering must run before spending upstream credits.
    for (agent, key, expected) in [
        (
            "claude-cli/2.1.235 (external, test)",
            daemon.api_key.as_deref().unwrap(),
            reqwest::StatusCode::BAD_REQUEST,
        ),
        (
            CODEX_AGENT,
            "invalid-test-key",
            reqwest::StatusCode::UNAUTHORIZED,
        ),
    ] {
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/responses"))
            .header("user-agent", agent)
            .bearer_auth(key)
            .json(&json!({"model":"source-large","input":"hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }

    for buffered in [false, true] {
        let config_path = daemon.home().join("config.toml");
        let mut config: kproxy_core::config::Config =
            toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
        config.features.buffer_tool_calls = buffered;
        config.features.auto_continue_rounds = 0;
        tokio::fs::write(&config_path, toml::to_string_pretty(&config).unwrap())
            .await
            .unwrap();
        assert_eq!(
            expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
            true
        );
        for stream in [false, true] {
            for endpoint in ["/v1/responses", "/responses"] {
                let initial_input = json!({"role":"user","content":[{"type":"input_text","text":"Inspect the file."}]});
                let additional_tools = json!({"type":"additional_tools","role":"developer","tools":[
                    {"type":"namespace","name":"functions","tools":[
                        {"type":"function","name":"read_file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}},
                        {"type":"custom","name":"apply_patch","format":{"type":"text"}}
                    ]}
                ]});
                let mut request = json!({
                    "model":"source-large","instructions":"Respect the repository rules.",
                    "input":[additional_tools.clone(),initial_input.clone()],
                    "stream":stream,"store":false,"max_output_tokens":512
                });
                let first = create_response(&client, &daemon, port, endpoint, &request).await;
                assert_eq!(first["object"], "response");
                assert_eq!(first["status"], "completed");
                assert_eq!(first["model"], "source-large");
                assert_eq!(first["usage"]["input_tokens"], 100);
                assert_eq!(first["usage"]["output_tokens"], 20);
                assert!(first["id"].as_str().unwrap().starts_with("resp_"));
                assert_eq!(first["tools"].as_array().map(Vec::len), Some(1));
                let output = first["output"].as_array().unwrap();
                let message = output
                    .iter()
                    .find(|item| item["type"] == "message")
                    .unwrap();
                assert_eq!(message["content"][0]["text"], "Inspecting the file. ");
                let function = output
                    .iter()
                    .find(|item| item["type"] == "function_call")
                    .unwrap();
                assert_eq!(function["call_id"], "call_read");
                assert_eq!(function["name"], "read_file");
                assert_eq!(function["namespace"], "functions");
                assert_eq!(
                    serde_json::from_str::<Value>(function["arguments"].as_str().unwrap()).unwrap(),
                    json!({"path":"README.md"})
                );
                let custom = output
                    .iter()
                    .find(|item| item["type"] == "custom_tool_call")
                    .unwrap_or_else(|| panic!("missing custom tool: buffered={buffered}, stream={stream}, endpoint={endpoint}, response={first}"));
                assert_eq!(custom["name"], "apply_patch");
                assert_eq!(custom["namespace"], "functions");
                assert_eq!(custom["input"], "*** Begin Patch\n*** End Patch");

                let mut replay = vec![additional_tools, initial_input];
                replay.extend(output.iter().cloned());
                replay.push(json!({"type":"function_call_output","call_id":"call_read","output":"README contents"}));
                replay.push(json!({"type":"custom_tool_call_output","call_id":"call_patch","output":[{"type":"input_text","text":"Patch applied."}]}));
                request["input"] = json!(replay);
                let second = create_response(&client, &daemon, port, endpoint, &request).await;
                assert_eq!(
                    second["output"][0]["content"][0]["text"],
                    "Read the file and applied the patch."
                );
                assert_ne!(first["id"], second["id"]);
            }
        }
    }

    let received = mock.received_requests().await.unwrap();
    let payloads = received
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 16);
    for pair in payloads.chunks_exact(2) {
        assert_eq!(pair[0]["inferenceConfig"]["maxTokens"], 512);
        assert_eq!(
            pair[1]["conversationState"]["conversationId"],
            pair[0]["conversationState"]["conversationId"]
        );
        let results = pair[1]["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["toolResults"]
            .as_array()
            .unwrap();
        assert_eq!(results.len(), 2);
        let read = results
            .iter()
            .find(|result| result["toolUseId"] == "call_read")
            .unwrap();
        let patch = results
            .iter()
            .find(|result| result["toolUseId"] == "call_patch")
            .unwrap();
        assert_eq!(read["content"][0]["text"], "README contents");
        assert_eq!(patch["content"][0]["text"], "Patch applied.");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn responses_default_store_preserves_affinity_and_resumes_tool_roundtrips() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(ToolRoundtrip)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();

    for (index, stream) in [false, true].into_iter().enumerate() {
        let initial = json!({
            "model":"source-large",
            "instructions":"Follow the repository rules.",
            "input":"Inspect the file.",
            "stream":stream,
            "tools":[{"type":"namespace","name":"functions","tools":[
                {"type":"function","name":"read_file","parameters":{"type":"object"}},
                {"type":"custom","name":"apply_patch"}
            ]}],
            "tool_choice":{"type":"allowed_tools","mode":"required","tools":[
                {"type":"function","name":"functions.read_file"},
                {"type":"custom","name":"functions.apply_patch"}
            ]}
        });
        let first = create_response_with_session(
            &client,
            &daemon,
            port,
            "/v1/responses",
            Some("responses-parent"),
            &initial,
        )
        .await;
        let first_id = first["id"].as_str().expect("response id").to_owned();
        assert_eq!(first["store"], true);

        // Only newly supplied items are sent. Omitted `store` defaults to true,
        // and the stored response keeps the resolved Kiro conversation ID even
        // if a continuation drops or changes its affinity header.
        let continuation = json!({
            "model":"source-large",
            "previous_response_id":first_id,
            "input":[
                {"type":"function_call_output","call_id":"call_read","output":"README contents"},
                {"type":"custom_tool_call_output","call_id":"call_patch","output":"Patch applied."}
            ],
            "stream":stream
        });
        let continuation_session = (index == 1).then_some("responses-child");
        let second = create_response_with_session(
            &client,
            &daemon,
            port,
            "/v1/responses",
            continuation_session,
            &continuation,
        )
        .await;
        assert_eq!(second["previous_response_id"], first_id);
        assert_eq!(second["store"], true);
        assert_eq!(
            second["output"][0]["content"][0]["text"],
            "Read the file and applied the patch."
        );
    }

    let payloads = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 4);
    for pair in payloads.chunks_exact(2) {
        assert_eq!(
            pair[1]["conversationState"]["conversationId"],
            pair[0]["conversationState"]["conversationId"]
        );
        let context = &pair[1]["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"];
        assert_eq!(context["tools"].as_array().unwrap().len(), 2);
        assert_eq!(context["toolResults"].as_array().unwrap().len(), 2);
    }
    daemon.stop().await;
}

#[tokio::test]
async fn responses_upstream_stream_errors_remain_failed_events() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let mut body = event_stream_frame(
        "assistantResponseEvent",
        json!({"content":"partial answer"}),
    );
    body.extend(event_stream_frame(
        "toolUseEvent",
        json!({"toolUseId":"broken","name":"read_file","input":"{broken JSON","stop":true}),
    ));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body),
        )
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let response = reqwest::Client::new().post(format!("http://127.0.0.1:{port}/v1/responses"))
        .bearer_auth(daemon.api_key.as_deref().unwrap()).header("user-agent", CODEX_AGENT)
        .json(&json!({"model":"source-large","input":"read the file","stream":true,"tools":[{"type":"function","name":"read_file","parameters":{"type":"object"}}]}))
        .send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("event: response.failed\n"), "{body}");
    assert!(!body.contains("event: response.completed\n"), "{body}");
    assert!(!body.contains("[DONE]"), "{body}");
    daemon.stop().await;
}

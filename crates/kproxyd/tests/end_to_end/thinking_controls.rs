use super::*;
use serde_json::{json, Value};

#[tokio::test]
async fn haiku_without_native_controls_succeeds_while_supported_models_keep_thinking() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[
            {"modelId":"claude-haiku-4.5", "additionalModelRequestFieldsSchema":null},
            {"modelId":"claude-sonnet-4.6", "additionalModelRequestFieldsSchema":{
                "type":"object", "properties":{
                    "thinking":{"type":"object","properties":{
                        "type":{"enum":["adaptive","disabled"]},
                        "display":{"enum":["summarized","omitted"]}
                    }},
                    "output_config":{"properties":{"effort":{"enum":["low","medium","high","max"]}}}
                }
            }}
        ]})))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(|request: &wiremock::Request| {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            let model = payload["conversationState"]["currentMessage"]["userInputMessage"]
                ["modelId"]
                .as_str()
                .unwrap();
            if model == "claude-haiku-4.5" && payload.get("additionalModelRequestFields").is_some()
            {
                // Match the real AmazonQ rejection, including null/empty objects.
                return ResponseTemplate::new(400).set_body_json(json!({
                    "message":"additionalModelRequestFields is not supported for this model",
                    "reason":"REQUEST_BODY_INVALID"
                }));
            }
            if model == "claude-sonnet-4.6" {
                assert_eq!(
                    payload["additionalModelRequestFields"]["thinking"]["type"],
                    "adaptive"
                );
                assert_eq!(
                    payload["additionalModelRequestFields"]["output_config"]["effort"],
                    "low"
                );
            }
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("pong"))
        })
        .expect(12)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_55555555", "email":"thinking@example.com", "machine_id":"5".repeat(64),
        "credentials":{"access_token":"thinking-test-token", "region":"us-east-1",
            "expires_at":4_000_000_000i64, "auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1}, "created_at":1
    }]})).await);
    assert_eq!(
        expect_ok(daemon.call("models", json!({})).await)
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let client = reqwest::Client::new();
    for model in ["claude-haiku-4-5-20251001", "claude-sonnet-4-6"] {
        for stream in [false, true] {
            for route in ["messages", "chat/completions", "responses"] {
                let mut request = json!({"model":model,"stream":stream});
                if route == "responses" {
                    request["input"] = json!("Reply pong only");
                    request["max_output_tokens"] = json!(4096);
                    request["reasoning"] = json!({"effort":"low"});
                } else {
                    request["messages"] = json!([{"role":"user","content":"Reply pong only"}]);
                    request["max_tokens"] = json!(4096);
                    request["thinking"] = json!({"type":"enabled","budget_tokens":1024});
                }
                let user_agent = if route == "messages" {
                    "claude-cli/2.1.235 (external, e2e)"
                } else {
                    "codex_cli_rs/0.1.0"
                };
                let response = client
                    .post(format!("http://127.0.0.1:{port}/v1/{route}"))
                    .bearer_auth(daemon.api_key.as_deref().unwrap())
                    .header("x-api-key", daemon.api_key.as_deref().unwrap())
                    .header("anthropic-version", "2023-06-01")
                    .header("user-agent", user_agent)
                    .json(&request)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let body = response.text().await.unwrap();
                assert_eq!(
                    status,
                    reqwest::StatusCode::OK,
                    "{route} {model} stream={stream}: {body}"
                );
                assert!(body.contains("pong"), "{body}");
                assert!(!body.contains("upstream_unavailable"), "{body}");
            }
        }
    }
    let requests = mock.received_requests().await.unwrap();
    let generations: Vec<Value> = requests
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(
        generations.len(),
        12,
        "No field-removal retries should be needed"
    );
    for (index, payload) in generations.iter().enumerate() {
        let expected_model = if index < 6 {
            "claude-haiku-4.5"
        } else {
            "claude-sonnet-4.6"
        };
        assert_eq!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            expected_model
        );
        assert_eq!(
            payload.get("additionalModelRequestFields").is_some(),
            index >= 6
        );
    }
    daemon.stop().await;
}

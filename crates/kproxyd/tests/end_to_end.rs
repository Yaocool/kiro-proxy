//! 端到端测试：拉起真实 kproxyd，通过 Unix socket 走完整协议。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kproxy_ipc::protocol::{decode_line, encode_line, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static HTTP_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Daemon {
    child: Child,
    socket: PathBuf,
    home: tempfile::TempDir,
    api_key: Option<String>,
}

impl Daemon {
    async fn start() -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let socket = home.path().join("admin.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_kproxyd"))
            .env("KPROXY_HOME", home.path())
            .env("KPROXY_HTTP_PORT", "0")
            .env("KPROXY_DISABLE_HTTP", "1")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn kproxyd");
        wait_for_socket(&socket).await;
        Self {
            child,
            socket,
            home,
            api_key: None,
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> Response {
        rpc_call(&self.socket, method, params).await
    }

    async fn start_http(port: u16, upstream_url: &str) -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let socket = home.path().join("admin.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_kproxyd"))
            .env("KPROXY_HOME", home.path())
            .env("KPROXY_CODEWHISPERER_URL", upstream_url)
            .env("KPROXY_AMAZONQ_URL", upstream_url)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn kproxyd with HTTP");
        wait_for_socket(&socket).await;
        let created = expect_ok(
            rpc_call(
                &socket,
                "service.create",
                serde_json::json!({"name":"e2e","host":"127.0.0.1","port":port}),
            )
            .await,
        );
        let api_key = created["api_key"]["key"]
            .as_str()
            .expect("created API key")
            .to_string();
        wait_for_http(port).await;
        Self {
            child,
            socket,
            home,
            api_key: Some(api_key),
        }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    async fn stop(mut self) {
        let _kill_result = self.child.kill().await;
        let _wait_result = self.child.wait().await;
    }
}

async fn rpc_call(socket: &Path, method: &str, params: serde_json::Value) -> Response {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let (read_half, mut write_half) = stream.into_split();
    let line = encode_line(&Request::new(1, method, params)).expect("encode");
    write_half.write_all(line.as_bytes()).await.expect("write");
    write_half.flush().await.expect("flush");
    let raw = BufReader::new(read_half)
        .lines()
        .next_line()
        .await
        .expect("read")
        .expect("response");
    decode_line(&raw).expect("decode")
}

async fn wait_for_socket(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if socket.exists() && UnixStream::connect(socket).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "kproxyd did not start: {}",
            socket.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_http(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let client = reqwest::Client::new();
    loop {
        if client
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "kproxyd HTTP plane did not start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn expect_ok(response: Response) -> serde_json::Value {
    match response {
        Response::Ok { result, .. } => result,
        Response::Err { error, .. } => panic!("RPC failed {}: {}", error.code, error.message),
    }
}

fn unused_tcp_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local address").port()
}

fn event_stream_frame(event_type: &str, payload: serde_json::Value) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [
        (":message-type", "event"),
        (":event-type", event_type),
        (":content-type", "application/json"),
    ] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = serde_json::to_vec(&payload).expect("serialize event payload");
    let total_length = 16 + headers.len() + payload.len();
    let mut frame = Vec::with_capacity(total_length);
    frame.extend_from_slice(&(total_length as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame
}

fn generation_body(content: &str) -> Vec<u8> {
    let mut body = event_stream_frame(
        "assistantResponseEvent",
        serde_json::json!({"content":content}),
    );
    body.extend(event_stream_frame(
        "messageMetadataEvent",
        serde_json::json!({
            "messageMetadataEvent": {
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 20,
                    "creditsConsumed": 0.25
                }
            }
        }),
    ));
    body
}

#[derive(Clone)]
struct CompactionResponder;

impl wiremock::Respond for CompactionResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        let content = if current.contains("durable conversation checkpoint") {
            "<summary>Task Overview: migrate compact to semantic Kiro summaries. Current State: implementation is active. Next Steps: verify the main response.</summary>"
        } else {
            "main request completed"
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(generation_body(content))
    }
}

#[derive(Clone)]
struct LongCompactionResponder;

impl wiremock::Respond for LongCompactionResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        let content = if current.contains("durable conversation checkpoint") {
            format!(
                "<summary>{}</summary>",
                "durable checkpoint detail ".repeat(2_000)
            )
        } else {
            "main request completed".into()
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(generation_body(&content))
    }
}

#[derive(Clone)]
struct SlowCompactionResponder;

impl wiremock::Respond for SlowCompactionResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        let summary = current.contains("durable conversation checkpoint");
        let content = if summary {
            "<summary>Task Overview: finish accounting after the caller timeout.</summary>"
        } else {
            "main request completed while summary accounting continued"
        };
        let response = ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(generation_body(content));
        if summary {
            response.set_delay(Duration::from_millis(100))
        } else {
            response
        }
    }
}

#[derive(Clone)]
struct PartialFailureCompactionResponder;

impl wiremock::Respond for PartialFailureCompactionResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        let body = if current.contains("durable conversation checkpoint") {
            let mut body = generation_body(
                "<summary>This summary is discarded because the stream ends malformed.</summary>",
            );
            // Preserve two complete events (including authoritative usage),
            // then force decode_eof to report a truncated trailing frame.
            body.extend_from_slice(&[0, 0, 0, 32, 0, 0, 0, 0]);
            body
        } else {
            generation_body("main request completed after the failed summary was accounted")
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(body)
    }
}

#[derive(Clone, Default)]
struct RetryingCompactionStreamResponder {
    main_calls: Arc<AtomicUsize>,
}

impl wiremock::Respond for RetryingCompactionStreamResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        let body = if current.contains("durable conversation checkpoint") {
            generation_body(
                "<summary>Task Overview: preserve the compacted stream across a retry.</summary>",
            )
        } else if self.main_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            event_stream_frame(
                "toolUseEvent",
                serde_json::json!({
                    "toolUseEvent": {
                        "toolUseId":"broken-write",
                        "name":"write_file",
                        "input":"{\"path\":",
                        "stop":false
                    }
                }),
            )
        } else {
            generation_body("main request completed after retry")
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(body)
    }
}

#[derive(Clone, Default)]
struct ModelUnavailableFallbackResponder;

impl wiremock::Respond for ModelUnavailableFallbackResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let model = payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"]
            .as_str()
            .unwrap_or_default();
        if model == "claude-opus-5" {
            ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "message": "Encountered unexpectedly high load when processing the request, please try again.",
                "reason": "MODEL_TEMPORARILY_UNAVAILABLE"
            }))
        } else {
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("completed with the fallback model"))
        }
    }
}

async fn mount_context_alignment_models(mock: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models":[
                {
                    "modelId":"source-large",
                    "tokenLimits":{"maxInputTokens":1000000,"maxOutputTokens":16384}
                },
                {
                    "modelId":"mapped-small",
                    "tokenLimits":{"maxInputTokens":128000,"maxOutputTokens":16384}
                },
                {
                    "modelId":"resolved-small",
                    "tokenLimits":{"maxInputTokens":64000,"maxOutputTokens":16384}
                },
                {
                    "modelId":"resolved-tiny",
                    "tokenLimits":{"maxInputTokens":500,"maxOutputTokens":16384}
                },
                {
                    "modelId":"summary-large",
                    "tokenLimits":{"maxInputTokens":1000000,"maxOutputTokens":16384}
                }
            ]
        })))
        .mount(mock)
        .await;
}

async fn import_context_alignment_account(daemon: &Daemon, used_credits: f64) {
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_99999999",
                            "email": "alignment@example.com",
                            "machine_id": "9".repeat(64),
                            "credentials": {
                                "access_token": "alignment-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": used_credits,
                                "limit": 100.0,
                                "percent_used": used_credits,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );
    let models = expect_ok(daemon.call("models", serde_json::json!({})).await);
    assert_eq!(models.as_array().map(Vec::len), Some(5));
}

async fn configure_context_alignment(daemon: &Daemon, summary_model: &str, mapping: &str) {
    let config_path = daemon.home().join("config.toml");
    let raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config");
    let edited = raw
        .replace("model_mapping = []", "")
        .replace(
            "auto_compact_on_overflow = false",
            "auto_compact_on_overflow = true",
        )
        .replace(
            "compaction_summary_model = \"\"",
            &format!("compaction_summary_model = \"{summary_model}\""),
        );
    let edited = format!("{edited}\n{mapping}\n");
    toml::from_str::<kproxy_core::config::Config>(&edited).expect("parse edited config");
    tokio::fs::write(&config_path, edited)
        .await
        .expect("write config");
    let reload = expect_ok(daemon.call("config.reload", serde_json::json!({})).await);
    assert_eq!(reload["applied"], true, "config reload failed: {reload}");
    let shown = expect_ok(daemon.call("config.show", serde_json::json!({})).await);
    assert_eq!(
        shown["effective_json"]["context"]["auto_compact_on_overflow"],
        true
    );
    assert!(!shown["effective_json"]["model_mapping"]
        .as_array()
        .expect("model mappings")
        .is_empty());
}

#[tokio::test]
async fn dotenv_is_loaded_before_daemon_startup_configuration() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let home = workspace.path().join("dotenv-home");
    let socket = home.join("admin.sock");
    std::fs::write(
        workspace.path().join(".env"),
        format!(
            "KPROXY_HOME={}\nKPROXY_HTTP_PORT=0\nKPROXY_DISABLE_HTTP=1\nRUST_LOG=warn\n",
            home.display()
        ),
    )
    .expect("write .env");

    let mut child = Command::new(env!("CARGO_BIN_EXE_kproxyd"))
        .current_dir(workspace.path())
        .env_remove("KPROXY_HOME")
        .env_remove("KPROXY_HTTP_PORT")
        .env_remove("KPROXY_DISABLE_HTTP")
        .env_remove("RUST_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn kproxyd from .env");

    wait_for_socket(&socket).await;
    assert!(home.join("config.toml").exists());
    assert!(home.join("accounts.json").exists());

    let _kill_result = child.kill().await;
    let _wait_result = child.wait().await;
}

#[tokio::test]
async fn compact_model_alias_runs_through_translation_pool_and_mock_upstream() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    let mut upstream_body = event_stream_frame(
        "assistantResponseEvent",
        serde_json::json!({"content":"hello through the complete Rust chain"}),
    );
    upstream_body.extend(event_stream_frame(
        "messageMetadataEvent",
        serde_json::json!({
            "messageMetadataEvent": {
                "usage": {
                    "inputTokens": 11,
                    "outputTokens": 7,
                    "creditsConsumed": 0.25
                }
            }
        }),
    ));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(upstream_body),
        )
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_77777777",
                            "email": "wiremock@example.com",
                            "machine_id": "7".repeat(64),
                            "credentials": {
                                "access_token": "wiremock-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": 0.0,
                                "limit": 100.0,
                                "percent_used": 0.0,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(daemon.api_key.as_deref().expect("service API key"))
        .json(&serde_json::json!({
            "model": "opus5",
            "messages": [{"role":"user", "content":"exercise every layer"}],
            "max_tokens": 64,
            "stream": false
        }))
        .send()
        .await
        .expect("call Rust proxy");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("OpenAI response JSON");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello through the complete Rust chain"
    );
    assert_eq!(body["usage"]["prompt_tokens"], 11);
    assert_eq!(body["usage"]["completion_tokens"], 7);

    let received = mock.received_requests().await.expect("received requests");
    assert!(!received.is_empty());
    let generation_payloads = received
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter(|payload| payload.get("conversationState").is_some())
        .collect::<Vec<_>>();
    assert_eq!(generation_payloads.len(), 1);
    for payload in generation_payloads {
        assert_eq!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "claude-opus-5"
        );
        assert!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| content.ends_with("exercise every layer"))
        );
    }
    daemon.stop().await;
}

#[tokio::test]
async fn temporarily_unavailable_model_falls_back_to_lower_same_family_model() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [
                {
                    "modelId": "claude-opus-5",
                    "tokenLimits": {"maxInputTokens": 1000000, "maxOutputTokens": 64000}
                },
                {
                    "modelId": "claude-opus-4.8",
                    "tokenLimits": {"maxInputTokens": 1000000, "maxOutputTokens": 64000}
                }
            ]
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(ModelUnavailableFallbackResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_66666666",
                            "email": "fallback@example.com",
                            "machine_id": "6".repeat(64),
                            "credentials": {
                                "access_token": "fallback-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": 0.0,
                                "limit": 100.0,
                                "percent_used": 0.0,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );
    let models = expect_ok(daemon.call("models", serde_json::json!({})).await);
    assert_eq!(models.as_array().map(Vec::len), Some(2));

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.0 (external, e2e)")
        .json(&serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 64,
            "stream": false,
            "messages": [{"role": "user", "content": "exercise model fallback"}]
        }))
        .send()
        .await
        .expect("call Claude fallback path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(
        body["content"][0]["text"],
        "completed with the fallback model"
    );

    let generated_models = mock
        .received_requests()
        .await
        .expect("received requests")
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter_map(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(generated_models, vec!["claude-opus-5", "claude-opus-4.8"]);

    daemon.stop().await;
}

#[tokio::test]
async fn claude_stream_stops_before_later_frames_in_the_same_upstream_chunk() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let mut upstream_body = event_stream_frame(
        "assistantResponseEvent",
        serde_json::json!({"content":"before <END> ignored"}),
    );
    upstream_body.extend(event_stream_frame(
        "reasoningContentEvent",
        serde_json::json!({"reasoningContentEvent":{"text":"must not leak"}}),
    ));
    upstream_body.extend(event_stream_frame(
        "messageMetadataEvent",
        serde_json::json!({
            "messageMetadataEvent": {
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 20,
                    "creditsConsumed": 0.25
                }
            }
        }),
    ));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(upstream_body),
        )
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let config_path = daemon.home().join("config.toml");
    let config = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config")
        .replace("enable_prompt_cache = false", "enable_prompt_cache = true");
    tokio::fs::write(&config_path, config)
        .await
        .expect("enable prompt cache");
    let reload = expect_ok(daemon.call("config.reload", serde_json::json!({})).await);
    assert_eq!(reload["applied"], true, "config reload failed: {reload}");

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stop_sequences":["<END>"],
            "stream":true,
            "system":[{"type":"text","text":"cacheable system ".repeat(1500),
                "cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":"stop at the delimiter"}]
        }))
        .send()
        .await
        .expect("call Claude stop sequence stream");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("stream body");
    assert!(body.contains("before "), "{body}");
    assert!(!body.contains("ignored"), "{body}");
    assert!(!body.contains("must not leak"), "{body}");
    assert!(body.contains(r#""stop_reason":"stop_sequence""#), "{body}");
    assert!(body.contains(r#""stop_sequence":"<END>""#), "{body}");
    let message_start = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event["type"] == "message_start")
        .expect("message_start event");
    assert!(
        message_start["message"]["usage"]["cache_creation_input_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens >= 1024)
    );

    daemon.stop().await;
}

#[tokio::test]
async fn claude_thinking_follows_body_state_and_normalizes_tagged_content() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let mut upstream_body = event_stream_frame(
        "assistantResponseEvent",
        serde_json::json!({"content":"<thinking>tagged secret</thinking>Hello"}),
    );
    upstream_body.extend(event_stream_frame(
        "reasoningContentEvent",
        serde_json::json!({"reasoningContentEvent":{"text":"native secret"}}),
    ));
    upstream_body.extend(event_stream_frame(
        "messageMetadataEvent",
        serde_json::json!({
            "messageMetadataEvent": {
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 20,
                    "creditsConsumed": 0.25
                }
            }
        }),
    ));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(upstream_body),
        )
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();
    let endpoint = format!("http://127.0.0.1:{port}/v1/messages");
    let beta = "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13";

    let disabled = client
        .post(&endpoint)
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", beta)
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":4096,
            "stream":true,
            "thinking":{"type":"disabled"},
            "messages":[{"role":"user","content":"thinking disabled"}]
        }))
        .send()
        .await
        .expect("call Claude with thinking disabled");
    assert_eq!(disabled.status(), reqwest::StatusCode::OK);
    let disabled_body = disabled.text().await.expect("disabled stream body");
    assert!(disabled_body.contains("Hello"), "{disabled_body}");
    assert!(!disabled_body.contains("tagged secret"), "{disabled_body}");
    assert!(!disabled_body.contains("native secret"), "{disabled_body}");
    assert!(!disabled_body.contains("thinking_delta"), "{disabled_body}");
    assert!(!disabled_body.contains("<thinking>"), "{disabled_body}");

    let enabled = client
        .post(&endpoint)
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", beta)
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":4096,
            "stream":true,
            "thinking":{"type":"adaptive"},
            "context_management":{"edits":[{
                "type":"clear_thinking_20251015","keep":"all"
            }]},
            "messages":[{"role":"user","content":"thinking enabled"}]
        }))
        .send()
        .await
        .expect("call Claude with thinking enabled");
    assert_eq!(enabled.status(), reqwest::StatusCode::OK);
    let enabled_body = enabled.text().await.expect("enabled stream body");
    assert!(enabled_body.contains("Hello"), "{enabled_body}");
    assert!(enabled_body.contains("tagged secret"), "{enabled_body}");
    assert!(enabled_body.contains("native secret"), "{enabled_body}");
    assert!(enabled_body.contains("thinking_delta"), "{enabled_body}");
    assert!(enabled_body.contains("signature_delta"), "{enabled_body}");
    assert!(!enabled_body.contains("<thinking>"), "{enabled_body}");

    let received = mock.received_requests().await.expect("received requests");
    let prompts = received
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter_map(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 2);
    assert!(!prompts[0].contains("<thinking_mode>"));
    assert!(prompts[1].starts_with("<thinking_mode>enabled</thinking_mode>"));

    daemon.stop().await;
}

#[tokio::test]
async fn claude_compaction_uses_a_separate_semantic_kiro_request() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_88888888",
                            "email": "compact@example.com",
                            "machine_id": "8".repeat(64),
                            "credentials": {
                                "access_token": "compact-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": 0.0,
                                "limit": 100.0,
                                "percent_used": 0.0,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.0 (external, e2e)")
        .json(&serde_json::json!({
            "model":"minimax-m2.5",
            "max_tokens":64,
            "stream":false,
            "context_management":{"edits":[{
                "type":"compact_20260112",
                "trigger":{"type":"input_tokens","value":50000}
            }]},
            "messages":[
                {"role":"user","content":"old context ".repeat(35_000)},
                {"role":"assistant","content":"an earlier conclusion"},
                {"role":"user","content":"continue the implementation"}
            ]
        }))
        .send()
        .await
        .expect("call Claude compact path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert!(body["content"][0]["content"]
        .as_str()
        .is_some_and(|content| content.contains("migrate compact to semantic Kiro summaries")));
    assert_eq!(body["content"][1]["text"], "main request completed");
    assert_eq!(body["usage"]["iterations"][0]["type"], "compaction");
    assert_eq!(body["usage"]["iterations"][1]["type"], "message");

    let requests = mock.received_requests().await.expect("received requests");
    let payloads = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter(|payload| payload.get("conversationState").is_some())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 2, "summary plus main Kiro generation");
    let summary_request = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| content.contains("durable conversation checkpoint"))
        })
        .expect("semantic summary request");
    assert!(
        summary_request["conversationState"]["currentMessage"]["userInputMessage"]
            .get("userInputMessageContext")
            .is_none()
    );
    let main_request = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| {
                    content.contains("continue the implementation")
                        && !content.contains("durable conversation checkpoint")
                })
        })
        .expect("main request");
    assert!(
        main_request["conversationState"]["history"][0]["userInputMessage"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("System-generated conversation checkpoint"))
    );

    daemon.stop().await;
}

#[tokio::test]
async fn model_mapping_overflow_auto_compacts_before_the_first_generation() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "large-to-small"
type = "replace"
source_models = ["source-large"]
target_models = ["mapped-small"]
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "system":"Never lose this governing instruction.",
            "messages":[
                {"role":"user","content":"old context ".repeat(140000)},
                {"role":"assistant","content":"an earlier conclusion"},
                {"role":"user","content":"continue the implementation"}
            ]
        }))
        .send()
        .await
        .expect("call automatic compact path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert_eq!(body["content"][1]["text"], "main request completed");
    assert_eq!(body["usage"]["iterations"][0]["type"], "compaction");
    assert_eq!(body["usage"]["iterations"][1]["type"], "message");
    let applied = &body["context_management"]["applied_edits"][0];
    assert_eq!(applied["reason"], "model_mapping_overflow");
    assert!(
        applied["original_input_tokens"].as_u64().expect("original")
            > applied["compacted_input_tokens"]
                .as_u64()
                .expect("compacted")
    );

    let requests = mock.received_requests().await.expect("received requests");
    let payloads = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter(|payload| payload.get("conversationState").is_some())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 2, "one summary plus one main generation");
    let summary = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| content.contains("durable conversation checkpoint"))
        })
        .expect("summary request");
    assert_eq!(
        summary["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "summary-large"
    );
    let main = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| {
                    content.contains("continue the implementation")
                        && !content.contains("durable conversation checkpoint")
                })
        })
        .expect("main request");
    assert_eq!(
        main["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "mapped-small"
    );
    assert!(
        main["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("Never lose this governing instruction."))
    );

    daemon.stop().await;
}

#[tokio::test]
async fn compacted_stream_retries_before_flushing_the_compaction_block() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let responder = RetryingCompactionStreamResponder::default();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(responder.clone())
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_77777777",
                            "email": "stream-retry@example.com",
                            "machine_id": "7".repeat(64),
                            "credentials": {
                                "access_token": "stream-retry-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": 0.0,
                                "limit": 100.0,
                                "percent_used": 0.0,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "large-to-small-stream"
type = "replace"
source_models = ["source-large"]
target_models = ["mapped-small"]
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":true,
            "messages":[
                {"role":"user","content":"old context ".repeat(140000)},
                {"role":"assistant","content":"an earlier conclusion"},
                {"role":"user","content":"continue the streamed implementation"}
            ]
        }))
        .send()
        .await
        .expect("call compacted stream retry path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("stream body");
    assert_eq!(responder.main_calls.load(Ordering::SeqCst), 2);
    assert!(!body.contains("broken-write"), "{body}");
    assert!(!body.contains("produced complete JSON"), "{body}");
    assert!(body.contains("compaction_delta"), "{body}");
    assert!(
        body.contains("main request completed after retry"),
        "{body}"
    );
    assert!(
        body.find("compaction_delta").expect("compaction")
            < body
                .find("main request completed after retry")
                .expect("retried content")
    );

    daemon.stop().await;
}

#[tokio::test]
async fn timed_out_compaction_summary_is_still_accounted() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(SlowCompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "large-to-small-timeout"
type = "replace"
source_models = ["source-large"]
target_models = ["mapped-small"]
priority = 10
"#,
    )
    .await;
    let config_path = daemon.home().join("config.toml");
    let raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config");
    tokio::fs::write(
        &config_path,
        raw.replace(
            "compaction_summary_timeout_ms = 30000",
            "compaction_summary_timeout_ms = 1",
        ),
    )
    .await
    .expect("write timeout config");
    let reload = expect_ok(daemon.call("config.reload", serde_json::json!({})).await);
    assert_eq!(reload["applied"], true, "config reload failed: {reload}");

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"old context ".repeat(140000)},
                {"role":"assistant","content":"an earlier conclusion"},
                {"role":"user","content":"continue after the summary timeout"}
            ]
        }))
        .send()
        .await
        .expect("call summary timeout path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert_eq!(
        body["content"][1]["text"],
        "main request completed while summary accounting continued"
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let stats = expect_ok(
            daemon
                .call("stats", serde_json::json!({"detail":true,"recent":20}))
                .await,
        );
        let accounted = stats["stats"]["recent_requests"]
            .as_array()
            .is_some_and(|requests| {
                requests.iter().any(|request| {
                    request["path"] == "/internal/compact"
                        && request["input_tokens"]
                            .as_u64()
                            .is_some_and(|tokens| tokens > 0)
                        && request["credits"]
                            .as_f64()
                            .is_some_and(|credits| credits > 0.0)
                })
            });
        if accounted {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed-out summary never reached stats accounting: {stats}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    daemon.stop().await;
}

#[tokio::test]
async fn failed_compaction_stream_preserves_partial_usage_accounting() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(PartialFailureCompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "large-to-small-partial-summary"
type = "replace"
source_models = ["source-large"]
target_models = ["mapped-small"]
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"old context ".repeat(140000)},
                {"role":"assistant","content":"an earlier conclusion"},
                {"role":"user","content":"continue after the malformed summary stream"}
            ]
        }))
        .send()
        .await
        .expect("call malformed summary stream path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert_eq!(
        body["content"][1]["text"],
        "main request completed after the failed summary was accounted"
    );
    assert_eq!(body["usage"]["iterations"][0]["input_tokens"], 100);
    assert_eq!(body["usage"]["iterations"][0]["output_tokens"], 20);

    let stats = expect_ok(
        daemon
            .call("stats", serde_json::json!({"detail":true,"recent":20}))
            .await,
    );
    let compact = stats["stats"]["recent_requests"]
        .as_array()
        .and_then(|requests| {
            requests
                .iter()
                .find(|request| request["path"] == "/internal/compact")
        })
        .expect("failed compact request was recorded");
    assert_eq!(compact["status"], 502);
    assert_eq!(compact["input_tokens"], 100);
    assert_eq!(compact["output_tokens"], 20);
    assert_eq!(compact["credits"], 0.25);

    daemon.stop().await;
}

#[tokio::test]
async fn resolved_account_window_replans_once_and_reuses_the_summary() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 50.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "low-credit-small-window"
type = "replace"
source_models = ["source-large"]
target_models = ["resolved-small"]
max_remaining_credit_percent = 90.0
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"resolved history ".repeat(35000)},
                {"role":"assistant","content":"keep the decision"},
                {"role":"user","content":"finish after replanning"}
            ]
        }))
        .send()
        .await
        .expect("call resolved-window compact path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");

    let requests = mock.received_requests().await.expect("received requests");
    let payloads = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter(|payload| payload.get("conversationState").is_some())
        .collect::<Vec<_>>();
    assert_eq!(
        payloads.len(),
        2,
        "the failed context preflight must not reach Kiro and replanning must not resummarize"
    );
    assert_eq!(
        payloads
            .iter()
            .filter(
                |payload| payload["conversationState"]["currentMessage"]["userInputMessage"]
                    ["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("durable conversation checkpoint"))
            )
            .count(),
        1
    );
    let main = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| {
                    content.contains("finish after replanning")
                        && !content.contains("durable conversation checkpoint")
                })
        })
        .expect("main request");
    assert_eq!(
        main["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "resolved-small"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn mapped_compaction_is_reapplied_for_a_smaller_resolved_window_without_resummarizing() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(LongCompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 50.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "low-credit-tiny-window"
type = "replace"
source_models = ["source-large"]
target_models = ["resolved-tiny"]
max_remaining_credit_percent = 90.0
priority = 5

[[model_mapping]]
name = "large-to-small"
type = "replace"
source_models = ["source-large"]
target_models = ["mapped-small"]
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"mapped history ".repeat(140000)},
                {"role":"assistant","content":"keep the decision"},
                {"role":"user","content":"finish after semantic reuse"}
            ]
        }))
        .send()
        .await
        .expect("call two-window compact path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert!(body["content"][0]["content"]
        .as_str()
        .is_some_and(|content| content.contains("checkpoint shortened to fit context")));

    let requests = mock.received_requests().await.expect("received requests");
    let payloads = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter(|payload| payload.get("conversationState").is_some())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 2, "one summary plus one main request");
    assert_eq!(
        payloads
            .iter()
            .filter(
                |payload| payload["conversationState"]["currentMessage"]["userInputMessage"]
                    ["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("durable conversation checkpoint"))
            )
            .count(),
        1,
        "resolved-window replanning must reuse the first summary"
    );
    let main = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| {
                    content.contains("finish after semantic reuse")
                        && !content.contains("durable conversation checkpoint")
                })
        })
        .expect("main request");
    assert_eq!(
        main["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "resolved-tiny"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn a_second_smaller_resolved_window_fails_without_a_compaction_loop() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 20.0).await;
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_88888888",
                            "email": "smaller-window@example.com",
                            "machine_id": "8".repeat(64),
                            "credentials": {
                                "access_token": "smaller-window-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": 30.0,
                                "limit": 100.0,
                                "percent_used": 30.0,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );
    configure_context_alignment(
        &daemon,
        // Force capacity preflight to choose extractive fallback so the
        // summary operation cannot affect which account the second main
        // preflight selects.
        "resolved-tiny",
        r#"
[[model_mapping]]
name = "lowest-credit-tiny-window"
type = "replace"
source_models = ["source-large"]
target_models = ["resolved-tiny"]
max_remaining_credit_percent = 75.0
priority = 5

[[model_mapping]]
name = "low-credit-small-window"
type = "replace"
source_models = ["source-large"]
target_models = ["resolved-small"]
max_remaining_credit_percent = 85.0
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"resolved drift history ".repeat(35000)},
                {"role":"assistant","content":"keep the decision"},
                {"role":"user","content":"this must stop after one replan ".repeat(2000)}
            ]
        }))
        .send()
        .await
        .expect("call bounded resolved-window path");
    let status = response.status();
    let body: serde_json::Value = response.json().await.expect("Claude error JSON");
    let requests = mock.received_requests().await.expect("received requests");
    let generated_models = requests
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter_map(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "unexpected response {body}; generated models: {generated_models:?}"
    );
    let message = body["error"]["message"].as_str().expect("error message");
    assert!(
        message.contains("resolved-tiny"),
        "unexpected error: {message}; generated models: {generated_models:?}"
    );
    assert!(message.contains("prompt is too long"));
    assert!(
        message.contains(" > 495"),
        "unexpected safe window: {message}"
    );

    assert_eq!(
        generated_models.len(),
        0,
        "both account checks must fail before Kiro and no third attempt is allowed"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn bare_start_creates_files_secures_socket_and_serves_status() {
    use std::os::unix::fs::PermissionsExt;

    let daemon = Daemon::start().await;
    for name in ["config.toml", "accounts.json", "daily.json", "stats.json"] {
        assert!(daemon.home().join(name).exists(), "{name} missing");
    }
    let socket_mode = std::fs::metadata(&daemon.socket)
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(socket_mode, 0o600);
    let status = expect_ok(daemon.call("status", serde_json::json!({})).await);
    assert_eq!(status["account_total"], 0);
    assert_eq!(status["listen"], "-");
    assert_eq!(status["proxy_service_total"], 0);
    assert!(status["hint"].is_string());
    daemon.stop().await;
}

#[tokio::test]
async fn account_lifecycle_persists_without_token_leakage() {
    let daemon = Daemon::start().await;
    let account = serde_json::json!({
        "accounts": [{
            "id": "acc_11111111",
            "email": "e2e@example.com",
            "machine_id": "2".repeat(64),
            "credentials": {
                "access_token": "at-e2e-secret",
                "region": "us-east-1",
                "expires_at": 4_000_000_000i64,
                "auth_method": "idc"
            },
            "created_at": 1
        }]
    });
    assert_eq!(
        expect_ok(daemon.call("account.import", account).await)["imported"],
        1
    );
    let list = expect_ok(daemon.call("account.list", serde_json::json!({})).await);
    assert_eq!(list["accounts"].as_array().expect("array").len(), 1);
    assert!(!serde_json::to_string(&list)
        .expect("serialize")
        .contains("at-e2e-secret"));

    expect_ok(
        daemon
            .call(
                "account.tag",
                serde_json::json!({"id": "e2e@example.com", "add": ["prod"]}),
            )
            .await,
    );
    let filtered = expect_ok(
        daemon
            .call("account.list", serde_json::json!({"tag": "prod"}))
            .await,
    );
    assert_eq!(filtered["accounts"].as_array().expect("array").len(), 1);

    expect_ok(
        daemon
            .call(
                "account.setEnabled",
                serde_json::json!({"id": "acc_11111111", "enabled": false}),
            )
            .await,
    );
    let after = expect_ok(daemon.call("account.list", serde_json::json!({})).await);
    assert_eq!(after["accounts"][0]["enabled"], false);
    let disk = tokio::fs::read_to_string(daemon.home().join("accounts.json"))
        .await
        .expect("read disk");
    assert!(disk.contains("prod"));

    expect_ok(
        daemon
            .call("account.remove", serde_json::json!({"id": "acc_11111111"}))
            .await,
    );
    let empty = expect_ok(daemon.call("account.list", serde_json::json!({})).await);
    assert!(empty["accounts"].as_array().expect("array").is_empty());
    daemon.stop().await;
}

#[tokio::test]
async fn config_edit_hot_reloads_and_broken_edit_keeps_service_alive() {
    let daemon = Daemon::start().await;
    let config_path = daemon.home().join("config.toml");
    let raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config");
    let edited = raw.replace("enable_prompt_cache = false", "enable_prompt_cache = true");
    tokio::fs::write(&config_path, edited)
        .await
        .expect("edit config");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let show = expect_ok(daemon.call("config.show", serde_json::json!({})).await);
        if show["effective_json"]["features"]["enable_prompt_cache"] == true {
            break;
        }
        assert!(Instant::now() < deadline, "hot reload timeout");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    tokio::fs::write(&config_path, "[server\nport = ")
        .await
        .expect("break config");
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let status = expect_ok(daemon.call("status", serde_json::json!({})).await);
    assert_eq!(status["listen"], "-");
    daemon.stop().await;
}

#[tokio::test]
async fn unknown_method_returns_structured_error() {
    let daemon = Daemon::start().await;
    match daemon.call("does.not.exist", serde_json::json!({})).await {
        Response::Err { error, .. } => assert_eq!(error.code, 404),
        Response::Ok { .. } => panic!("unknown method should fail"),
    }
    daemon.stop().await;
}

#[tokio::test]
async fn restart_reloads_persisted_accounts() {
    let home = tempfile::tempdir().expect("tempdir");
    let socket = home.path().join("admin.sock");
    let mut first = Command::new(env!("CARGO_BIN_EXE_kproxyd"))
        .env("KPROXY_HOME", home.path())
        .env("KPROXY_HTTP_PORT", "0")
        .env("KPROXY_DISABLE_HTTP", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn first");
    wait_for_socket(&socket).await;
    expect_ok(
        rpc_call(
            &socket,
            "account.import",
            serde_json::json!({
                "accounts": [{
                    "id": "acc_33333333",
                    "email": "persist@example.com",
                    "machine_id": "4".repeat(64),
                    "credentials": {
                        "access_token": "at",
                        "region": "us-east-1",
                        "expires_at": 4_000_000_000i64,
                        "auth_method": "idc"
                    },
                    "created_at": 1
                }]
            }),
        )
        .await,
    );
    first.kill().await.expect("kill first");
    let _first_status = first.wait().await;

    let mut second = Command::new(env!("CARGO_BIN_EXE_kproxyd"))
        .env("KPROXY_HOME", home.path())
        .env("KPROXY_HTTP_PORT", "0")
        .env("KPROXY_DISABLE_HTTP", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn second");
    wait_for_socket(&socket).await;
    let list = expect_ok(rpc_call(&socket, "account.list", serde_json::json!({})).await);
    assert_eq!(list["accounts"].as_array().expect("array").len(), 1);
    assert_eq!(list["accounts"][0]["email"], "persist@example.com");
    second.kill().await.expect("kill second");
}

#[tokio::test]
async fn external_account_file_edits_reload_without_accepting_corruption() {
    let daemon = Daemon::start().await;
    let path = daemon.home().join("accounts.json");
    tokio::fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!([{
            "id": "acc_55555555",
            "email": "external@example.com",
            "machine_id": "5".repeat(64),
            "credentials": {
                "access_token": "external-token",
                "region": "us-east-1",
                "expires_at": 4_000_000_000i64,
                "auth_method": "idc"
            },
            "created_at": 1
        }]))
        .expect("serialize accounts"),
    )
    .await
    .expect("write accounts");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = expect_ok(daemon.call("account.list", serde_json::json!({})).await);
        if list["accounts"]
            .as_array()
            .is_some_and(|items| items.len() == 1)
        {
            assert_eq!(list["accounts"][0]["email"], "external@example.com");
            break;
        }
        assert!(Instant::now() < deadline, "account file reload timeout");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    tokio::fs::write(&path, b"{broken")
        .await
        .expect("break accounts");
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let retained = expect_ok(daemon.call("account.list", serde_json::json!({})).await);
    assert_eq!(retained["accounts"][0]["email"], "external@example.com");
    daemon.stop().await;
}

#[tokio::test]
async fn stale_socket_file_does_not_block_startup() {
    let home = tempfile::tempdir().expect("tempdir");
    let socket = home.path().join("admin.sock");
    tokio::fs::write(&socket, b"stale")
        .await
        .expect("write stale");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kproxyd"))
        .env("KPROXY_HOME", home.path())
        .env("KPROXY_HTTP_PORT", "0")
        .env("KPROXY_DISABLE_HTTP", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");
    wait_for_socket(&socket).await;
    child.kill().await.expect("kill");
}

#[tokio::test]
async fn second_daemon_cannot_delete_a_live_socket() {
    let daemon = Daemon::start().await;
    let mut second = Command::new(env!("CARGO_BIN_EXE_kproxyd"))
        .env("KPROXY_HOME", daemon.home())
        .env("KPROXY_HTTP_PORT", "0")
        .env("KPROXY_DISABLE_HTTP", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn second");
    let status = tokio::time::timeout(Duration::from_secs(5), second.wait())
        .await
        .expect("second daemon should exit")
        .expect("wait second");
    assert!(!status.success());

    let still_alive = expect_ok(daemon.call("status", serde_json::json!({})).await);
    assert_eq!(still_alive["pid"], daemon.child.id().expect("pid"));
    daemon.stop().await;
}

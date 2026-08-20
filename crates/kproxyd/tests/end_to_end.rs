//! 端到端测试：拉起真实 kproxyd，通过 Unix socket 走完整协议。

use std::path::{Path, PathBuf};
use std::process::Stdio;
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
async fn inbound_openai_request_runs_through_translation_pool_and_mock_upstream() {
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
            "model": "minimax-m2.5",
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
            "minimax-m2.5"
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

use super::*;
use serde_json::{json, Value};

async fn configure(daemon: &Daemon, edit: impl FnOnce(&mut kproxy_core::config::Config)) {
    let path = daemon.home().join("config.toml");
    let mut config: kproxy_core::config::Config =
        toml::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
    edit(&mut config);
    tokio::fs::write(path, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    assert_eq!(
        expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
        true
    );
}

async fn log_records(daemon: &Daemon) -> Vec<Value> {
    let mut records = Vec::new();
    let mut files = tokio::fs::read_dir(daemon.home().join("logs"))
        .await
        .unwrap();
    while let Some(file) = files.next_entry().await.unwrap() {
        if file.path().extension().is_some_and(|ext| ext == "log") {
            for line in tokio::fs::read_to_string(file.path())
                .await
                .unwrap()
                .lines()
            {
                if let Ok(value) = serde_json::from_str::<Value>(line) {
                    records.push(value);
                }
            }
        }
    }
    records
}

#[tokio::test]
async fn invalid_models_are_rejected_once_without_leaking_pasted_text() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    configure(&daemon, |config| config.log.level = "debug".into()).await;
    let client = reqwest::Client::new();
    let mut traces = Vec::new();
    for model in [
        "private_terminal_sentinel\nSELECT secret".into(),
        format!("private_terminal_sentinel{}", "x".repeat(300)),
    ] {
        for route in [
            "messages",
            "messages/count_tokens",
            "chat/completions",
            "responses",
        ] {
            let body = if route == "responses" {
                json!({"model":model,"input":"hello","store":false})
            } else {
                json!({"model":model,"max_tokens":1,"messages":[{"role":"user","content":"hello"}]})
            };
            let response = client
                .post(format!("http://127.0.0.1:{port}/v1/{route}"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header(
                    "user-agent",
                    if route.starts_with("messages") {
                        "claude-cli/2.1.260 (external, test)"
                    } else {
                        "codex_cli_rs/0.147.0"
                    },
                )
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 400);
            assert_eq!(
                response.headers()["x-kproxy-error-stage"],
                "request_validation"
            );
            traces.push(
                response.headers()["request-id"]
                    .to_str()
                    .unwrap()
                    .to_owned(),
            );
            let error_body = response.text().await.unwrap();
            assert!(
                error_body.contains("invalid value for model"),
                "{error_body}"
            );
            assert!(!error_body.contains("private_terminal_sentinel"));
        }
    }
    let stats = expect_ok(
        daemon
            .call("stats", json!({"detail":true,"recent":20}))
            .await,
    );
    assert!(!stats.to_string().contains("private_terminal_sentinel"));
    for request in stats["stats"]["recent_requests"].as_array().unwrap() {
        assert!(request["attempts"].as_array().is_none_or(Vec::is_empty));
        assert_eq!(request["credits"], 0.0);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let records = log_records(&daemon).await;
        assert!(!records
            .iter()
            .any(|record| record.to_string().contains("private_terminal_sentinel")));
        let counts = traces
            .iter()
            .map(|trace| {
                records
                    .iter()
                    .filter(|record| {
                        record["level"] == "WARN" && record["fields"]["trace_id"] == *trace
                    })
                    .count()
            })
            .collect::<Vec<_>>();
        assert!(
            counts.iter().all(|count| *count <= 1),
            "duplicate rejection warnings: {counts:?}"
        );
        if counts.iter().all(|count| *count == 1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "missing rejection warnings: {counts:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    daemon.stop().await;
}

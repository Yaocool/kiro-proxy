//! Verify that the new explicit actions preserve their intended RPC contracts.
#![cfg(unix)]

use std::time::Duration;

use kproxy_ipc::protocol::{decode_line, encode_line, method, Request, Response};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

async fn run(args: &[&str], results: Vec<Value>) -> (std::process::Output, Vec<Request>) {
    let workspace = tempfile::tempdir().expect("tempdir");
    let socket = workspace.path().join("admin.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock admin socket");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for result in results {
            let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .expect("accept timeout")
                .expect("accept request");
            let (read, mut write) = stream.into_split();
            let line = BufReader::new(read)
                .lines()
                .next_line()
                .await
                .expect("read request")
                .expect("request line");
            let request: Request = decode_line(&line).expect("decode request");
            write
                .write_all(
                    encode_line(&Response::ok(request.id, result))
                        .expect("encode response")
                        .as_bytes(),
                )
                .await
                .expect("write response");
            requests.push(request);
        }
        requests
    });
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(env!("CARGO_BIN_EXE_kproxy"))
            .args(["--json", "--socket"])
            .arg(&socket)
            .args(args)
            .env_remove("KPROXY_ADMIN_SOCKET")
            .output(),
    )
    .await
    .expect("CLI timeout")
    .expect("run CLI");
    let requests = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("mock server timeout")
        .expect("mock server task");
    (output, requests)
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn explicit_list_actions_use_the_existing_methods() {
    let (tasks, requests) = run(&["tasks", "list"], vec![json!([])]).await;
    assert_success(&tasks);
    assert_eq!(requests[0].method, method::TASKS);
    assert_eq!(requests[0].params, json!({}));

    let (models, requests) = run(
        &["models", "list", "--refresh"],
        vec![json!({"ran": true}), json!([])],
    )
    .await;
    assert_success(&models);
    assert_eq!(requests[0].method, method::TASK_RUN);
    assert_eq!(requests[0].params, json!({"name": "model_cache_refresh"}));
    assert_eq!(requests[1].method, method::MODELS);
}

#[tokio::test]
async fn explicit_log_action_preserves_the_existing_query_parameters() {
    let (output, requests) = run(
        &[
            "logs",
            "show",
            "--tail",
            "25",
            "--level",
            "error",
            "--account",
            "alice@example.com",
        ],
        vec![json!({"entries": []})],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests[0].method, method::LOGS);
    assert_eq!(
        requests[0].params,
        json!({
            "after_request_id": null,
            "tail": 25,
            "wait_ms": 0,
            "level": "error",
            "account": "alice@example.com"
        })
    );
}

#[tokio::test]
async fn diagnose_all_preserves_the_two_step_diagnosis_parameters() {
    let (output, requests) = run(
        &[
            "diagnose",
            "all",
            "--region",
            "us-west-2",
            "--timeout",
            "30s",
            "--concurrency",
            "4",
        ],
        vec![json!({"reachable": true}), json!({"healthy": true})],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests[0].method, method::DIAGNOSE_ENDPOINTS);
    assert_eq!(requests[0].params, json!({"region": "us-west-2"}));
    assert_eq!(requests[1].method, method::DIAGNOSE_ACCOUNT);
    assert_eq!(
        requests[1].params,
        json!({"all": true, "timeout_secs": 30, "concurrency": 4})
    );
}

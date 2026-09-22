//! Verify that the new explicit actions preserve their intended RPC contracts.
#![cfg(unix)]

use std::time::Duration;
use std::{fs, path::Path};

use kproxy_ipc::protocol::{decode_line, encode_line, method, Request, Response};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

async fn run(args: &[&str], results: Vec<Value>) -> (std::process::Output, Vec<Request>) {
    let workspace = tempfile::tempdir().expect("tempdir");
    run_in_workspace(workspace.path(), args, results).await
}

async fn run_human(args: &[&str], results: Vec<Value>) -> (std::process::Output, Vec<Request>) {
    let workspace = tempfile::tempdir().expect("tempdir");
    run_in_workspace_mode(workspace.path(), args, results, false).await
}

async fn run_in_workspace(
    workspace: &Path,
    args: &[&str],
    results: Vec<Value>,
) -> (std::process::Output, Vec<Request>) {
    run_in_workspace_mode(workspace, args, results, true).await
}

async fn run_in_workspace_mode(
    workspace: &Path,
    args: &[&str],
    results: Vec<Value>,
    json_output: bool,
) -> (std::process::Output, Vec<Request>) {
    let socket = workspace.join("admin.sock");
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_kproxy"));
    if json_output {
        command.arg("--json");
    }
    command
        .arg("--socket")
        .arg(&socket)
        .args(args)
        .env_remove("KPROXY_ADMIN_SOCKET");
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .expect("CLI timeout")
        .expect("run CLI");
    let requests = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("mock server timeout")
        .expect("mock server task");
    (output, requests)
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
        vec![json!({
            "schema_version": 2,
            "scope": {"provider": "all"},
            "models": [],
            "errors": {},
            "complete": true
        })],
    )
    .await;
    assert_success(&models);
    assert_eq!(requests[0].method, method::V2_MODELS);
    assert_eq!(
        requests[0].params,
        json!({"provider": "all", "provider_kind": null, "refresh": true})
    );
}

#[tokio::test]
async fn account_list_sends_a_boolean_enabled_filter() {
    let (output, requests) = run(
        &["account", "list"],
        vec![json!({
            "schema_version": 2,
            "scope": {"provider": "all"},
            "accounts": [],
            "errors": {},
            "complete": true
        })],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests[0].method, method::V2_ACCOUNT_LIST);
    assert_eq!(requests[0].params["enabled_only"], false);
}

#[tokio::test]
async fn account_overage_enable_updates_config_and_uses_the_minimal_rpc_sequence() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let config_file = workspace.path().join("config.toml");
    fs::write(
        &config_file,
        "# keep this comment\n[pool]\nenable_overage = false\nmax_overage_credits_per_account = 250.0\n",
    )
    .expect("write config");
    let mut effective = kproxy_core::config::Config::default();
    effective.pool.enable_overage = true;
    effective.pool.max_overage_credits_per_account = Some(500.0);
    let path = config_file.to_string_lossy().into_owned();
    let (output, requests) = run_in_workspace(
        workspace.path(),
        &[
            "account",
            "overage",
            "enable",
            "--max-credits",
            "500",
            "--no-refresh",
        ],
        vec![
            json!({
                "config_file": path,
                "accounts_file": workspace.path().join("accounts.json"),
                "daily_file": workspace.path().join("daily.json"),
                "stats_file": workspace.path().join("stats.json"),
                "admin_socket": workspace.path().join("admin.sock")
            }),
            json!({"applied": true, "needs_restart": []}),
            json!({
                "path": config_file,
                "raw": "",
                "effective_json": serde_json::to_value(effective).expect("effective config")
            }),
            json!({
                "schema_version": 2,
                "scope": {"provider": "kiro"},
                "accounts": [],
                "errors": {},
                "complete": true
            }),
        ],
    )
    .await;
    assert_success(&output);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        [
            method::CONFIG_PATH,
            method::CONFIG_RELOAD,
            method::CONFIG_SHOW,
            method::V2_ACCOUNT_LIST
        ]
    );
    assert_eq!(requests[3].params["enabled_only"], false);
    let saved = fs::read_to_string(&config_file).expect("read updated config");
    assert!(saved.contains("# keep this comment"));
    let parsed: kproxy_core::config::Config = toml::from_str(&saved).expect("parse updated config");
    assert!(parsed.pool.enable_overage);
    assert_eq!(parsed.pool.max_overage_credits_per_account, Some(500.0));
    let output: Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    assert_eq!(output["action"], "enabled");
    assert_eq!(output["enabled"], true);
}

#[tokio::test]
async fn account_overage_refresh_reports_raw_machine_values() {
    let mut effective = kproxy_core::config::Config::default();
    effective.pool.enable_overage = true;
    effective.pool.max_overage_credits_per_account = Some(500.0);
    let (output, requests) = run(
        &["account", "overage", "show", "--refresh"],
        vec![
            json!({"name": "status_check", "result": "ok: 1 healthy, 0 failed"}),
            json!({
                "path": "/tmp/config.toml",
                "raw": "",
                "effective_json": serde_json::to_value(effective).expect("effective config")
            }),
            json!({
                "schema_version": 2,
                "scope": {"provider": "kiro"},
                "accounts": [{
                    "provider_id": "kiro",
                    "provider_kind": "kiro",
                    "id": "acc_00000001",
                    "display_name": "a@example.com",
                    "email": "a@example.com",
                    "label": null,
                    "enabled": true,
                    "health": "available",
                    "auth_state": "ready",
                    "tags": ["prod"],
                    "quota_current": 10000.01,
                    "quota_limit": 10500.0,
                    "quota_unit": "kiro_credits",
                    "supported_models": [],
                    "details": {
                        "overage_enabled": true,
                        "max_overage_credits_per_account": 500.0,
                        "effective_overage_cap": 500.0,
                        "kiro_overage_cap": 10000.0,
                        "kiro_overage_total_limit": 20000.0
                    }
                }],
                "errors": {},
                "complete": true
            }),
        ],
    )
    .await;
    assert_success(&output);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        [
            method::TASK_RUN,
            method::CONFIG_SHOW,
            method::V2_ACCOUNT_LIST
        ]
    );
    assert_eq!(
        requests[0].params,
        json!({"name": "status_check", "provider": null})
    );
    let output: Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    assert_eq!(output["refresh"]["ok"], true);
    assert_eq!(output["accounts"][0]["health"], "available");
    assert_eq!(output["accounts"][0]["current"], 10000.01);
    assert_eq!(output["accounts"][0]["proxy_total"], 10500.0);
    assert_eq!(output["accounts"][0]["kiro_total"], 20000.0);
    assert_eq!(output["accounts"][0]["effective_overage"], 500.0);
    assert_eq!(output["accounts"][0]["kiro_overage"], 10000.0);
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
            "account": "alice@example.com",
            "provider": null
        })
    );

    let (output, requests) = run(
        &["logs", "show", "--level", "info"],
        vec![json!({"entries": []})],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests[0].params["level"], Value::Null);
}

#[tokio::test]
async fn stats_group_is_normalized_to_the_daemon_contract() {
    let (output, requests) = run(&["stats", "--detail", "--by", "apikey"], vec![json!({})]).await;
    assert_success(&output);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, method::STATS);
    assert_eq!(requests[0].params["by"], "apikey");
    assert_eq!(requests[0].params["detail"], true);
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

#[tokio::test]
async fn service_create_sends_multiple_account_tags_as_one_array() {
    let (output, requests) = run(
        &[
            "service",
            "create",
            "--name",
            "team",
            "--account-tag",
            "xx1",
            "xx2",
        ],
        vec![json!({
            "service": {
                "id": "svc_team",
                "name": "team",
                "host": "127.0.0.1",
                "port": 5581,
                "enabled": true,
                "running": true,
                "api_key_ids": ["ak_team"],
                "account_tags": ["xx1", "xx2"],
                "account_ids": [],
                "excluded_account_ids": [],
                "created_at": 1
            },
            "api_key": {"id": "ak_team", "name": "team-default", "key": "sk-test"}
        })],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, method::SERVICE_CREATE);
    assert_eq!(requests[0].params["account_tag"], json!(["xx1", "xx2"]));
}

#[tokio::test]
async fn destructive_account_command_supports_yes_and_json_cancellation() {
    let (removed, requests) = run(
        &["account", "rm", "acc_00000001", "--yes"],
        vec![json!({"id": "acc_00000001", "removed": true})],
    )
    .await;
    assert_success(&removed);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, method::V2_ACCOUNT_REMOVE);
    assert_eq!(requests[0].params["id"], "acc_00000001");

    let (cancelled, requests) = run(&["account", "rm", "acc_00000001"], vec![]).await;
    assert_success(&cancelled);
    assert!(requests.is_empty());
    let cancelled: Value = serde_json::from_slice(&cancelled.stdout).expect("JSON cancellation");
    assert_eq!(cancelled["removed"], false);
    assert_eq!(cancelled["cancelled"], true);
}

#[tokio::test]
async fn batch_account_remove_emits_one_json_document() {
    let (output, requests) = run(
        &["account", "rm", "acc_00000001", "acc_00000002", "--yes"],
        vec![
            json!({"id": "acc_00000001", "removed": true}),
            json!({"id": "acc_00000002", "removed": true}),
        ],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests.len(), 2);
    let output: Value = serde_json::from_slice(&output.stdout).expect("single JSON document");
    assert_eq!(output["results"].as_array().map(Vec::len), Some(2));
    assert_eq!(output["errors"].as_array().map(Vec::len), Some(0));
    assert_eq!(output["complete"], true);
}

#[tokio::test]
async fn edit_commands_reject_empty_changes_without_rpc_calls() {
    for args in [
        &["provider", "edit", "kiro"][..],
        &["service", "edit", "main"][..],
        &["apikey", "edit", "ci"][..],
        &["alert", "edit", "ops"][..],
        &["model-map", "edit", "low-credit"][..],
    ] {
        let (output, requests) = run(args, vec![]).await;
        assert!(!output.status.success(), "{args:?}");
        assert!(requests.is_empty(), "{args:?}");
        assert!(stderr(&output).contains("没有指定修改项"), "{args:?}");
    }
}

#[tokio::test]
async fn diagnose_all_honors_human_output_mode() {
    let (output, requests) = run_human(
        &["diagnose", "all"],
        vec![json!({"reachable": true}), json!({"healthy": true})],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests.len(), 2);
    let text = String::from_utf8(output.stdout).expect("UTF-8 output");
    assert!(text.contains("端点诊断"), "{text}");
    assert!(text.contains("账号诊断"), "{text}");
    assert!(serde_json::from_str::<Value>(&text).is_err());
}

#[tokio::test]
async fn config_mutation_commands_honor_json_output() {
    let alert_workspace = tempfile::tempdir().expect("tempdir");
    let alert_config = alert_workspace.path().join("config.toml");
    fs::write(&alert_config, "").expect("write alert config");
    let (alert, requests) = run_in_workspace(
        alert_workspace.path(),
        &[
            "alert",
            "add",
            "--name",
            "ops",
            "--platform",
            "dingtalk",
            "--webhook-url",
            "https://example.com/hook",
            "--event",
            "token-refresh-failed",
        ],
        vec![
            json!({
                "config_file": alert_config,
                "accounts_file": alert_workspace.path().join("accounts.json"),
                "daily_file": alert_workspace.path().join("daily.json"),
                "stats_file": alert_workspace.path().join("stats.json"),
                "admin_socket": alert_workspace.path().join("admin.sock")
            }),
            json!({"applied": true, "needs_restart": []}),
        ],
    )
    .await;
    assert_success(&alert);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        [method::CONFIG_PATH, method::CONFIG_RELOAD]
    );
    let alert: Value = serde_json::from_slice(&alert.stdout).expect("alert JSON");
    assert_eq!(alert, json!({"name": "ops", "created": true}));

    let map_workspace = tempfile::tempdir().expect("tempdir");
    let map_config = map_workspace.path().join("config.toml");
    fs::write(&map_config, "").expect("write model map config");
    let (mapping, requests) = run_in_workspace(
        map_workspace.path(),
        &[
            "model-map",
            "add",
            "--name",
            "low-credit",
            "--source",
            "claude-opus-*",
            "--target",
            "claude-sonnet-4",
        ],
        vec![
            json!({
                "config_file": map_config,
                "accounts_file": map_workspace.path().join("accounts.json"),
                "daily_file": map_workspace.path().join("daily.json"),
                "stats_file": map_workspace.path().join("stats.json"),
                "admin_socket": map_workspace.path().join("admin.sock")
            }),
            json!({"applied": true, "needs_restart": []}),
        ],
    )
    .await;
    assert_success(&mapping);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        [method::CONFIG_PATH, method::CONFIG_RELOAD]
    );
    let mapping: Value = serde_json::from_slice(&mapping.stdout).expect("model map JSON");
    assert_eq!(mapping, json!({"name": "low-credit", "created": true}));
}

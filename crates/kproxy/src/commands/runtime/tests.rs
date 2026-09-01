use super::*;

fn pool_output_fixture() -> PoolOutput {
    PoolOutput {
        model: "claude-opus-5".into(),
        queued: 2,
        scoring: Some(PoolScoringOutput {
            weight_active: 0.5,
            weight_credit: 0.4,
            weight_idle: 0.1,
            max_concurrent_per_account: 50,
            idle_window_ms: 300_000,
        }),
        accounts: vec![
            PoolAccountOutput {
                account_id: "acc_ready".into(),
                account_name: "Primary account".into(),
                score: Some(0.123_456),
                active_factor: 0.02,
                credit_factor: 0.25,
                idle_factor: 0.5,
                eligible: true,
                reason: "available".into(),
            },
            PoolAccountOutput {
                account_id: "acc_exhausted".into(),
                account_name: "Exhausted account".into(),
                score: None,
                active_factor: 0.0,
                credit_factor: 0.0,
                idle_factor: 0.0,
                eligible: false,
                reason: "exhausted".into(),
            },
            PoolAccountOutput {
                account_id: "acc_wrong_model".into(),
                account_name: "Different subscription".into(),
                score: None,
                active_factor: 0.0,
                credit_factor: 0.0,
                idle_factor: 0.0,
                eligible: false,
                reason: "model_unavailable".into(),
            },
        ],
    }
}

#[test]
fn pool_default_view_is_compact_and_folds_unavailable_accounts() {
    let output = render_pool_output(&pool_output_fixture(), false);
    assert!(output.contains("模型 claude-opus-5  排队 2  可调度 1/3"));
    assert!(output.contains("额度耗尽 1"));
    assert!(output.contains("模型不支持 1"));
    assert!(output.contains("acc_ready"));
    assert!(output.contains("0.1235"));
    assert!(output.contains("评分说明：越低越优"));
    assert!(output.contains("使用 --explain 查看公式"));
    assert!(!output.contains("acc_exhausted"));
    assert!(!output.contains("acc_wrong_model"));
    assert!(!output.contains('{'));
}

#[test]
fn pool_explain_view_shows_all_accounts_and_factors() {
    let output = render_pool_output(&pool_output_fixture(), true);
    assert!(output.contains("acc_ready"));
    assert!(output.contains("acc_exhausted"));
    assert!(output.contains("acc_wrong_model"));
    assert!(output.contains("并发"));
    assert!(output.contains("2.0%"));
    assert!(output.contains("25.0%"));
    assert!(output.contains("50.0%"));
    assert!(output.contains("额度耗尽"));
    assert!(output.contains("模型不支持"));
    assert!(output.contains("评分 = 并发 × 0.5 + 额度 × 0.4 + 近期使用 × 0.1"));
    assert!(output.contains("并发 = 活跃请求数 ÷ 50"));
    assert!(output.contains("空闲 5 分钟后降为 0%"));
    assert!(output.contains("完全同分时会加入极小随机量打破平局"));
}

#[test]
fn credits_are_displayed_with_two_decimal_places() {
    assert_eq!(format_credits(12.345), "12.35");
    assert_eq!(format_credits(4.0), "4.00");
}

#[tokio::test]
async fn config_backup_never_overwrites_an_existing_backup() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = directory.path().join("config.toml");
    std::fs::write(config.with_file_name("config.toml.bak"), b"first").expect("existing backup");

    let backup = write_config_backup(&config, b"current")
        .await
        .expect("new backup");

    assert_eq!(backup, config.with_file_name("config.toml.bak.1"));
    assert_eq!(std::fs::read(backup).expect("backup contents"), b"current");
    assert_eq!(
        std::fs::read(config.with_file_name("config.toml.bak")).expect("original backup"),
        b"first"
    );
}

#[test]
fn config_reset_preserves_api_keys_and_running_proxy_services() {
    let raw = r#"[server]
host = "127.0.0.1"
port = 6200
max_concurrent_requests = 17

[[api_key]]
id = "ak_keep"
name = "keep"
key = "sk-keep-secret"
format = "sk"
enabled = true
credits_limit = 12.5

[[proxy_service]]
id = "svc_keep"
name = "keep"
host = "127.0.0.1"
port = 6201
enabled = true
api_key_ids = ["ak_keep"]
created_at = 123
"#;

    let output = render_reset_config_preserving_services(raw).expect("reset config");
    let reset: kproxy_core::config::Config = toml::from_str(&output).expect("parse reset config");
    let defaults = kproxy_core::config::Config::default();

    assert_eq!(reset.server.host, defaults.server.host);
    assert_eq!(reset.server.port, defaults.server.port);
    assert_eq!(
        reset.server.max_concurrent_requests,
        defaults.server.max_concurrent_requests
    );
    assert_eq!(reset.api_key.len(), 1);
    assert_eq!(reset.api_key[0].id.as_deref(), Some("ak_keep"));
    assert_eq!(reset.api_key[0].key, "sk-keep-secret");
    assert!(reset.api_key[0].enabled);
    assert_eq!(reset.api_key[0].credits_limit, Some(12.5));
    assert_eq!(reset.proxy_service.len(), 1);
    assert_eq!(reset.proxy_service[0].id, "svc_keep");
    assert_eq!(reset.proxy_service[0].port, 6201);
    assert!(reset.proxy_service[0].enabled);
    assert_eq!(reset.proxy_service[0].api_key_ids, ["ak_keep"]);
    assert!(output.contains("# kiro-proxy 配置文件"));
}

#[test]
fn config_reset_keeps_unbound_api_keys() {
    let raw = r#"[[api_key]]
id = "ak_unbound"
name = "unbound"
key = "token_unbound"
format = "token"
enabled = true
"#;

    let output = render_reset_config_preserving_services(raw).expect("reset config");
    let reset: kproxy_core::config::Config = toml::from_str(&output).expect("parse reset config");

    assert_eq!(reset.api_key.len(), 1);
    assert_eq!(reset.api_key[0].id.as_deref(), Some("ak_unbound"));
    assert!(reset.proxy_service.is_empty());
}

#[test]
fn config_module_catalog_covers_every_top_level_config_section() {
    let configured = serde_json::to_value(kproxy_core::config::Config::default())
        .expect("serialize default config")
        .as_object()
        .expect("config object")
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let cataloged = CONFIG_MODULES
        .iter()
        .map(|module| module.key.to_string())
        .collect::<std::collections::BTreeSet<_>>();

    assert_eq!(cataloged, configured);
}

#[test]
fn config_module_names_accept_cli_names_toml_keys_and_aliases() {
    assert_eq!(
        resolve_config_module("model-mapping")
            .expect("CLI name")
            .key,
        "model_mapping"
    );
    assert_eq!(
        resolve_config_module("model_mapping")
            .expect("TOML key")
            .name,
        "model-mapping"
    );
    assert_eq!(
        resolve_config_module("apikey").expect("alias").key,
        "api_key"
    );
    assert!(resolve_config_module("missing").is_err());
}

#[test]
fn config_module_document_is_scoped_and_preserves_comments() {
    let raw = r#"# server section
[server]
# port explanation
port = 5580

# pool section
[pool]
max_queue_size = 10
"#;
    let server = resolve_config_module("server").expect("server module");

    let output = render_config_module_document(raw, server).expect("render module");
    let parsed = output.parse::<toml::Value>().expect("parse module");

    assert_eq!(
        parsed.as_table().expect("root").keys().collect::<Vec<_>>(),
        ["server"]
    );
    assert!(output.contains("# port explanation"));
    assert!(!output.contains("max_queue_size"));
}

#[test]
fn config_module_edit_changes_only_the_selected_module() {
    let original = kproxy_store::bootstrap::render_default_config(
        &kproxy_core::config::Config::default().admin.socket,
    );
    let pool = resolve_config_module("pool").expect("pool module");
    let edited = render_config_module_editor(&original, pool)
        .expect("editor document")
        .replace(
            "max_concurrent_per_account = 50",
            "max_concurrent_per_account = 7",
        );

    let output = merge_edited_config_module(&original, pool, &edited).expect("merge module");
    let before = original.parse::<toml::Value>().expect("original TOML");
    let after = output.parse::<toml::Value>().expect("updated TOML");
    let config: kproxy_core::config::Config = toml::from_str(&output).expect("updated config");

    assert_eq!(config.pool.max_concurrent_per_account, 7);
    assert_eq!(before["server"], after["server"]);
    assert_eq!(before["features"], after["features"]);
    assert!(output.contains("# 账号池、排队、额度保护与选号"));
}

#[test]
fn config_module_edit_rejects_other_top_level_modules() {
    let original = kproxy_store::bootstrap::render_default_config(
        &kproxy_core::config::Config::default().admin.socket,
    );
    let pool = resolve_config_module("pool").expect("pool module");
    let edited = "[pool]\nmax_queue_size = 10\n\n[server]\nport = 6200\n";

    let error = merge_edited_config_module(&original, pool, edited).expect_err("reject module");

    assert!(error.to_string().contains("server"));
}

#[test]
fn config_module_edit_validates_references_against_the_full_config() {
    let original = r#"[[api_key]]
id = "ak_keep"
name = "keep"
key = "sk-keep"
enabled = true

[[proxy_service]]
id = "svc_keep"
name = "keep"
host = "127.0.0.1"
port = 6201
enabled = true
api_key_ids = ["ak_keep"]
"#;
    let api_keys = resolve_config_module("api-key").expect("API key module");

    let error = merge_edited_config_module(original, api_keys, "api_key = []\n")
        .expect_err("orphaned service reference must fail");

    assert!(error.to_string().contains("配置校验失败"));
}

#[test]
fn config_module_reset_restores_only_the_selected_module() {
    let original = kproxy_store::bootstrap::render_default_config(
        &kproxy_core::config::Config::default().admin.socket,
    )
    .replace("port = 5580", "port = 6200")
    .replace(
        "max_concurrent_per_account = 50",
        "max_concurrent_per_account = 7",
    );
    let pool = resolve_config_module("pool").expect("pool module");

    let output = render_config_module_reset(&original, pool).expect("reset pool");
    let reset: kproxy_core::config::Config = toml::from_str(&output).expect("reset config");
    let defaults = kproxy_core::config::Config::default();

    assert_eq!(reset.pool.max_concurrent_per_account, 50);
    assert_eq!(reset.pool.max_queue_size, defaults.pool.max_queue_size);
    assert_eq!(reset.server.port, 6200);
    assert!(output.contains("# 账号池、排队、额度保护与选号"));
}

#[test]
fn config_module_reset_clears_only_the_selected_rule_map() {
    let original = r#"[server]
port = 6200

[model_thinking_mode]
"claude-opus" = false
"claude-sonnet" = true
"#;
    let thinking = resolve_config_module("thinking").expect("thinking module");

    let output = render_config_module_reset(original, thinking).expect("reset thinking map");
    let reset: kproxy_core::config::Config = toml::from_str(&output).expect("reset config");

    assert!(reset.model_thinking_mode.is_empty());
    assert_eq!(reset.server.port, 6200);
}

#[test]
fn config_module_reset_clears_only_the_selected_rule_array() {
    let original = r#"[server]
port = 6200

[[model_mapping]]
name = "fallback"
type = "replace"
source_models = ["claude-opus-*"]
target_models = ["claude-sonnet-4.6"]
priority = 10
"#;
    let mappings = resolve_config_module("model-map").expect("mapping module");

    let output = render_config_module_reset(original, mappings).expect("reset mappings");
    let reset: kproxy_core::config::Config = toml::from_str(&output).expect("reset config");

    assert!(reset.model_mapping.is_empty());
    assert_eq!(reset.server.port, 6200);
}

#[test]
fn config_module_reset_refuses_foundation_service_resources() {
    for name in ["api-key", "proxy-service"] {
        let module = resolve_config_module(name).expect("foundation module");
        assert!(!module.resettable());
        let error = render_config_module_reset("", module).expect_err("reset must be refused");
        assert!(error.to_string().contains("基础服务资源"));
    }
}

#[test]
fn configured_editor_preserves_quoted_arguments() {
    assert_eq!(
        parse_editor("code --wait --profile 'K Proxy'").expect("editor command"),
        EditorCommand {
            program: PathBuf::from("code"),
            args: vec!["--wait".into(), "--profile".into(), "K Proxy".into()],
        }
    );
    assert!(parse_editor("code '").is_err());
}

#[test]
fn editor_uses_utf8_locale_when_effective_locale_is_not_utf8() {
    use std::ffi::OsStr;

    assert!(editor_needs_utf8_locale(None, None, None));
    assert!(editor_needs_utf8_locale(
        Some(OsStr::new("C")),
        Some(OsStr::new("zh_CN.UTF-8")),
        Some(OsStr::new("zh_CN.UTF-8")),
    ));
    assert!(editor_needs_utf8_locale(
        None,
        None,
        Some(OsStr::new("zh_CN.GB18030")),
    ));
    assert!(!editor_needs_utf8_locale(
        None,
        Some(OsStr::new("C.utf8")),
        Some(OsStr::new("C")),
    ));
    assert!(!editor_needs_utf8_locale(
        None,
        None,
        Some(OsStr::new("en_US.UTF-8")),
    ));
}

#[test]
fn docker_wrapper_maps_log_files_to_the_host_data_volume() {
    let mut result = LogFilesResult {
        base_path: "/var/lib/kproxy/logs/kproxyd.log".into(),
        host_base_path: None,
        directory: "/var/lib/kproxy/logs".into(),
        host_directory: None,
        format: "json".into(),
        level_filter: "info".into(),
        files: vec![kproxy_ipc::protocol::LogFileView {
            path: "/var/lib/kproxy/logs/kproxyd-2026-08-24-info.log".into(),
            host_path: None,
            level: "info".into(),
            date: "2026-08-24".into(),
            size_bytes: 42,
            modified_at: None,
        }],
    };

    populate_host_log_paths(
        &mut result,
        Some(Path::new(
            "/var/lib/docker/volumes/kiro-proxy_kproxy-data/_data",
        )),
        Path::new("/var/lib/kproxy"),
    );

    assert_eq!(
        result.host_directory.as_deref(),
        Some("/var/lib/docker/volumes/kiro-proxy_kproxy-data/_data/logs")
    );
    assert_eq!(
        result.host_base_path.as_deref(),
        Some("/var/lib/docker/volumes/kiro-proxy_kproxy-data/_data/logs/kproxyd.log")
    );
    assert_eq!(
        result.files[0].host_path.as_deref(),
        Some(
            "/var/lib/docker/volumes/kiro-proxy_kproxy-data/_data/logs/\
             kproxyd-2026-08-24-info.log"
        )
    );
}

#[test]
fn logs_outside_the_data_volume_do_not_claim_a_host_path() {
    assert_eq!(
        host_log_path(
            "/tmp/kproxyd.log",
            Path::new("/var/lib/docker/volumes/kproxy/_data"),
            Path::new("/var/lib/kproxy"),
        ),
        None
    );
}

#[cfg(unix)]
#[test]
fn default_editor_prefers_vim_over_vi() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory");
    let vim = directory.path().join("vim");
    let vi = directory.path().join("vi");
    std::fs::write(&vim, "").expect("vim");
    std::fs::write(&vi, "").expect("vi");
    std::fs::set_permissions(&vim, std::fs::Permissions::from_mode(0o755))
        .expect("vim executable permissions");
    std::fs::set_permissions(&vi, std::fs::Permissions::from_mode(0o755))
        .expect("vi executable permissions");

    assert_eq!(
        find_default_editor(Some(directory.path().as_os_str())),
        Some(vim)
    );
}

#[cfg(unix)]
#[test]
fn default_editor_uses_first_executable_candidate_on_path() {
    use std::os::unix::fs::PermissionsExt;

    let first = tempfile::tempdir().expect("first temp directory");
    let second = tempfile::tempdir().expect("second temp directory");
    let non_executable_vi = first.path().join("vi");
    std::fs::write(&non_executable_vi, "").expect("non-executable vi");
    let vim = second.path().join("vim");
    std::fs::write(&vim, "").expect("vim");
    std::fs::set_permissions(&vim, std::fs::Permissions::from_mode(0o755))
        .expect("executable permissions");
    let path = std::env::join_paths([first.path(), second.path()]).expect("PATH");

    assert_eq!(find_default_editor(Some(path.as_os_str())), Some(vim));
}

#[test]
fn log_account_prefers_name_and_keeps_id_for_diagnostics() {
    let value = serde_json::json!({
        "account_id": "acc_deadbeef",
        "account_name": "Enterprise team"
    });
    assert_eq!(log_account(&value), "Enterprise team (acc_deadbeef)");
    assert_eq!(log_account(&serde_json::json!({})), "-");
}

#[test]
fn apikey_list_hides_usage_until_detail_is_requested() {
    let entries = serde_json::from_value::<Vec<ApiKeyListEntry>>(serde_json::json!([{
        "id":"ak_one",
        "name":"one",
        "enabled":true,
        "credits_limit":100.0,
        "reserved_credits":0.5,
        "usage":{
            "total_requests":3,
            "total_credits":1.25,
            "total_input_tokens":120,
            "total_output_tokens":30,
            "daily":{"2026-08-11":{"requests":3}},
            "history":[{"credits":1.25}]
        }
    }]))
    .expect("API key entries");
    let summary = ApiKeyListSummary::from_entries(&entries);

    let compact = apikey_list_json(&entries, &summary, false);
    assert_eq!(compact["summary"]["total"], 1);
    assert_eq!(compact["summary"]["total_credits"], 1.25);
    assert_eq!(compact["summary"]["total_input_tokens"], 120);
    assert!(compact["api_keys"][0].get("total_credits").is_none());
    assert!(compact["api_keys"][0].get("usage").is_none());

    let detail = apikey_list_json(&entries, &summary, true);
    assert_eq!(detail["summary"]["total_requests"], 3);
    assert_eq!(detail["summary"]["total_input_tokens"], 120);
    assert_eq!(detail["summary"]["total_output_tokens"], 30);
    assert_eq!(detail["summary"]["total_credits"], 1.25);
    assert_eq!(detail["api_keys"][0]["reserved_credits"], 0.5);
    assert!(detail["api_keys"][0].get("history").is_none());
}

#[test]
fn log_model_route_distinguishes_mapping_from_automatic_resolution() {
    let automatic = serde_json::json!({
        "original_model": "claude-4.6-sonnet",
        "model": "claude-4.6-sonnet",
        "kiro_model": "claude-sonnet-4.6"
    });
    assert_eq!(
        log_model_route(&automatic),
        LogModelRoute {
            original: "claude-4.6-sonnet",
            routed: "claude-4.6-sonnet",
            resolved: "claude-sonnet-4.6",
            mapping_rule: None,
        }
    );
    let forced = serde_json::json!({
        "original_model": "claude-4.6-sonnet",
        "model": "claude-opus-4.6",
        "kiro_model": "claude-opus-4.6",
        "model_mapping_rule": "force-opus"
    });
    assert_eq!(log_model_route(&forced).mapping_rule, Some("force-opus"));
}

#[test]
fn timestamps_accept_unix_and_timezone_aware_rfc3339() {
    assert_eq!(parse_timestamp("0").expect("unix epoch"), 0);
    assert_eq!(
        parse_timestamp("2026-08-27T10:00:00+08:00").expect("China time"),
        parse_timestamp("2026-08-27T02:00:00Z").expect("UTC time")
    );
    assert_eq!(
        parse_timestamp("2024-02-29T00:00:00Z").expect("leap day"),
        1_709_164_800
    );
}

#[test]
fn timestamps_reject_ambiguous_or_invalid_values() {
    assert!(parse_timestamp("2026-08-27T10:00:00").is_err());
    assert!(parse_timestamp("2026-02-29T10:00:00Z").is_err());
    assert!(parse_timestamp("2026-08-27T25:00:00+08:00").is_err());
}

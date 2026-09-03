use kproxy_core::account::{Account, AuthMethod, Credentials, Usage};
use kproxy_core::config::Config;
use kproxy_core::paths::Paths;
use kproxy_store::accounts::AccountStore;
use kproxy_store::config_loader::ConfigHandle;
use tempfile::TempDir;

use super::*;

fn sample_account(id: &str, email: &str, enabled: bool) -> Account {
    Account {
        id: id.into(),
        email: email.into(),
        label: None,
        enabled,
        machine_id: "a".repeat(64),
        profile_arn: None,
        upstream_user_id: None,
        credentials: Credentials {
            access_token: "at-secret".into(),
            refresh_token: Some("rt-secret".into()),
            client_id: Some("cid".into()),
            client_secret: Some("cs-secret".into()),
            region: "us-east-1".into(),
            expires_at: 1_700_000_000,
            auth_method: AuthMethod::Idc,
        },
        usage: None,
        subscription: None,
        tags: vec![],
        created_at: 0,
        credit_exhausted: false,
    }
}

#[test]
fn sso_identity_matching_accepts_idc_username_variants_but_rejects_other_users() {
    assert!(sso_identities_match(
        "alice@example.com",
        "ALICE@example.com"
    ));
    assert!(sso_identities_match(
        "kiro.svc.70@patsnap.com",
        "kirosvc.70"
    ));
    assert!(sso_identities_match(
        "kiro.svc.51@patsnap.com",
        "kirosvc.51 kirosvc.51"
    ));
    assert!(sso_identities_match(
        "kiro.svc.59@patsnap.com",
        "kirosvc.59 kirosvc.5"
    ));
    assert!(!sso_identities_match(
        "kiro.svc.70@patsnap.com",
        "kirosvc.41"
    ));
    assert!(!sso_identities_match(
        "kiro.svc.70@patsnap.com",
        "kirosvc.41 kirosvc.41"
    ));
    assert!(!sso_identities_match("", ""));
}

#[test]
fn sso_identity_validation_uses_stable_user_id_instead_of_display_name() {
    let limits: kproxy_kiro::UsageLimits = serde_json::from_value(serde_json::json!({
        "userInfo": {
            "email": "unrelated display name",
            "userId": "stable-user-71"
        }
    }))
    .expect("usage limits");
    assert_eq!(
        authenticated_sso_user_id(&limits).expect("stable identity"),
        "stable-user-71"
    );

    let error = authenticated_sso_user_id(&kproxy_kiro::UsageLimits::default())
        .expect_err("missing stable identity must be rejected");
    assert!(error.message.contains("stable user ID"));
    assert!(error.message.contains("not saved"));
}

async fn state_with(accounts: Vec<Account>) -> (TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().expect("tempdir");
    let paths = Paths::from_env_values(
        Some(directory.path().to_str().expect("utf8")),
        None,
        None,
        None,
    );
    kproxy_store::bootstrap::ensure_layout(&paths)
        .await
        .expect("bootstrap");
    let mut store = AccountStore::load(&paths.accounts_file)
        .await
        .expect("load");
    for account in accounts {
        store.insert(account).expect("insert");
    }
    store.save().await.expect("save");
    let state = Arc::new(AppState::new(
        paths,
        ConfigHandle::new(Config::default()),
        store,
    ));
    (directory, state)
}

fn expect_ok(response: Response) -> serde_json::Value {
    match response {
        Response::Ok { result, .. } => result,
        Response::Err { error, .. } => {
            panic!("expected ok, got {}: {}", error.code, error.message)
        }
    }
}

#[tokio::test]
async fn status_reports_counts_and_empty_hint() {
    let mut protected = sample_account("acc_00000003", "protected@example.com", true);
    protected.usage = Some(Usage {
        current: 97.0,
        limit: 100.0,
        percent_used: 97.0,
        next_reset_date: None,
        updated_at: 0,
    });
    let mut exhausted = sample_account("acc_00000004", "exhausted@example.com", true);
    exhausted.usage = Some(Usage {
        current: 100.0,
        limit: 100.0,
        percent_used: 100.0,
        next_reset_date: None,
        updated_at: 0,
    });
    let (_directory, state) = state_with(vec![
        sample_account("acc_00000001", "a@example.com", true),
        sample_account("acc_00000002", "b@example.com", false),
        protected,
        exhausted,
    ])
    .await;
    state.admission.set_maximum(123);
    let status: StatusResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::STATUS, serde_json::json!({})),
        )
        .await,
    ))
    .expect("status");
    assert_eq!(status.account_total, 4);
    assert_eq!(status.account_enabled, 3);
    assert_eq!(status.account_available, 1);
    assert_eq!(status.account_protected, 1);
    assert_eq!(status.account_exhausted, 1);
    assert_eq!(status.account_refreshing, 0);
    assert_eq!(status.listen, "-");
    assert_eq!(status.proxy_service_total, 0);
    assert_eq!(status.proxy_service_running, 0);
    assert_eq!(status.max_concurrent_requests, 123);
    assert!(!status.ready);
    assert!(status
        .readiness_reasons
        .iter()
        .any(|reason| reason.contains("proxy service")));

    let truncated: StatusResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::STATUS,
                serde_json::json!({"start_secs":0,"end_secs":now_secs()}),
            ),
        )
        .await,
    ))
    .expect("truncated status");
    assert!(truncated.stats_truncated);
    assert_eq!(
        truncated.stats_start,
        Some(state.stats.session_started_at())
    );

    let (_directory, empty) = state_with(vec![]).await;
    let empty_status: StatusResult = serde_json::from_value(expect_ok(
        dispatch(
            &empty,
            Request::new(1, method::STATUS, serde_json::json!({})),
        )
        .await,
    ))
    .expect("empty status");
    assert!(empty_status.hint.is_some());
    assert!(!empty_status.ready);
}

#[tokio::test]
async fn log_files_reports_resolved_paths_and_physical_partitions() {
    let (_directory, state) = state_with(vec![]).await;
    let log_directory = state.paths.data_dir.join("logs");
    std::fs::create_dir_all(&log_directory).expect("log directory");
    let info = log_directory.join("kproxyd-2026-08-23-info.log");
    let error = log_directory.join("kproxyd-2026-08-23-error.1.log");
    std::fs::write(&info, b"info\n").expect("info log");
    std::fs::write(&error, b"failure\n").expect("error log");
    std::fs::write(log_directory.join("unrelated.log"), b"ignore").expect("unrelated log");

    let result: LogFilesResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::LOG_FILES, serde_json::json!({})),
        )
        .await,
    ))
    .expect("log files result");
    assert_eq!(result.directory, log_directory.display().to_string());
    assert_eq!(
        result.base_path,
        log_directory.join("kproxyd.log").display().to_string()
    );
    assert_eq!(result.files.len(), 2);
    assert!(result.files.iter().any(|file| {
        file.path == info.display().to_string() && file.level == "info" && file.size_bytes == 5
    }));
    assert!(result.files.iter().any(|file| {
        file.path == error.display().to_string() && file.level == "error" && file.size_bytes == 8
    }));

    let paths: ConfigPathResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(2, method::CONFIG_PATH, serde_json::json!({})),
        )
        .await,
    ))
    .expect("config paths");
    assert_eq!(paths.log_directory, result.directory);
    assert_eq!(paths.log_base_path, result.base_path);
}

#[tokio::test]
async fn trace_logs_searches_across_exact_severity_partitions() {
    let (_directory, state) = state_with(vec![]).await;
    let log_directory = state.paths.data_dir.join("logs");
    std::fs::create_dir_all(&log_directory).expect("log directory");
    let trace_id = "trace_0123456789abcdef0123456789abcdef";
    std::fs::write(
        log_directory.join("kproxyd-2026-08-23-info.log"),
        format!(
            "{{\"timestamp\":\"2026-08-23T01:00:00Z\",\"fields\":{{\"message\":\"received\",\"trace_id\":\"{trace_id}\"}}}}\n"
        ),
    )
    .expect("info log");
    std::fs::write(
        log_directory.join("kproxyd-2026-08-23-error.log"),
        format!(
            "{{\"timestamp\":\"2026-08-23T01:00:01Z\",\"span\":{{\"trace_id\":\"{trace_id}\"}},\"fields\":{{\"message\":\"failed\"}}}}\n"
        ),
    )
    .expect("error log");

    let result: LogTraceResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::LOG_TRACE,
                serde_json::json!({"trace_id":trace_id,"tail":100}),
            ),
        )
        .await,
    ))
    .expect("trace log result");

    assert_eq!(result.trace_id, trace_id);
    assert_eq!(result.files_scanned, 2);
    assert_eq!(result.matched_records, 2);
    assert_eq!(result.entries.len(), 2);
    assert_eq!(result.entries[0].level, "info");
    assert_eq!(result.entries[1].level, "error");
    assert!(!result.truncated);
}

#[tokio::test]
async fn stats_default_is_compact_and_detail_restores_recent_requests() {
    let (_directory, state) = state_with(vec![]).await;
    state.stats.record(crate::stats::RequestLog {
        timestamp: now_secs(),
        trace_id: "trace_stats".into(),
        request_id: "req_stats".into(),
        path: "/v1/messages".into(),
        model: "claude-sonnet-4.6".into(),
        original_model: "claude-4.6-sonnet".into(),
        kiro_model: "claude-sonnet-4.6".into(),
        account_id: "acc_stats".into(),
        account_name: "Enterprise stats".into(),
        endpoint: "codewhisperer".into(),
        model_path: vec!["claude-4.6-sonnet".into(), "claude-sonnet-4.6".into()],
        model_mapping_rule: None,
        attempts: Vec::new(),
        duration_ms: 25,
        status: 200,
        input_tokens: 120,
        output_tokens: 30,
        credits: 0.5,
        error: None,
        diagnostics: crate::stats::RequestDiagnostics::default(),
    });

    let compact = expect_ok(
        dispatch(
            &state,
            Request::new(1, method::STATS, serde_json::json!({})),
        )
        .await,
    );
    assert_eq!(compact["summary"]["requests"], 1);
    assert_eq!(compact["scope"], "persistent");
    assert_eq!(compact["latency"]["average_ms"], 25);
    assert_eq!(compact["latency"]["p50_ms"], 25);
    assert!(compact.get("stats").is_none());
    assert!(compact.get("grouped").is_none());

    let detail = expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::STATS,
                serde_json::json!({"detail":true,"recent":20,"by":"model"}),
            ),
        )
        .await,
    );
    assert_eq!(
        detail["stats"]["recent_requests"][0]["account_name"],
        "Enterprise stats"
    );
    assert_eq!(detail["grouped"]["claude-sonnet-4.6"]["requests"], 1);

    let status: StatusResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(3, method::STATUS, serde_json::json!({})),
        )
        .await,
    ))
    .expect("status");
    assert_eq!(status.stats_scope, "session");
    assert_eq!(status.request_count, 1);
    assert_eq!(status.average_latency_ms, 25);
    assert!(status.stats_start.is_some());
    assert!(status.stats_end.is_some());

    let historical = expect_ok(
        dispatch(
            &state,
            Request::new(
                4,
                method::STATS,
                serde_json::json!({"start_secs":0,"end_secs":60}),
            ),
        )
        .await,
    );
    assert_eq!(historical["summary"]["requests"], 0);
    assert_eq!(historical["range"]["start"], 0);
    assert_eq!(historical["range"]["end"], 60);
    assert_eq!(historical["range"]["truncated"], false);
}

#[tokio::test]
async fn creating_first_proxy_service_returns_a_scoped_api_key() {
    let (_directory, state) = state_with(vec![]).await;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);

    let created: ProxyServiceCreateResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({"name":"first","port":port}),
            ),
        )
        .await,
    ))
    .expect("created service");
    assert!(created.service.running);
    assert_eq!(created.service.host, "0.0.0.0");
    assert_eq!(created.service.port, port);
    assert_eq!(
        created.service.api_key_ids,
        vec![created.api_key.id.clone()]
    );
    assert!(created.api_key.key.starts_with("sk-"));

    let listed: ProxyServiceListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(2, method::SERVICE_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("service list");
    assert_eq!(listed.services.len(), 1);
    assert!(!serde_json::to_string(&listed)
        .expect("serialize list")
        .contains(&created.api_key.key));

    let hidden: ProxyServiceApiKeysResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::SERVICE_APIKEYS,
                serde_json::json!({"service":"first"}),
            ),
        )
        .await,
    ))
    .expect("hidden service API keys");
    assert_eq!(hidden.service_id, created.service.id);
    assert_eq!(hidden.api_keys.len(), 1);
    assert!(hidden.api_keys[0].key.is_none());
    assert!(!serde_json::to_string(&hidden)
        .expect("serialize hidden keys")
        .contains(&created.api_key.key));

    let revealed: ProxyServiceApiKeysResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                4,
                method::SERVICE_APIKEYS,
                serde_json::json!({
                    "service":created.service.id,
                    "show_secret":true
                }),
            ),
        )
        .await,
    ))
    .expect("revealed service API keys");
    assert_eq!(
        revealed.api_keys[0].key.as_deref(),
        Some(created.api_key.key.as_str())
    );

    let health: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/health"))
        .await
        .expect("health request")
        .json()
        .await
        .expect("health JSON");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["available_accounts"], 0);

    let deleted: ProxyServiceDeleteResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                5,
                method::SERVICE_DELETE,
                serde_json::json!({"service":"first"}),
            ),
        )
        .await,
    ))
    .expect("deleted service");
    assert_eq!(deleted.service_id, created.service.id);
    assert_eq!(deleted.service_name, created.service.name);
    assert_eq!(deleted.deleted_api_key_ids.len(), 1);
    assert_eq!(deleted.deleted_api_key_ids[0], created.api_key.id);
    assert!(deleted.retained_api_key_ids.is_empty());

    let listed_after_delete: ProxyServiceListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(6, method::SERVICE_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("service list after delete");
    assert!(listed_after_delete.services.is_empty());
    assert!(!state
        .config
        .current()
        .api_key
        .iter()
        .any(|key| key.id.as_deref() == Some(created.api_key.id.as_str())));
    assert!(state
        .meter
        .authenticate(Some(&created.api_key.key))
        .expect("empty key registry permits unauthenticated requests")
        .is_none());
    let persisted = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .expect("persisted config");
    assert!(!persisted.contains(&created.api_key.id));
    assert!(!persisted.contains(&created.api_key.key));
    assert!(persisted.contains("# 日志"));
    assert!(persisted.contains("# [[proxy_service]]"));
    state.shutdown.cancel();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_mutation_waits_for_the_config_file_lock() {
    let (_directory, state) = state_with(vec![]).await;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);
    let transaction = kproxy_store::atomic::lock_file_exclusive(&state.paths.config_file)
        .await
        .expect("lock config transaction");

    let mutation_state = Arc::clone(&state);
    let mut mutation = tokio::spawn(async move {
        dispatch(
            &mutation_state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({"name":"locked","port":port}),
            ),
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut mutation)
            .await
            .is_err(),
        "service mutation bypassed an active config transaction"
    );

    drop(transaction);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), mutation)
        .await
        .expect("service mutation timed out")
        .expect("join service mutation");
    let created: ProxyServiceCreateResult =
        serde_json::from_value(expect_ok(response)).expect("created service");
    assert_eq!(created.service.name, "locked");
    state.shutdown.cancel();
}

#[test]
fn service_key_cleanup_preserves_keys_shared_with_other_services() {
    let exclusive = ApiKeyConfig {
        id: Some("ak_exclusive".into()),
        name: "exclusive".into(),
        key: "sk-exclusive".into(),
        format: ApiKeyFormat::Sk,
        enabled: true,
        credits_limit: None,
    };
    let shared = ApiKeyConfig {
        id: Some("ak_shared".into()),
        name: "shared".into(),
        key: "sk-shared".into(),
        format: ApiKeyFormat::Sk,
        enabled: true,
        credits_limit: None,
    };
    let removed = ProxyServiceConfig {
        id: "svc_removed".into(),
        name: "removed".into(),
        host: "127.0.0.1".into(),
        port: 5580,
        enabled: true,
        api_key_ids: vec!["ak_exclusive".into(), "ak_shared".into()],
        created_at: 0,
    };
    let remaining = ProxyServiceConfig {
        id: "svc_remaining".into(),
        name: "remaining".into(),
        host: "127.0.0.1".into(),
        port: 5581,
        enabled: true,
        api_key_ids: vec!["ak_shared".into()],
        created_at: 0,
    };
    let mut config = Config {
        api_key: vec![exclusive, shared],
        proxy_service: vec![remaining],
        ..Config::default()
    };

    let (deleted, retained) = remove_unshared_service_api_keys(&mut config, &removed);

    assert_eq!(deleted, ["ak_exclusive"]);
    assert_eq!(retained, ["ak_shared"]);
    assert_eq!(config.api_key.len(), 1);
    assert_eq!(config.api_key[0].id.as_deref(), Some("ak_shared"));
}

#[tokio::test]
async fn account_lifecycle_persists_without_exposing_tokens() {
    let (_directory, state) = state_with(vec![]).await;
    let account = sample_account("acc_00000001", "a@example.com", true);
    let imported: AccountImportResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::ACCOUNT_IMPORT,
                serde_json::json!({"accounts": [account]}),
            ),
        )
        .await,
    ))
    .expect("import");
    assert_eq!(imported.imported, 1);

    let list = expect_ok(
        dispatch(
            &state,
            Request::new(2, method::ACCOUNT_LIST, serde_json::json!({})),
        )
        .await,
    );
    assert_eq!(list["accounts"].as_array().expect("array").len(), 1);
    assert!(!serde_json::to_string(&list)
        .expect("serialize")
        .contains("at-secret"));

    expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::ACCOUNT_TAG,
                serde_json::json!({"id": "a@example.com", "add": ["prod"]}),
            ),
        )
        .await,
    );
    let raw = tokio::fs::read_to_string(&state.paths.accounts_file)
        .await
        .expect("read disk");
    assert!(raw.contains("prod"));
}

#[tokio::test]
async fn account_lists_and_exports_default_to_email_order() {
    let (_directory, state) = state_with(vec![
        sample_account("acc_00000001", "z@example.com", true),
        sample_account("acc_00000002", "a@example.com", true),
        sample_account("acc_00000003", "B@example.com", true),
    ])
    .await;

    let list: AccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::ACCOUNT_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("account list");
    assert_eq!(
        list.accounts
            .iter()
            .map(|account| account.email.as_str())
            .collect::<Vec<_>>(),
        ["a@example.com", "B@example.com", "z@example.com"]
    );

    let by_id: AccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(2, method::ACCOUNT_LIST, serde_json::json!({"sort":"id"})),
        )
        .await,
    ))
    .expect("account list by ID");
    assert_eq!(
        by_id
            .accounts
            .iter()
            .map(|account| account.email.as_str())
            .collect::<Vec<_>>(),
        ["z@example.com", "a@example.com", "B@example.com"]
    );

    let exported = expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::ACCOUNT_EXPORT,
                serde_json::json!({"redact":true}),
            ),
        )
        .await,
    );
    assert_eq!(
        exported
            .as_array()
            .expect("exported accounts")
            .iter()
            .map(|account| account["email"].as_str().expect("email"))
            .collect::<Vec<_>>(),
        ["a@example.com", "B@example.com", "z@example.com"]
    );

    let invalid = dispatch(
        &state,
        Request::new(
            4,
            method::ACCOUNT_LIST,
            serde_json::json!({"sort":"unknown"}),
        ),
    )
    .await;
    assert!(matches!(
        invalid,
        Response::Err { error, .. } if error.message.contains("unsupported account sort field")
    ));
}

#[tokio::test]
async fn administrative_lists_use_stable_name_order() {
    let (_directory, state) = state_with(vec![]).await;
    let mut config = Config::default();
    config.webhook = ["Zulu", "alpha"]
        .map(|name| kproxy_core::config::WebhookConfig {
            name: name.into(),
            kind: "custom".into(),
            url: format!("https://example.com/{name}"),
            enabled: true,
            events: vec![],
            dingtalk_sign: None,
            telegram_chat_id: None,
            custom_template: None,
        })
        .into();
    config.api_key = [
        ("ak_zulu", "Zulu key", "secret-zulu"),
        ("ak_alpha", "alpha key", "secret-alpha"),
    ]
    .map(|(id, name, key)| ApiKeyConfig {
        id: Some(id.into()),
        name: name.into(),
        key: key.into(),
        format: ApiKeyFormat::Sk,
        enabled: true,
        credits_limit: None,
    })
    .into();
    config.proxy_service = [
        ("svc_zulu", "Zulu service", 6001),
        ("svc_alpha", "alpha service", 6002),
    ]
    .map(|(id, name, port)| ProxyServiceConfig {
        id: id.into(),
        name: name.into(),
        host: "127.0.0.1".into(),
        port,
        enabled: false,
        api_key_ids: vec!["ak_zulu".into(), "ak_alpha".into()],
        created_at: 0,
    })
    .into();
    state.config.replace(config);

    let services: ProxyServiceListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::SERVICE_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("service list");
    assert_eq!(
        services
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha service", "Zulu service"]
    );

    let keys: ProxyServiceApiKeysResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::SERVICE_APIKEYS,
                serde_json::json!({"service":"svc_alpha","show_secret":false}),
            ),
        )
        .await,
    ))
    .expect("service API keys");
    assert_eq!(
        keys.api_keys
            .iter()
            .map(|key| key.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha key", "Zulu key"]
    );

    let webhooks = expect_ok(
        dispatch(
            &state,
            Request::new(3, method::WEBHOOK_LIST, serde_json::json!({})),
        )
        .await,
    );
    assert_eq!(
        webhooks
            .as_array()
            .expect("webhook list")
            .iter()
            .map(|target| target["name"].as_str().expect("name"))
            .collect::<Vec<_>>(),
        ["alpha", "Zulu"]
    );

    let mut models = ["z-model", "A-model", "b-model"].map(|model_id| kproxy_kiro::ModelInfo {
        model_id: model_id.into(),
        model_name: model_id.into(),
        description: String::new(),
        rate_multiplier: None,
        token_limits: None,
        additional_model_request_fields_schema: None,
    });
    sort_models_for_display(&mut models);
    assert_eq!(
        models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect::<Vec<_>>(),
        ["A-model", "b-model", "z-model"]
    );
}

#[tokio::test]
async fn account_list_distinguishes_low_credit_from_exhaustion() {
    let ready = sample_account("acc_00000001", "ready@example.com", true);
    let mut protected = sample_account("acc_00000002", "protected@example.com", true);
    protected.usage = Some(Usage {
        current: 97.0,
        limit: 100.0,
        percent_used: 97.0,
        next_reset_date: None,
        updated_at: 0,
    });
    let mut exhausted = sample_account("acc_00000003", "exhausted@example.com", true);
    exhausted.usage = Some(Usage {
        current: 100.0,
        limit: 100.0,
        percent_used: 100.0,
        next_reset_date: None,
        updated_at: 0,
    });
    let (_directory, state) = state_with(vec![ready, protected, exhausted]).await;

    let list: AccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::ACCOUNT_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("account list");
    let health_by_email = list
        .accounts
        .iter()
        .map(|account| (account.email.as_str(), account.health.as_deref()))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(health_by_email["ready@example.com"], Some("available"));
    assert_eq!(health_by_email["protected@example.com"], Some("low_credit"));
    assert_eq!(health_by_email["exhausted@example.com"], Some("exhausted"));

    let protected_only: AccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::ACCOUNT_LIST,
                serde_json::json!({"status":"low_credit"}),
            ),
        )
        .await,
    ))
    .expect("protected accounts");
    assert_eq!(protected_only.accounts.len(), 1);
    assert_eq!(protected_only.accounts[0].email, "protected@example.com");

    let detail: AccountDetail = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::ACCOUNT_SHOW,
                serde_json::json!({"id":"protected@example.com"}),
            ),
        )
        .await,
    ))
    .expect("account detail");
    assert_eq!(detail.summary.health.as_deref(), Some("low_credit"));
}

#[tokio::test]
async fn model_resolution_uses_each_accounts_real_model_cache() {
    let account = sample_account("acc_00000001", "enterprise@example.com", true);
    let mut protected = sample_account("acc_00000002", "protected@example.com", true);
    protected.usage = Some(Usage {
        current: 99.0,
        limit: 100.0,
        percent_used: 99.0,
        next_reset_date: None,
        updated_at: 0,
    });
    let (_directory, state) = state_with(vec![account, protected]).await;
    state
        .pool()
        .get("acc_00000001")
        .await
        .expect("runtime account")
        .set_supported_models(["claude-opus-5".into(), "claude-sonnet-4.6".into()])
        .await;
    state
        .pool()
        .get("acc_00000002")
        .await
        .expect("protected runtime account")
        .set_supported_models(["claude-opus-4.8".into()])
        .await;

    let result: ModelResolutionResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::MODEL_RESOLVE,
                serde_json::json!({"model":"opus5"}),
            ),
        )
        .await,
    ))
    .expect("model resolution result");

    assert_eq!(result.input_model, "opus5");
    assert_eq!(result.mapped_model, "opus5");
    assert_eq!(result.resolved_model.as_deref(), Some("claude-opus-5"));
    assert_eq!(result.possible_models, ["claude-opus-5"]);
    assert_eq!(result.matched_accounts, 1);
    assert_eq!(result.total_accounts, 1);
    assert_eq!(result.accounts.len(), 2);
    assert_eq!(result.accounts[0].model_source, "account_cache");
    assert!(result.accounts[0].schedulable);
    assert_eq!(result.accounts[0].health, "available");
    assert_eq!(
        result.accounts[0].resolved_model.as_deref(),
        Some("claude-opus-5")
    );
    assert!(!result.accounts[1].schedulable);
    assert_eq!(result.accounts[1].health, "low_credit");
    assert_eq!(
        result.accounts[1].resolved_model.as_deref(),
        Some("claude-opus-4.8")
    );
}

#[tokio::test]
async fn config_reload_keeps_old_value_on_error() {
    let (_directory, state) = state_with(vec![]).await;
    tokio::fs::write(&state.paths.config_file, "[server\nport = ")
        .await
        .expect("break");
    let result: ConfigReloadResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::CONFIG_RELOAD, serde_json::json!({})),
        )
        .await,
    ))
    .expect("reload");
    assert!(!result.applied);
    assert_eq!(state.config.current().server.port, 5580);
}

#[tokio::test]
async fn config_reload_applies_service_defaults_but_not_socket_fields() {
    let (_directory, state) = state_with(vec![]).await;
    tokio::fs::write(
        &state.paths.config_file,
        "[server]\nport = 6100\n\n[features]\nenable_prompt_cache = true\n",
    )
    .await
    .expect("edit");
    let result: ConfigReloadResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::CONFIG_RELOAD, serde_json::json!({})),
        )
        .await,
    ))
    .expect("reload");
    assert!(result.applied);
    assert!(result.needs_restart.is_empty());
    assert_eq!(state.config.current().server.port, 6100);
    assert!(state.config.current().features.enable_prompt_cache);
}

#[tokio::test]
async fn config_reload_rolls_back_when_proxy_listener_cannot_start() {
    let (_directory, state) = state_with(vec![]).await;
    let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("occupied port");
    let port = occupied.local_addr().expect("address").port();
    let mut next = state.config.current().as_ref().clone();
    next.api_key.push(ApiKeyConfig {
        id: Some("ak_reload".into()),
        name: "reload".into(),
        key: "sk-reload".into(),
        format: ApiKeyFormat::Sk,
        enabled: true,
        credits_limit: None,
    });
    next.proxy_service.push(ProxyServiceConfig {
        id: "svc_reload".into(),
        name: "reload".into(),
        host: "127.0.0.1".into(),
        port,
        enabled: true,
        api_key_ids: vec!["ak_reload".into()],
        created_at: 0,
    });
    tokio::fs::write(
        &state.paths.config_file,
        toml::to_string_pretty(&next).expect("serialize"),
    )
    .await
    .expect("write config");

    let result: ConfigReloadResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::CONFIG_RELOAD, serde_json::json!({})),
        )
        .await,
    ))
    .expect("reload result");

    assert!(!result.applied);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("svc_reload")));
    assert!(state.config.current().proxy_service.is_empty());
    assert!(state.config.current().api_key.is_empty());
}

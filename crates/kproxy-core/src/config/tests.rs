use super::*;

#[test]
fn defaults_match_the_spec() {
    let config = Config::default();
    assert_eq!(config.server.host, "0.0.0.0");
    assert_eq!(config.server.port, 5580);
    assert!(config.server.enforce_user_agent_check);
    assert_eq!(config.server.max_concurrent_requests, 500);
    assert!(!config.server.tls.enabled);
    assert!(!config.server.adaptive.enabled);
    assert_eq!(config.server.adaptive.minimum_samples, 5);
    assert_eq!(config.server.adaptive.overload_error_rate, 0.05);

    assert_eq!(config.pool.max_concurrent_per_account, 50);
    assert_eq!(config.pool.max_queue_size, 10);
    assert_eq!(config.pool.max_queue_wait_ms, 30_000);
    assert_eq!(config.pool.queue_full_wait_ms, 5_000);
    assert_eq!(config.pool.low_credit_min_remaining, 4.0);
    assert_eq!(config.pool.credit_estimate_per_1k_tokens, 1.0);
    assert_eq!(config.pool.credit_estimate_output_token_cap, 8_192);
    assert!(config.pool.auto_switch_on_quota_exhausted);
    assert_eq!(config.pool.balance.weight_active, 0.5);
    assert_eq!(config.pool.balance.weight_credit, 0.4);
    assert_eq!(config.pool.balance.weight_idle, 0.1);
    assert_eq!(config.pool.balance.idle_window_ms, 300_000);
    assert_eq!(config.pool.cooldown.max_error_count, 3);
    assert_eq!(config.pool.cooldown.cooldown_ms, 30_000);
    assert_eq!(config.pool.cooldown.error_cooldown_ms, 5_000);
    assert_eq!(config.pool.cooldown.quota_reset_ms, 300_000);
    assert_eq!(config.pool.cooldown.quota_error_window_ms, 300_000);
    assert_eq!(config.pool.cooldown.quota_error_threshold, 50);

    assert_eq!(config.features.auto_continue_rounds, 0);
    assert!(config.features.enable_model_fallback);
    assert!(!config.features.enable_prompt_cache);
    assert!(config.features.enhance_system_prompt);
    assert!(config.features.buffer_tool_calls);
    assert_eq!(config.features.tool_call_buffer_delay_ms, 500);
    assert!(config.features.adaptive_thinking);
    assert_eq!(config.features.max_thinking_budget_tokens, 8192);
    assert!(config.features.enable_web_tools);
    assert!(config.features.enable_tool_leak_filter);
    assert_eq!(config.features.tool_search_max_rounds, 4);
    assert_eq!(config.features.tool_search_max_operations, 32);
    assert!(config.features.enable_tool_search);
    assert_eq!(config.features.web_search_max_rounds, 20);

    assert_eq!(config.upstream.pool.stream_pipelining, 1);
    assert_eq!(config.upstream.pool.http_pipelining, 5);
    assert_eq!(config.upstream.pool.stream_max_connections, 256);
    assert_eq!(config.upstream.pool.http_max_connections, 128);
    assert_eq!(config.upstream.token_refresh_before_expiry, 900);
    assert_eq!(config.upstream.web_search_timeout_ms, 60_000);
    assert_eq!(config.upstream.stream_slot_wait_timeout_ms, 30_000);
    assert_eq!(config.upstream.stream_read_timeout_ms, 600_000);
    assert!(config.upstream.web_search_endpoint.is_none());
    assert_eq!(config.effective_token_refresh_before_expiry(), 900);
    assert_eq!(config.context.max_input_tokens, 200_000);
    assert_eq!(config.context.safe_input_ratio, 0.95);
    assert_eq!(config.context.compact_safe_input_ratio, 0.99);
    assert!(config.context.auto_compact_on_overflow);
    assert_eq!(config.context.max_tool_input_tokens, 32_000);
    assert_eq!(config.context.max_loaded_tools, MAX_LOADED_TOOLS);
    assert_eq!(config.context.max_upstream_payload_bytes, 8 * 1024 * 1024);
    assert!(config.context.compaction_summary_model.is_empty());
    assert_eq!(config.context.compaction_summary_timeout_ms, 30_000);
    assert_eq!(config.context.compaction_preserve_recent_turns, 3);
    assert_eq!(config.notify.low_credit_threshold_percent, 10.0);
    assert_eq!(config.notify.max_notifications, 5);
    assert_eq!(config.storage.compression_threshold, 100);
    assert!(config.storage.incremental_write);
    assert_eq!(config.log.max_file_size_mb, 100);
    assert_eq!(config.log.retention_days, 3);
    assert_eq!(config.log.max_files_per_day, 3);
    assert!(config.model_mapping.is_empty());
    assert!(config.sso.start_url.is_empty());
    assert!(config.webhook.is_empty());
    assert!(config.api_key.is_empty());
    assert!(config.proxy_service.is_empty());
}

#[test]
fn default_toml_parses_into_default_config() {
    let parsed: Config = toml::from_str(DEFAULT_CONFIG_TOML).expect("default toml must parse");
    let expected = Config::default();
    assert_eq!(parsed.server.port, expected.server.port);
    assert_eq!(
        parsed.pool.balance.weight_active,
        expected.pool.balance.weight_active
    );
    assert_eq!(
        parsed.features.auto_continue_rounds,
        expected.features.auto_continue_rounds
    );
    assert_eq!(
        parsed.upstream.pool.stream_pipelining,
        expected.upstream.pool.stream_pipelining
    );
}

#[test]
fn legacy_credit_ratio_setting_is_ignored() {
    let parsed: Config =
        toml::from_str("[pool]\nlow_credit_ratio = 0.25\nlow_credit_min_remaining = 6.0\n")
            .expect("legacy credit ratio must remain forward-compatible");

    assert_eq!(parsed.pool.low_credit_min_remaining, 6.0);
}

#[test]
fn documented_default_covers_every_config_field() {
    let documented: toml::Value = toml::from_str(&uncomment_documented_settings())
        .expect("all documented settings must form valid TOML");
    let expected = fully_populated_config();
    let serialized = toml::to_string(&expected).expect("serialize populated config");
    let expected: toml::Value = toml::from_str(&serialized).expect("parse populated config");

    assert_same_config_shape(&expected, &documented, "config");
}

fn uncomment_documented_settings() -> String {
    DEFAULT_CONFIG_TOML
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let setting = trimmed.strip_prefix("# ").unwrap_or(trimmed);
            let is_table = setting.starts_with('[') && setting.ends_with(']');
            let is_value = setting.contains(" = ");
            (is_table || is_value).then_some(setting)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fully_populated_config() -> Config {
    let mut config = Config::default();
    config.server.tls.cert_path = Some("/cert.pem".into());
    config.server.tls.key_path = Some("/key.pem".into());
    config.server.tls.cert = Some("certificate".into());
    config.server.tls.key = Some("private-key".into());
    config.upstream.preferred_endpoint = Some(Endpoint::Amazonq);
    config.upstream.web_search_endpoint = Some("https://example.com/mcp".into());
    config.model_mapping.push(ModelMappingRule {
        name: "mapping".into(),
        enabled: true,
        kind: "replace".into(),
        source_models: vec!["source".into()],
        target_models: vec!["target".into()],
        priority: 1,
        weights: Some(vec![1]),
        max_remaining_credit_percent: Some(10.0),
        api_key_ids: Some(vec!["ak_example".into()]),
        schedule: Some(ModelMappingSchedule {
            mode: "daily".into(),
            days_of_week: Some(vec![1]),
            days: Some(vec!["mon".into()]),
            start_minutes: Some(540),
            start: Some("09:00".into()),
            end_minutes: Some(1080),
            end: Some("18:00".into()),
            start_at: Some(1),
            end_at: Some(2),
        }),
    });
    config.webhook.push(WebhookConfig {
        name: "webhook".into(),
        kind: "custom".into(),
        url: "https://example.com/webhook".into(),
        enabled: true,
        events: vec!["account-credit-protected".into(), "low-credit".into()],
        dingtalk_sign: Some("secret".into()),
        telegram_chat_id: Some("123".into()),
        custom_template: Some("template".into()),
    });
    config.api_key.push(ApiKeyConfig {
        id: Some("ak_example".into()),
        name: "example".into(),
        key: "sk-example".into(),
        format: ApiKeyFormat::Sk,
        enabled: true,
        credits_limit: Some(100.0),
    });
    config.proxy_service.push(ProxyServiceConfig {
        id: "svc_example".into(),
        name: "example".into(),
        host: "127.0.0.1".into(),
        port: 5580,
        enabled: true,
        api_key_ids: vec!["ak_example".into()],
        created_at: 1,
    });
    config
        .model_thinking_mode
        .insert("claude-sonnet-4.6".into(), true);
    config
        .model_thinking_mode
        .insert("claude-haiku".into(), false);
    config
}

fn assert_same_config_shape(expected: &toml::Value, actual: &toml::Value, path: &str) {
    match (expected, actual) {
        (toml::Value::Table(expected), toml::Value::Table(actual)) => {
            let expected_keys = expected.keys().collect::<BTreeSet<_>>();
            let actual_keys = actual.keys().collect::<BTreeSet<_>>();
            assert_eq!(
                actual_keys, expected_keys,
                "documented fields differ at {path}"
            );
            for (key, expected) in expected {
                assert_same_config_shape(
                    expected,
                    actual.get(key).expect("matching key checked above"),
                    &format!("{path}.{key}"),
                );
            }
        }
        (toml::Value::Array(expected), toml::Value::Array(actual)) => {
            assert!(!actual.is_empty(), "documented array is empty at {path}");
            if let Some(expected) = expected.first() {
                assert_same_config_shape(
                    expected,
                    actual.first().expect("non-empty documented array"),
                    &format!("{path}[0]"),
                );
            }
        }
        (expected, actual) => assert_eq!(
            std::mem::discriminant(expected),
            std::mem::discriminant(actual),
            "value type differs at {path}"
        ),
    }
}

#[test]
fn empty_toml_yields_defaults() {
    let parsed: Config = toml::from_str("").expect("empty toml must parse");
    assert_eq!(parsed.server.port, 5580);
    assert_eq!(parsed.pool.max_concurrent_per_account, 50);
}

#[test]
fn stream_and_log_resource_limits_must_be_positive() {
    let mut config = Config::default();
    config.upstream.stream_slot_wait_timeout_ms = 0;
    assert!(config
        .validate()
        .expect_err("zero stream timeout")
        .to_string()
        .contains("upstream.stream_timeout"));

    let mut config = Config::default();
    config.log.max_files_per_day = 0;
    assert!(config
        .validate()
        .expect_err("zero log file limit")
        .to_string()
        .contains("log"));
}

#[test]
fn loaded_tool_limit_cannot_exceed_the_proxy_ceiling() {
    let mut config = Config::default();
    config.context.max_loaded_tools = MAX_LOADED_TOOLS + 1;
    let error = config
        .validate()
        .expect_err("limits above the proxy ceiling must be rejected");
    assert!(
        error.to_string().contains("context.max_loaded_tools"),
        "{error}"
    );
}

#[test]
fn compaction_summary_limits_are_validated() {
    let mut config = Config::default();
    config.context.compaction_summary_timeout_ms = 0;
    let error = config
        .validate()
        .expect_err("zero compaction summary timeout must be rejected");
    assert!(
        error
            .to_string()
            .contains("context.compaction_summary_timeout_ms"),
        "{error}"
    );

    let mut config = Config::default();
    config.context.compaction_preserve_recent_turns = 65;
    let error = config
        .validate()
        .expect_err("unbounded retained compact turns must be rejected");
    assert!(
        error
            .to_string()
            .contains("context.compaction_preserve_recent_turns"),
        "{error}"
    );
}

#[test]
fn tool_search_operation_limit_is_positive_and_bounded() {
    for value in [0, 257] {
        let mut config = Config::default();
        config.features.tool_search_max_operations = value;
        let error = config
            .validate()
            .expect_err("unsafe Tool Search operation limit must be rejected");
        assert!(
            error
                .to_string()
                .contains("features.tool_search_max_operations"),
            "{error}"
        );
    }
}

#[test]
fn sso_start_url_requires_https_when_configured() {
    let mut config = Config::default();
    config.sso.start_url = "http://example.awsapps.com/start".into();
    let error = config
        .validate()
        .expect_err("HTTP SSO URL must be rejected");
    assert!(error.to_string().contains("sso.start_url"), "{error}");

    config.sso.start_url = "https://example.awsapps.com/start".into();
    config.validate().expect("HTTPS SSO URL must be accepted");
}

#[test]
fn token_refresh_lead_has_a_safe_runtime_floor() {
    let mut config = Config::default();
    config.upstream.token_refresh_before_expiry = 300;
    config.tasks.token_refresh_interval_ms = 300_000;
    assert_eq!(config.effective_token_refresh_before_expiry(), 600);

    config.tasks.token_refresh_interval_ms = 600_001;
    assert_eq!(config.effective_token_refresh_before_expiry(), 1_202);
}

#[test]
fn model_mapping_schedule_accepts_documented_human_format() {
    let parsed: Config = toml::from_str(
        r#"
[[model_mapping]]
name = "office hours"
type = "replace"
source_models = ["claude-opus-4*"]
target_models = ["claude-sonnet-4"]
schedule = { start = "09:00", end = "18:00", days = ["mon", "tue"] }
"#,
    )
    .expect("documented schedule");
    let schedule = parsed.model_mapping[0].schedule.as_ref().expect("schedule");
    assert_eq!(schedule.start.as_deref(), Some("09:00"));
    assert_eq!(schedule.days.as_ref().expect("days")[0], "mon");
}

#[test]
fn rejects_port_below_privileged_range() {
    let mut config = Config::default();
    config.server.port = 80;
    let error = config.validate().expect_err("port 80 must be rejected");
    assert!(error.to_string().contains("port"), "{error}");
}

#[test]
fn rejects_non_local_host_without_api_key() {
    let mut config = Config::default();
    config.proxy_service.push(ProxyServiceConfig {
        id: "svc_test".into(),
        name: "test".into(),
        host: "0.0.0.0".into(),
        port: 5580,
        enabled: true,
        api_key_ids: vec!["ak_missing".into()],
        created_at: 0,
    });
    let error = config.validate().expect_err("public bind must fail");
    assert!(error.to_string().contains("API key"), "{error}");
}

#[test]
fn accepts_non_local_host_with_enabled_api_key() {
    let mut config = Config::default();
    config.api_key.push(ApiKeyConfig {
        id: Some("ak_test".into()),
        name: "alice".into(),
        key: "sk-test".into(),
        format: ApiKeyFormat::Sk,
        enabled: true,
        credits_limit: None,
    });
    config.proxy_service.push(ProxyServiceConfig {
        id: "svc_test".into(),
        name: "test".into(),
        host: "0.0.0.0".into(),
        port: 5580,
        enabled: true,
        api_key_ids: vec!["ak_test".into()],
        created_at: 0,
    });
    config.validate().expect("public bind with key must pass");
}

#[test]
fn treats_loopback_hosts_as_local() {
    for host in ["localhost", "127.0.0.1", "::1", "[::1]"] {
        let mut config = Config::default();
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_test".into()),
            name: "alice".into(),
            key: "sk-test".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            credits_limit: None,
        });
        config.proxy_service.push(ProxyServiceConfig {
            id: "svc_test".into(),
            name: "test".into(),
            host: host.into(),
            port: 5580,
            enabled: true,
            api_key_ids: vec!["ak_test".into()],
            created_at: 0,
        });
        config
            .validate()
            .unwrap_or_else(|error| panic!("{host} should be local: {error}"));
    }
}

#[test]
fn rejects_disabled_api_key_as_public_credential() {
    let mut config = Config::default();
    config.api_key.push(ApiKeyConfig {
        id: Some("ak_test".into()),
        name: "off".into(),
        key: "sk-test".into(),
        format: ApiKeyFormat::Sk,
        enabled: false,
        credits_limit: None,
    });
    config.proxy_service.push(ProxyServiceConfig {
        id: "svc_test".into(),
        name: "test".into(),
        host: "0.0.0.0".into(),
        port: 5580,
        enabled: true,
        api_key_ids: vec!["ak_test".into()],
        created_at: 0,
    });
    assert!(config.validate().is_err());
}

#[test]
fn rejects_ratios_outside_zero_to_one() {
    let mut config = Config::default();
    config.context.safe_input_ratio = 1.5;
    assert!(config.validate().is_err());

    let mut config = Config::default();
    config.server.adaptive.overload_error_rate = f64::NAN;
    assert!(config.validate().is_err());

    let mut config = Config::default();
    config.server.adaptive.overload_error_rate = 1.1;
    assert!(config.validate().is_err());
}

#[test]
fn adaptive_feedback_sample_count_is_positive_and_bounded() {
    for minimum_samples in [0, 10_001] {
        let mut config = Config::default();
        config.server.adaptive.enabled = true;
        config.server.adaptive.minimum_samples = minimum_samples;
        let error = config
            .validate()
            .expect_err("unsafe adaptive sample count must be rejected");
        assert!(error.to_string().contains("server.adaptive"), "{error}");
    }
}

#[test]
fn rejects_invalid_credit_estimation_coefficient() {
    let mut config = Config::default();
    config.pool.credit_estimate_per_1k_tokens = f64::NAN;
    assert!(config.validate().is_err());
    config.pool.credit_estimate_per_1k_tokens = -0.1;
    assert!(config.validate().is_err());
}

#[test]
fn socket_path_honours_development_environment() {
    assert_eq!(
        default_socket_path_from(Some("/tmp/kproxy"), None),
        "/tmp/kproxy/admin.sock"
    );
    assert_eq!(
        default_socket_path_from(None, Some("/run/user/1000")),
        "/run/user/1000/kproxy/admin.sock"
    );
    assert_eq!(
        default_socket_path_from(None, None),
        "/run/kproxy/admin.sock"
    );
}

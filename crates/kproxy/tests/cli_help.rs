//! Process-level tests for the public CLI navigation contract.

use std::path::Path;
use std::process::{Command, Output};

fn run(current_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kproxy"))
        .args(args)
        .current_dir(current_dir)
        .env_remove("KPROXY_ADMIN_SOCKET")
        .env_remove("KPROXY_WRAPPER_LOCAL_ONLY")
        .env("RUST_BACKTRACE", "0")
        .output()
        .expect("run kproxy")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("UTF-8 stderr")
}

fn bash_case_options<'a>(script: &'a str, command: &str) -> &'a str {
    let marker = format!("{command})");
    let mut lines = script
        .lines()
        .skip_while(|line| line.trim() != marker)
        .skip(1);
    lines
        .find_map(|line| line.trim().strip_prefix("opts=\"")?.strip_suffix('"'))
        .unwrap_or_else(|| panic!("missing Bash completion options for {command}"))
}

#[test]
fn bare_command_groups_show_help_without_loading_the_environment() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    for group in [
        "config",
        "account",
        "diagnose",
        "tasks",
        "logs",
        "apikey",
        "service",
        "alert",
        "models",
        "model-map",
    ] {
        let output = run(workspace.path(), &[group]);
        assert!(output.status.success(), "{group}: {}", stderr(&output));
        let text = stdout(&output);
        assert!(text.contains("Usage: kproxy"), "{group}: {text}");
        assert!(text.contains("Commands:"), "{group}: {text}");
    }

    for args in [
        &["help", "--all"][..],
        &["guide", "balance"][..],
        &["completions", "zsh"][..],
        &["version"][..],
    ] {
        let output = run(workspace.path(), args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        assert!(!stdout(&output).is_empty(), "{args:?}");
    }

    let business = run(workspace.path(), &["status"]);
    assert!(!business.status.success());
    assert!(stderr(&business).contains("unterminated"));
}

#[test]
fn json_requires_an_explicit_group_action() {
    let workspace = tempfile::tempdir().expect("tempdir");
    for group in [
        "config",
        "account",
        "diagnose",
        "tasks",
        "logs",
        "apikey",
        "service",
        "alert",
        "models",
        "model-map",
    ] {
        let output = run(workspace.path(), &["--json", group]);
        assert_eq!(output.status.code(), Some(2), "{group}");
        assert!(stdout(&output).is_empty(), "{group}");
        assert!(stderr(&output).contains("需要明确指定子命令"), "{group}");
    }

    for (group, action) in [
        ("logs", "kproxy logs show"),
        ("models", "kproxy models list"),
        ("tasks", "kproxy tasks list"),
        ("diagnose", "kproxy diagnose all"),
    ] {
        let output = run(workspace.path(), &["--json", group]);
        assert!(stderr(&output).contains(action), "{group}");
    }
}

#[test]
fn nested_help_uses_the_same_clap_command_definition() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let bare_root = run(workspace.path(), &[]);
    let flag_root = run(workspace.path(), &["--help"]);
    let routed_root = run(workspace.path(), &["help"]);
    assert!(bare_root.status.success());
    assert_eq!(stdout(&bare_root), stdout(&flag_root));
    assert_eq!(stdout(&bare_root), stdout(&routed_root));

    let bare_group = run(workspace.path(), &["logs"]);
    let flag_group = run(workspace.path(), &["logs", "--help"]);
    let routed_group = run(workspace.path(), &["help", "logs"]);
    assert!(bare_group.status.success());
    assert_eq!(stdout(&bare_group), stdout(&flag_group));
    assert_eq!(stdout(&bare_group), stdout(&routed_group));
    let json_help = run(workspace.path(), &["logs", "--json", "--help"]);
    assert!(json_help.status.success());
    assert_eq!(stdout(&bare_group), stdout(&json_help));

    let flag_help = run(workspace.path(), &["logs", "trace", "--help"]);
    let routed_help = run(workspace.path(), &["help", "logs", "trace"]);
    assert!(flag_help.status.success());
    assert!(routed_help.status.success());
    assert_eq!(stdout(&flag_help), stdout(&routed_help));
    assert!(stdout(&routed_help).contains("Usage: kproxy logs trace"));
}

#[test]
fn full_tree_and_completions_expose_public_commands_only() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tree = run(workspace.path(), &["help", "--all"]);
    assert!(tree.status.success());
    let tree = stdout(&tree);
    for command in ["logs trace", "models list", "tasks list", "diagnose all"] {
        assert!(tree.contains(command), "missing {command}: {tree}");
    }
    assert!(!tree.contains("add-sso-batch"));
    assert!(!tree.contains("KPROXY_WRAPPER_LOCAL_ONLY"));

    for shell in ["bash", "zsh", "fish"] {
        let completion = run(workspace.path(), &["completions", shell]);
        assert!(
            completion.status.success(),
            "{shell}: {}",
            stderr(&completion)
        );
        let completion = stdout(&completion);
        assert!(!completion.is_empty(), "{shell}");
        assert!(completion.contains("balance"), "{shell}");
        assert!(!completion.contains("KPROXY_WRAPPER_LOCAL_ONLY"), "{shell}");
        assert!(!completion.contains("add-sso-batch"), "{shell}");
        if shell == "bash" {
            let log_options = bash_case_options(&completion, "kproxy__logs");
            for removed in ["-f", "--tail", "--level", "--account", "--follow"] {
                assert!(
                    !log_options
                        .split_whitespace()
                        .any(|option| option == removed),
                    "removed option {removed} leaked into logs completion: {log_options}"
                );
            }
            let model_options = bash_case_options(&completion, "kproxy__models");
            for removed in ["--mapped", "--refresh"] {
                assert!(
                    !model_options
                        .split_whitespace()
                        .any(|option| option == removed),
                    "removed option {removed} leaked into models completion: {model_options}"
                );
            }
            for command in ["kproxy__alert__add", "kproxy__alert__edit"] {
                let alert_options = bash_case_options(&completion, command);
                for removed in ["--kind", "--url"] {
                    assert!(
                        !alert_options
                            .split_whitespace()
                            .any(|option| option == removed),
                        "removed option {removed} leaked into {command} completion: {alert_options}"
                    );
                }
                for current in ["--platform", "--webhook-url"] {
                    assert!(
                        alert_options
                            .split_whitespace()
                            .any(|option| option == current),
                        "missing current option {current} in {command} completion: {alert_options}"
                    );
                }
            }
        }
    }
}

#[test]
fn guide_topics_are_validated_as_cli_values() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let output = run(workspace.path(), &["guide", "missing"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("possible values"));
}

#[test]
fn diagnose_account_requires_a_target_during_argument_parsing() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    let output = run(workspace.path(), &["diagnose", "account"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    let error = stderr(&output);
    assert!(error.contains("required arguments"), "{error}");
    assert!(error.contains("<ID|--all>"), "{error}");
    assert!(!error.contains("unterminated"), "{error}");
}

#[test]
fn bounded_numeric_arguments_are_validated_before_any_runtime_setup() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    for args in [
        &["diagnose", "all", "--timeout", "0s"][..],
        &["diagnose", "all", "--timeout", "301s"][..],
        &["diagnose", "all", "--concurrency", "0"][..],
        &["diagnose", "account", "--all", "--concurrency", "9"][..],
        &["logs", "show", "--tail", "0"][..],
        &["logs", "follow", "--tail", "1001"][..],
        &[
            "logs",
            "trace",
            "trace_0123456789abcdef0123456789abcdef",
            "--tail",
            "0",
        ][..],
        &[
            "logs",
            "trace",
            "trace_0123456789abcdef0123456789abcdef",
            "--tail",
            "1001",
        ][..],
    ] {
        let output = run(workspace.path(), args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        assert!(!stderr(&output).contains("unterminated"), "{args:?}");
    }
}

#[test]
fn invalid_nested_commands_fail_before_runtime_setup() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    for args in [
        &["logs", "missing"][..],
        &["logs", "trace"][..],
        &["help", "logs", "missing"][..],
        &["logs", "--tail", "10", "files"][..],
    ] {
        let output = run(workspace.path(), args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        let error = stderr(&output);
        assert!(!error.contains("unterminated"), "{args:?}: {error}");
    }
}

#[test]
fn wrapper_local_mode_rejects_business_commands() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_kproxy"))
        .args(["logs", "show"])
        .current_dir(workspace.path())
        .env("KPROXY_WRAPPER_LOCAL_ONLY", "1")
        .output()
        .expect("run local-only kproxy");
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("kproxyd 未运行"));

    let help = Command::new(env!("CARGO_BIN_EXE_kproxy"))
        .arg("logs")
        .current_dir(workspace.path())
        .env("KPROXY_WRAPPER_LOCAL_ONLY", "1")
        .output()
        .expect("run local-only help");
    assert!(help.status.success(), "{}", stderr(&help));
    assert!(stdout(&help).contains("logs show"));
}

#[test]
fn removed_command_forms_are_rejected_before_runtime_setup() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    for args in [
        &["logs", "--tail", "25"][..],
        &["logs", "--level", "error"][..],
        &["logs", "--account", "alice@example.com"][..],
        &["logs", "-f"][..],
        &["logs", "--follow"][..],
        &["models", "--mapped"][..],
        &["models", "--refresh"][..],
        &["account", "add-sso-batch", "--file", "accounts.csv"][..],
        &[
            "alert",
            "add",
            "--name",
            "ops",
            "--kind",
            "dingtalk",
            "--webhook-url",
            "https://example.com/hook",
            "--event",
            "token-refresh-failed",
        ][..],
        &[
            "alert",
            "add",
            "--name",
            "ops",
            "--platform",
            "dingtalk",
            "--url",
            "https://example.com/hook",
            "--event",
            "token-refresh-failed",
        ][..],
        &[
            "alert",
            "add",
            "--name",
            "ops",
            "--platform",
            "wechat",
            "--webhook-url",
            "https://example.com/hook",
            "--event",
            "token-refresh-failed",
        ][..],
        &["alert", "edit", "ops", "--kind", "dingtalk"][..],
        &["alert", "edit", "ops", "--url", "https://example.com/hook"][..],
        &["alert", "edit", "ops", "--platform", "wechat"][..],
        &["help", "balance"][..],
    ] {
        let output = run(workspace.path(), args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        let error = stderr(&output);
        assert!(!error.contains("unterminated"), "{args:?}: {error}");
    }
}

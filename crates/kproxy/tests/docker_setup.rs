//! Exercise the real deployment script with a strict Docker stub, never a daemon.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

struct Deployment {
    output: Output,
    calls: String,
    saved_image: Option<String>,
}

impl Deployment {
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    fn assert_success(&self) {
        assert!(
            self.output.status.success(),
            "stdout:\n{}\nstderr:\n{}\ncalls:\n{}",
            String::from_utf8_lossy(&self.output.stdout),
            self.stderr(),
            self.calls
        );
        assert!(self.stderr().is_empty(), "{}", self.stderr());
        assert!(!self
            .calls
            .contains("kiro-proxy-rollback:kiro-proxy | compose"));
    }

    fn health_calls(&self) -> usize {
        self.calls
            .lines()
            .filter(|line| line.contains(" exec -T kproxyd /usr/local/bin/kproxy health"))
            .count()
    }
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn deploy(scenario: &str, build: bool) -> Deployment {
    let workspace = tempfile::tempdir().unwrap();
    let bin = workspace.path().join("bin");
    fs::create_dir(&bin).unwrap();
    executable(
        &bin.join("docker"),
        include_str!("fixtures/docker_setup_mock.sh"),
    );
    // Keep timeout/retry tests deterministic and fast without changing production defaults.
    executable(&bin.join("sleep"), "#!/bin/sh\nexit 0\n");
    // Reproduce the production Linux volume checks on macOS test hosts as well.
    executable(&bin.join("uname"), "#!/bin/sh\necho Linux\n");
    let image_state = workspace.path().join("image-state");
    fs::write(&image_state, "kiro-proxy:test-old\n").unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new("/bin/sh")
        .arg(repo.join("deploy/docker-setup.sh"))
        .args(["--image", "kiro-proxy:test-new"])
        .arg(if build { "--build" } else { "--no-pull" })
        .args(["--timeout", "1", "--target"])
        .arg(workspace.path().join("kproxy"))
        .env_clear()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin.display()),
        )
        .env("DOCKER_SETUP_TEST_DIR", workspace.path())
        .env("DOCKER_SETUP_TEST_SCENARIO", scenario)
        .env("KPROXY_IMAGE_STATE_FILE", &image_state)
        .env("KPROXY_BUILD_IMAGE", "kiro-proxy:test-new")
        .output()
        .unwrap();
    let calls = fs::read_to_string(workspace.path().join("docker-calls")).unwrap();
    // The stub rejects unrecognized commands instead of masking harness mistakes.
    assert!(!calls.contains("UNEXPECTED"), "{calls}");
    Deployment {
        output,
        calls,
        saved_image: fs::read_to_string(image_state).ok(),
    }
}

#[test]
fn healthy_deployment_uses_supported_exec_flag_without_rollback() {
    for build in [false, true] {
        let result = deploy("healthy", build);
        result.assert_success();
        assert_eq!(result.health_calls(), 1);
        assert_eq!(
            result.saved_image.as_deref(),
            Some(if build {
                "kiro-proxy:test-old\n"
            } else {
                "kiro-proxy:test-new\n"
            })
        );
    }
}

#[test]
fn transient_health_failure_is_retried_without_rollback() {
    for build in [false, true] {
        let result = deploy("transient", build);
        result.assert_success();
        assert_eq!(result.health_calls(), 2);
    }
}

#[test]
fn unhealthy_deployment_reports_the_error_and_restores_previous_image() {
    for build in [false, true] {
        let result = deploy("unhealthy", build);
        assert!(!result.output.status.success());
        let stderr = result.stderr();
        assert!(
            stderr.contains("Health check failed after 1s (exit 7)"),
            "{stderr}"
        );
        assert!(
            stderr.contains("admin socket unavailable: kiro-proxy:test-new"),
            "{stderr}"
        );
        assert!(
            stderr.contains("the previous image was restored successfully"),
            "{stderr}"
        );
        assert_eq!(result.health_calls(), 3);
        assert!(result
            .calls
            .contains("kiro-proxy-rollback:kiro-proxy | compose"));
        assert_eq!(result.saved_image.as_deref(), Some("kiro-proxy:test-old\n"));
    }
}

#[test]
fn unhealthy_rollback_reports_its_own_health_error() {
    for build in [false, true] {
        let result = deploy("rollback-unhealthy", build);
        assert!(!result.output.status.success());
        let stderr = result.stderr();
        assert_eq!(
            stderr
                .matches("Health check failed after 1s (exit 7)")
                .count(),
            2
        );
        assert!(
            stderr.contains("admin socket unavailable: kiro-proxy:test-new"),
            "{stderr}"
        );
        assert!(
            stderr.contains("admin socket unavailable: kiro-proxy-rollback:kiro-proxy"),
            "{stderr}"
        );
        assert!(
            stderr.contains("automatic rollback did not become healthy"),
            "{stderr}"
        );
        assert_eq!(result.health_calls(), 4);
        assert_eq!(result.saved_image.as_deref(), Some("kiro-proxy:test-old\n"));
    }
}

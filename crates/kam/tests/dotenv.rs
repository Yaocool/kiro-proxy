//! `.env` 必须在 Clap 读取环境参数前加载。

use std::process::Command;

#[test]
fn dotenv_socket_is_loaded_before_cli_argument_parsing() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let socket = workspace.path().join("from-dotenv.sock");
    std::fs::write(
        workspace.path().join(".env"),
        format!("KAM_ADMIN_SOCKET={}\n", socket.display()),
    )
    .expect("write .env");

    let output = Command::new(env!("CARGO_BIN_EXE_kam"))
        .current_dir(workspace.path())
        .env_remove("KAM_ADMIN_SOCKET")
        .arg("status")
        .output()
        .expect("run kam");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(
        stderr.contains(&socket.display().to_string()),
        "CLI did not use KAM_ADMIN_SOCKET from .env: {stderr}"
    );
}

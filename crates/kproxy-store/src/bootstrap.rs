//! 首次运行时创建目录与默认文件。

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use kproxy_core::config::{Config, DEFAULT_CONFIG_TOML};
use kproxy_core::paths::Paths;

use crate::atomic::write_bytes_atomically;

/// 首次运行创建了哪些文件。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootstrapReport {
    /// 是否创建了配置文件。
    pub created_config: bool,
    /// 是否创建了账号库。
    pub created_accounts: bool,
    /// 是否创建了日额度文件。
    pub created_daily: bool,
    /// 是否创建了统计文件。
    pub created_stats: bool,
}

impl BootstrapReport {
    /// 是否创建了任何文件。
    pub fn created_anything(&self) -> bool {
        self.created_config || self.created_accounts || self.created_daily || self.created_stats
    }
}

/// 确保配置与数据文件齐备，已存在文件绝不覆盖。
pub async fn ensure_layout(paths: &Paths) -> Result<BootstrapReport> {
    tokio::fs::create_dir_all(&paths.config_dir)
        .await
        .with_context(|| format!("create config dir {}", paths.config_dir.display()))?;
    tokio::fs::create_dir_all(&paths.data_dir)
        .await
        .with_context(|| format!("create data dir {}", paths.data_dir.display()))?;
    set_dir_mode(&paths.data_dir, 0o700).await?;

    // DEFAULT_CONFIG_TOML 面向生产默认使用 /run。开发与容器使用 KPROXY_HOME 时，
    // 将首次生成配置中的 socket 写成环境解析结果，保证裸启动无需 root 权限。
    let socket = Config::default().admin.socket;
    let generated_config = DEFAULT_CONFIG_TOML.replace(
        "socket = \"/run/kproxy/admin.sock\"",
        &format!("socket = \"{socket}\""),
    );

    Ok(BootstrapReport {
        created_config: create_if_absent(
            &paths.config_file,
            generated_config.as_bytes(),
            Some(0o600),
        )
        .await?,
        created_accounts: create_if_absent(&paths.accounts_file, b"[]\n", Some(0o600)).await?,
        created_daily: create_if_absent(&paths.daily_file, b"{}\n", Some(0o600)).await?,
        created_stats: create_if_absent(&paths.stats_file, b"{}\n", Some(0o600)).await?,
    })
}

async fn create_if_absent(path: &Path, contents: &[u8], mode: Option<u32>) -> Result<bool> {
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
        return Ok(false);
    }
    write_bytes_atomically(path, contents, mode).await?;
    Ok(true)
}

#[cfg(unix)]
async fn set_dir_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(mode);
    tokio::fs::set_permissions(path, permissions)
        .await
        .with_context(|| format!("chmod {:o} {}", mode, path.display()))
}

#[cfg(not(unix))]
async fn set_dir_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// 创建 socket 父目录并清理陈旧 socket。
///
/// 如果已有 socket 可连接，则视为另一个 daemon 正在运行并拒绝删除。
pub async fn ensure_socket_parent(socket_path: &Path) -> Result<()> {
    let parent = socket_path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("socket path has no parent: {}", socket_path.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("create socket directory {}", parent.display()))?;

    if tokio::fs::try_exists(socket_path).await.unwrap_or(false) {
        #[cfg(unix)]
        if tokio::net::UnixStream::connect(socket_path).await.is_ok() {
            return Err(anyhow!(
                "admin socket {} is already accepting connections",
                socket_path.display()
            ));
        }
        tokio::fs::remove_file(socket_path)
            .await
            .with_context(|| format!("remove stale socket {}", socket_path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

    fn paths_under(root: &Path) -> Paths {
        Paths::from_env_values(Some(root.to_str().expect("utf8 path")), None, None, None)
    }

    #[tokio::test]
    async fn creates_every_file_and_preserves_existing_config() {
        let directory = tempdir().expect("tempdir");
        let paths = paths_under(directory.path());
        let report = ensure_layout(&paths).await.expect("bootstrap");
        assert!(report.created_config);
        assert!(report.created_accounts);
        assert!(report.created_daily);
        assert!(report.created_stats);
        let config: Config = toml::from_str(
            &tokio::fs::read_to_string(&paths.config_file)
                .await
                .expect("read config"),
        )
        .expect("parse config");
        config.validate().expect("valid config");

        tokio::fs::write(&paths.config_file, "[server]\nport = 6000\n")
            .await
            .expect("edit");
        let second = ensure_layout(&paths).await.expect("bootstrap twice");
        assert!(!second.created_anything());
        assert_eq!(
            tokio::fs::read_to_string(&paths.config_file)
                .await
                .expect("read edit"),
            "[server]\nport = 6000\n"
        );
    }

    #[tokio::test]
    async fn accounts_start_empty_with_restrictive_permissions() {
        let directory = tempdir().expect("tempdir");
        let paths = paths_under(directory.path());
        ensure_layout(&paths).await.expect("bootstrap");
        let accounts: Vec<kproxy_core::account::Account> = serde_json::from_str(
            &tokio::fs::read_to_string(&paths.accounts_file)
                .await
                .expect("read accounts"),
        )
        .expect("parse accounts");
        assert!(accounts.is_empty());
        assert_eq!(
            std::fs::metadata(&paths.data_dir)
                .expect("data metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&paths.accounts_file)
                .expect("accounts metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn removes_stale_socket_and_creates_parent() {
        let directory = tempdir().expect("tempdir");
        let socket = directory.path().join("nested/admin.sock");
        tokio::fs::create_dir_all(socket.parent().expect("parent"))
            .await
            .expect("mkdir");
        tokio::fs::write(&socket, b"stale").await.expect("stale");
        ensure_socket_parent(&socket).await.expect("ensure");
        assert!(!socket.exists());
        ensure_socket_parent(&socket).await.expect("idempotent");
    }
}

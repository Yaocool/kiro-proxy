//! 原子文件读写。

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use anyhow::{Context, Result};
use serde::Serialize;
use tokio::sync::Mutex;

const RETRY_DELAYS_MS: [u64; 6] = [25, 50, 100, 200, 400, 800];

fn path_locks() -> &'static StdMutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    static LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn lock_for(path: &Path) -> Arc<Mutex<()>> {
    let mut map = match path_locks().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    map.entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn is_retryable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::PermissionDenied
            | ErrorKind::AlreadyExists
            | ErrorKind::Interrupted
            | ErrorKind::WouldBlock
    )
}

/// 判断错误是否为文件不存在。
pub fn is_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|cause| cause.kind() == ErrorKind::NotFound)
}

/// 原子写入字节。
///
/// 同目录临时文件写完并 fsync 后，再 rename 覆盖目标。同一路径的写入
/// 由进程内 mutex 串行化，调用者不会观察到半写文档。
pub async fn write_bytes_atomically(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    let path_lock = lock_for(path);
    let _held = path_lock.lock().await;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create parent dir for {}", path.display()))?;
        }
    }

    let temporary = temp_path_for(path);
    if let Err(error) = write_temp_file(&temporary, bytes, mode).await {
        let _cleanup = tokio::fs::remove_file(&temporary).await;
        return Err(error);
    }

    let mut attempt = 0usize;
    loop {
        match tokio::fs::rename(&temporary, path).await {
            Ok(()) => {
                sync_parent_directory(path).await?;
                return Ok(());
            }
            Err(error) if attempt < RETRY_DELAYS_MS.len() && is_retryable(&error) => {
                tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAYS_MS[attempt]))
                    .await;
                attempt += 1;
            }
            Err(error) => {
                let _cleanup = tokio::fs::remove_file(&temporary).await;
                return Err(anyhow::Error::new(error)
                    .context(format!("rename temp file onto {}", path.display())));
            }
        }
    }
}

fn temp_path_for(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let unique = format!(
        ".{}.{}.{}.tmp",
        name,
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from(&unique), |parent| parent.join(&unique))
}

async fn write_temp_file(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(value) = mode {
        options.mode(value);
    }
    #[cfg(not(unix))]
    let _ignored_mode = mode;

    let mut file = options
        .open(path)
        .await
        .with_context(|| format!("create temp file {}", path.display()))?;
    file.write_all(bytes)
        .await
        .context("write temp file contents")?;
    file.flush().await.context("flush temp file")?;
    file.sync_all().await.context("fsync temp file")?;
    Ok(())
}

#[cfg(unix)]
async fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent().map(Path::to_path_buf) else {
        return Ok(());
    };
    tokio::task::spawn_blocking(move || {
        std::fs::File::open(&parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("fsync directory {}", parent.display()))
    })
    .await
    .context("join directory fsync task")??;
    Ok(())
}

#[cfg(not(unix))]
async fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}

/// 原子写入格式化 JSON。
pub async fn write_json_atomically<T: Serialize>(
    path: &Path,
    value: &T,
    mode: Option<u32>,
) -> Result<()> {
    let raw = serde_json::to_vec_pretty(value)
        .with_context(|| format!("serialize json for {}", path.display()))?;
    write_bytes_atomically(path, &raw, mode).await
}

/// 读文本；遇到瞬时文件系统错误时阶梯重试。
pub async fn read_to_string_with_retry(path: &Path) -> Result<String> {
    let mut attempt = 0usize;
    loop {
        match tokio::fs::read_to_string(path).await {
            Ok(content) => return Ok(content),
            Err(error) if attempt < RETRY_DELAYS_MS.len() && is_retryable(&error) => {
                tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAYS_MS[attempt]))
                    .await;
                attempt += 1;
            }
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!("read {}", path.display())));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn writes_overwrites_and_leaves_no_temp_files() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("a.txt");
        write_bytes_atomically(&path, b"aaaaaaaa", None)
            .await
            .expect("first write");
        write_bytes_atomically(&path, b"bb", None)
            .await
            .expect("second write");
        assert_eq!(read_to_string_with_retry(&path).await.expect("read"), "bb");
        let entries = std::fs::read_dir(directory.path())
            .expect("read dir")
            .collect::<Result<Vec<_>, _>>()
            .expect("entries");
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn concurrent_writes_do_not_interleave() {
        let directory = tempdir().expect("tempdir");
        let path = Arc::new(directory.path().join("a.txt"));
        let first = "a".repeat(4_096);
        let second = "b".repeat(2_048);
        let first_path = Arc::clone(&path);
        let second_path = Arc::clone(&path);
        let first_copy = first.clone();
        let second_copy = second.clone();
        let one = tokio::spawn(async move {
            write_bytes_atomically(&first_path, first_copy.as_bytes(), None).await
        });
        let two = tokio::spawn(async move {
            write_bytes_atomically(&second_path, second_copy.as_bytes(), None).await
        });
        one.await.expect("join one").expect("write one");
        two.await.expect("join two").expect("write two");
        let actual = read_to_string_with_retry(&path).await.expect("read");
        assert!(actual == first || actual == second);
    }

    #[tokio::test]
    async fn json_roundtrips_and_missing_is_detectable() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("a.json");
        write_json_atomically(&path, &vec![1u32, 2, 3], None)
            .await
            .expect("write json");
        let actual: Vec<u32> =
            serde_json::from_str(&read_to_string_with_retry(&path).await.expect("read json"))
                .expect("parse json");
        assert_eq!(actual, vec![1, 2, 3]);

        let error = read_to_string_with_retry(&directory.path().join("missing"))
            .await
            .expect_err("missing");
        assert!(is_missing(&error));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn applies_requested_mode_and_creates_parents() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("deep/secret.json");
        write_bytes_atomically(&path, b"{}", Some(0o600))
            .await
            .expect("write");
        let mode = std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

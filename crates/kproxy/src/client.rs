//! 管理面 Unix socket 客户端。

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use kproxy_ipc::protocol::{decode_line, encode_line, Request, Response};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// 管理面客户端。
pub struct AdminClient {
    socket: PathBuf,
    next_id: u64,
}

impl AdminClient {
    /// 指定 socket 路径构造客户端。
    pub fn connect(socket: PathBuf) -> Self {
        Self { socket, next_id: 1 }
    }

    /// 返回当前管理 socket，供受限并发的批量命令创建独立连接。
    pub fn socket_path(&self) -> PathBuf {
        self.socket.clone()
    }

    /// 调用一个方法并反序列化结果。
    pub async fn call<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        let id = self.next_id;
        self.next_id += 1;
        let stream = UnixStream::connect(&self.socket).await.with_context(|| {
            format!(
                "无法连接 {}。kproxyd 未运行？可用 `systemctl status kproxyd` 检查服务",
                self.socket.display()
            )
        })?;
        let (read_half, mut write_half) = stream.into_split();
        let line = encode_line(&Request::new(id, method, params))?;
        write_half
            .write_all(line.as_bytes())
            .await
            .context("发送请求失败")?;
        write_half.flush().await.context("刷新请求失败")?;

        let raw = BufReader::new(read_half)
            .lines()
            .next_line()
            .await
            .context("读取响应失败")?
            .ok_or_else(|| anyhow!("服务端在响应前关闭了连接"))?;
        match decode_line::<Response>(&raw)? {
            Response::Ok { result, .. } => {
                serde_json::from_value(result).context("解析响应结果失败")
            }
            Response::Err { error, .. } => {
                Err(anyhow!("服务端返回错误 {}: {}", error.code, error.message))
            }
        }
    }
}

/// 解析 socket 路径：命令行参数、配置文件、默认值。
pub async fn resolve_socket(explicit: Option<String>) -> PathBuf {
    if let Some(path) = explicit {
        return PathBuf::from(path);
    }
    let paths = kproxy_core::paths::Paths::from_env();
    match kproxy_store::config_loader::load_config(&paths.config_file).await {
        Ok(config) => PathBuf::from(config.admin.socket),
        Err(_) => PathBuf::from(kproxy_core::config::Config::default().admin.socket),
    }
}

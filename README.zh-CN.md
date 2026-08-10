# kiro-proxy

[English](README.md) | [简体中文](README.zh-CN.md)

`kiro-proxy` 是一个无头 Rust 服务，在 Kiro 上游服务之上提供兼容 Claude Messages 和
OpenAI Chat Completions 的 API，并包含多账号调度、自动 Token 刷新、端点切换、模型映射、
API Key 限额、TLS、Webhook、统计和运维 CLI。

本仓库不包含 GUI、KProxy MITM 或本机 Kiro 应用配置修改功能。

## 主要能力

- 兼容 Claude 的 `/v1/messages` 和 `/v1/messages/count_tokens` 端点。
- 兼容 OpenAI 的 `/v1/chat/completions` 和 `/v1/models` 端点。
- 带单账号并发限制、冷却、额度追踪和模型兼容检查的多账号加权调度。
- IdC 与社交账号 Token 自动刷新，并对同一账号做 singleflight 防并发刷新。
- 根据账号选择 Amazon Q 或 CodeWhisperer，使用有界的进程内可用性缓存。
- 动态模型发现，以及模型别名、替换、负载均衡和降级规则。
- 通过 Unix socket 和 `kam` CLI 管理，不依赖浏览器界面。
- TOML 热重载、结构化日志、Trace ID、统计、API Key 限额、TLS 和 Webhook 告警。
- `kamd` 与 `kam` 每次启动都会自动读取 `.env`。

## Workspace 结构

| 组件 | 作用 |
| --- | --- |
| `kamd` | 常驻代理服务和管理服务端。 |
| `kam` | 无头管理 CLI。 |
| `kam-core` | 领域模型、默认值和配置校验。 |
| `kam-store` | 原子持久化、`.env` 加载、首次初始化和热重载。 |
| `kam-ipc` | daemon 与 CLI 共用的行分隔 JSON-RPC 协议。 |
| `kam-translate` | Claude/OpenAI/Kiro 协议转换、校验和 Token 估算。 |
| `kam-kiro` | Kiro HTTP 客户端、Event Stream 解码、端点状态和模型发现。 |
| `kam-pool` | 账号健康、额度预留、并发和加权调度。 |
| `kam-notify` | Webhook 发送、重试、抑制和额度告警。 |

CLI 源码位于 [`crates/kam`](crates/kam)，daemon 源码位于
[`crates/kamd`](crates/kamd)。

## 快速开始

项目通过 `rust-toolchain.toml` 自动选择固定的 Rust 1.97.1 工具链。

```bash
cargo build --release --locked

# 两个二进制每次启动时都会读取该文件。
cp .env.example .env

# 首次启动会创建 config.toml、accounts.json、daily.json 和 stats.json。
./target/release/kamd
```

在另一个终端运行：

```bash
./target/release/kam status
./target/release/kam config path
./target/release/kam account list
```

`kamd` 和 `kam` 会从当前目录开始向上查找 `.env`。已经存在的进程环境变量优先于
`.env` 中的同名值，因此仍可做单次启动覆盖：

```bash
KAM_HTTP_PORT=5581 ./target/release/kamd
```

业务 API 默认监听 `http://127.0.0.1:5580`：

```text
POST /v1/messages
POST /v1/messages/count_tokens
POST /v1/chat/completions
GET  /v1/models
GET  /health
```

同时支持 Claude 别名 `/messages`、`/anthropic/v1/messages`，以及 OpenAI 别名
`/chat/completions`、`/models`。

## 配置与文件

`.env` 用于启动路径选择和临时进程级覆盖；`config.toml` 用于持久化服务、账号池、模型、
API Key、TLS、日志和通知配置。所有示例环境变量及其作用见
[`.env.example`](.env.example)。

设置 `KAM_HOME` 后，配置、数据、日志和管理 socket 会统一放到该目录。未设置时遵循
XDG 目录：

| 文件 | 默认位置 | 说明 |
| --- | --- | --- |
| `config.toml` | `${XDG_CONFIG_HOME:-~/.config}/kam/` | 人工维护的 daemon 配置。 |
| `accounts.json` | `${XDG_DATA_HOME:-~/.local/share}/kam/` | 包含凭证，创建权限为 `0600`。 |
| `daily.json` | `${XDG_DATA_HOME:-~/.local/share}/kam/` | 按 UTC 日期重置的每日额度记录。 |
| `stats.json` | `${XDG_DATA_HOME:-~/.local/share}/kam/` | 持久化请求聚合统计。 |
| `admin.sock` | `${XDG_RUNTIME_DIR}/kam/` 或 `/run/kam/` | 本地管理面。 |
| 日志 | `${XDG_DATA_HOME:-~/.local/share}/kam/logs/` | 按 UTC 日期和级别拆分。 |

首次启动只创建缺失文件，不覆盖已有数据。有效配置修改会自动热重载；TOML 格式错误或校验
失败时继续使用上一份有效配置。修改 `server.host`、`server.port`、`admin.socket` 或
HTTP/HTTPS 监听模式需要重启 daemon，其余大部分配置无需重启。

外部修改账号文件也会自动载入，损坏的账号数据不会替换内存中的有效快照。账号数较多时，
可根据存储配置使用 gzip envelope 和增量 sidecar。

## 导入账号

从 JSON 文件或 stdin 导入现有凭证：

```bash
kam account import --file accounts.json
cat accounts.json | kam account import --stdin
```

`id`、`machine_id` 和 `created_at` 可以省略，CLI 会自动生成。

```json
[
  {
    "email": "user@example.com",
    "credentials": {
      "access_token": "...",
      "refresh_token": "...",
      "client_id": "...",
      "client_secret": "...",
      "region": "us-east-1",
      "expires_at": 1767225600,
      "auth_method": "idc"
    }
  }
]
```

账号导出默认包含凭证。分享诊断结果前应使用 `--redact`：

```bash
kam --json account export --redact
```

## 常用 CLI 命令

```bash
kam status
kam account list
kam account show <id|email>
kam account tag <id|email> --add prod
kam account disable <id|email>
kam account refresh <id|email>
kam account refresh --all
kam account probe --all
kam account regen-machine-id <id|email>
kam account rm <id|email> --yes

kam config show --effective
kam config path
kam config validate
kam config reload

kam pool --watch --explain
kam diagnose endpoints
kam diagnose account --all -c 4 --timeout 45s
kam subscriptions
kam models --mapped
kam model-map test claude-opus-4

kam apikey list
kam webhook test --all
kam stats --since 1h --by endpoint
kam logs -f --level warn
kam tasks
kam tasks run status_check
```

所有命令都支持全局 `--json`，权威参数列表以 `kam --help` 和各子命令的 `--help` 为准。

## SSO 构建

默认 slim 构建支持凭证导入，不包含 Chromium。为 `kamd` 启用 `sso` feature 后，可以使用
IAM Identity Center 浏览器自动登录：

```bash
cargo build --release --locked -p kamd --features sso

printf '%s\n' "$PASSWORD" | kam account add-sso \
  --email user@example.com \
  --start-url https://example.awsapps.com/start \
  --password-stdin

kam account add-sso --batch accounts.csv \
  --start-url https://example.awsapps.com/start -c 1
```

密码只从 stdin 或两列 CSV 文件读取。遇到 MFA 或上游页面变化需要手工操作时，增加
`--headful`。

## Docker 与 systemd

```bash
docker build --target runtime-slim -t kamd:slim .
docker build --target runtime-full -t kamd:full .
docker compose up -d
```

Compose 只在宿主机发布 `127.0.0.1:5580`，数据保存在 `kam-data` volume。full 镜像额外
安装 Chromium，用于 SSO。

加固后的服务模板位于 [`deploy/kamd.service`](deploy/kamd.service)。将 `kamd` 和
`kam` 安装到 `/usr/local/bin`，创建 `kam` 系统用户与用户组，安装 unit 后即可启用服务。

## 开发

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

项目使用 Rust edition 2021，MSRV 为 1.97.1。

完整的启动、部署、日志、LLDB 和故障排查说明见
[启动与调试指南](docs/startup-and-debugging.zh-CN.md)。

## 许可

MIT

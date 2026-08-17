# kiro-proxy

[English](README.md) | [简体中文](README.zh-CN.md)

`kiro-proxy` 是一个无头 Rust 服务，在 Kiro 上游服务之上提供兼容 Claude Messages 和
OpenAI Chat Completions 的 API，并包含多账号调度、自动 Token 刷新、端点切换、模型映射、
API Key 限额、TLS、Webhook、统计和运维 CLI。

> [!IMPORTANT]
> 本项目仅支持通过企业 SSO（AWS IAM Identity Center/IdC）认证的 Kiro 企业账号。
> 其他所有账号和认证类型均不支持，包括个人账号和社交登录账号。

本仓库不包含 GUI、KProxy MITM 或本机 Kiro 应用配置修改功能。

## 主要能力

- 兼容 Claude 的 `/v1/messages` 和 `/v1/messages/count_tokens` 端点。
- 兼容 OpenAI 的 `/v1/chat/completions` 和 `/v1/models` 端点。
- 带单账号并发限制、冷却、额度追踪和模型兼容检查的多账号加权调度。
- Kiro 企业账号的 IdC/SSO Token 自动刷新，并对同一账号做 singleflight 防并发刷新。
- 根据账号选择 Amazon Q 或 CodeWhisperer，使用有界的进程内可用性缓存。
- 动态模型发现，以及模型别名、替换、负载均衡和降级规则。
- 通过 Unix socket 和 `kproxy` CLI 管理，不依赖浏览器界面。
- TOML 热重载、结构化日志、Trace ID、统计、API Key 限额、TLS 和 Webhook 告警。
- `kproxyd` 与 `kproxy` 每次启动都会自动读取 `.env`。

## Workspace 结构

| 组件 | 作用 |
| --- | --- |
| `kproxyd` | 常驻代理服务和管理服务端。 |
| `kproxy` | 无头管理 CLI。 |
| `kproxy-core` | 领域模型、默认值和配置校验。 |
| `kproxy-store` | 原子持久化、`.env` 加载、首次初始化和热重载。 |
| `kproxy-ipc` | daemon 与 CLI 共用的行分隔 JSON-RPC 协议。 |
| `kproxy-translate` | Claude/OpenAI/Kiro 协议转换、校验和 Token 估算。 |
| `kproxy-kiro` | Kiro HTTP 客户端、Event Stream 解码、端点状态和模型发现。 |
| `kproxy-pool` | 账号健康、额度预留、并发和加权调度。 |
| `kproxy-notify` | Webhook 发送、重试、抑制和额度告警。 |

CLI 源码位于 [`crates/kproxy`](crates/kproxy)，daemon 源码位于
[`crates/kproxyd`](crates/kproxyd)。

## 安装与启动

### Docker Compose（Linux 服务器推荐）

最简生产部署只需要 Docker Engine 和 Compose v2 插件。Compose 会构建 slim 镜像、使用
host network 启动 `kproxyd`，并将全部状态保存在 `kproxy-data` named volume 中。以下命令均在
仓库根目录执行：

```bash
docker compose up -d --build
docker compose ps
docker compose logs -f kproxyd
```

全新 daemon 只开放 Unix 管理 socket，不会自动创建业务代理。显式创建首个 service，并
立即保存命令输出的 API Key：

```bash
docker compose exec kproxyd kproxy status
docker compose exec kproxyd kproxy service create --name main
docker compose exec kproxyd kproxy service list
```

发送生成请求前，至少导入一个受支持的 Kiro 企业 SSO 账号：

```bash
docker compose exec -T kproxyd kproxy account import --stdin < accounts.json
docker compose exec kproxyd kproxy account probe --all
```

默认监听 `0.0.0.0:5580`，应使用宿主机防火墙或云安全组限制该端口；只允许 Docker 宿主机
访问时增加 `--host 127.0.0.1`。除非确实要清除配置、账号、用量和日志，否则不要执行
`docker compose down -v`。

### 本地二进制

项目通过 `rust-toolchain.toml` 自动选择固定的 Rust 1.97.1 工具链。

```bash
cp .env.example .env
cargo build --release --locked

# 首次启动会创建 config.toml、accounts.json、daily.json 和 stats.json。
./target/release/kproxyd
```

在另一个终端运行：

```bash
./target/release/kproxy health
./target/release/kproxy status
./target/release/kproxy service create --name main
./target/release/kproxy config path
./target/release/kproxy account list
```

`kproxy service create` 会创建并启动服务、为该服务创建首个 API Key，并返回明文 Key。之后可
使用 `kproxy service apikeys main --show-secret` 查询该 service 绑定的 Key。

`kproxyd` 和 `kproxy` 会从当前目录开始向上查找 `.env`。已经存在的进程环境变量优先于
`.env` 中的同名值，因此仍可做单次启动覆盖：

```bash
KPROXY_HTTP_PORT=5581 ./target/release/kproxyd
```

使用默认参数创建 `main` 后，业务 API 绑定 `0.0.0.0:5580`；在本机可通过
`http://127.0.0.1:5580` 访问：

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

设置 `KPROXY_HOME` 后，配置、数据、日志和管理 socket 会统一放到该目录。未设置时遵循
XDG 目录：

| 文件 | 默认位置 | 说明 |
| --- | --- | --- |
| `config.toml` | `${XDG_CONFIG_HOME:-~/.config}/kproxy/` | 人工维护的 daemon 配置。 |
| `accounts.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | 包含凭证，创建权限为 `0600`。 |
| `daily.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | 按 UTC 日期重置的每日额度记录。 |
| `stats.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | 持久化请求聚合统计。 |
| `admin.sock` | `${XDG_RUNTIME_DIR}/kproxy/` 或 `/run/kproxy/` | 本地管理面。 |
| 日志 | `${XDG_DATA_HOME:-~/.local/share}/kproxy/logs/` | 按 UTC 日期和级别拆分。 |

首次启动只创建缺失文件，不覆盖已有数据。有效配置修改会自动热重载；TOML 格式错误或校验
失败时继续使用上一份有效配置。`server.host` 和 `server.port` 是新建代理服务时使用的
默认值。修改 `admin.socket` 或共享的 HTTP/HTTPS 监听模式需要重启 daemon；包括代理服务
列表在内的其余大部分配置无需重启。

外部修改账号文件也会自动载入，损坏的账号数据不会替换内存中的有效快照。账号数较多时，
可根据存储配置使用 gzip envelope 和增量 sidecar。

## 导入企业 SSO 账号

只能导入由组织 SSO 签发给 Kiro 企业账号的凭证。导入操作不会让个人账号、社交登录账号或
其他账号类型变为可用。从 JSON 文件或 stdin 导入受支持的凭证：

```bash
kproxy account import --file accounts.json
cat accounts.json | kproxy account import --stdin
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
kproxy --json account export --redact
```

## 常用 CLI 命令

```bash
kproxy status
kproxy health
kproxy service list
kproxy service create --name main --port 5580
kproxy service apikeys main
kproxy service apikeys main --show-secret
kproxy service delete main --yes
kproxy account list
kproxy account show <id|email>
kproxy account tag <id|email> --add prod
kproxy account disable <id|email>
kproxy account refresh <id|email>
kproxy account refresh --all
kproxy account probe --all
kproxy account regen-machine-id <id|email>
kproxy account rm <id|email> --yes

kproxy config show --effective
kproxy config path
kproxy config validate
kproxy config reload

kproxy pool --watch --explain
kproxy diagnose endpoints
kproxy diagnose account --all -c 4 --timeout 45s
kproxy subscriptions
kproxy models --mapped
kproxy model-map test claude-opus-4

kproxy apikey list
kproxy webhook test --all
kproxy stats --since 1h --by endpoint
kproxy logs -f --level warn
kproxy tasks
kproxy tasks run status_check
```

所有命令都支持全局 `--json`，权威参数列表以 `kproxy --help` 和各子命令的 `--help` 为准。

## 企业 SSO 认证

默认 slim 构建支持导入已有的 Kiro 企业 SSO 凭证，不包含 Chromium。为 `kproxyd` 启用
`sso` feature 后，可以通过企业 IAM Identity Center 登录流程认证受支持的账号：

```bash
cargo build --release --locked -p kproxyd --features sso

printf '%s\n' "$PASSWORD" | kproxy account add-sso \
  --email user@example.com \
  --start-url https://example.awsapps.com/start \
  --password-stdin

kproxy account add-sso --batch accounts.csv \
  --start-url https://example.awsapps.com/start -c 1
```

密码只从 stdin 或两列 CSV 文件读取。遇到 MFA 或上游页面变化需要手工操作时，增加
`--headful`。该流程不会增加对非企业账号或非 SSO 认证方式的支持。

## Docker 与 systemd

```bash
docker compose up -d --build

# 不使用 Compose 时可单独构建镜像：
docker build --target runtime-slim -t kproxyd:slim .
docker build --target runtime-full -t kproxyd:full .
```

Compose 使用 host network，因此手动创建的任意端口代理服务都会立即在 Docker 宿主机
可访问，无需修改 Compose 或重启容器。服务默认绑定 `0.0.0.0`，应通过宿主机防火墙或云
安全组限制代理端口；只需宿主机本地访问时可使用 `--host 127.0.0.1`。Linux Docker
Engine 可直接使用；Docker Desktop 4.34+ 需要在设置中启用 host networking。数据保存
在 `kproxy-data` volume，full 镜像为企业 SSO 认证额外安装 Chromium。

源码更新后，重新构建并创建容器即可，不要删除 named volume：

```bash
docker compose up -d --build
docker compose exec kproxyd kproxy config show --effective
```

已有 `config.toml` 永远不会被覆盖。旧版本创建的 volume 可能仍包含
`server.host = "127.0.0.1"`；如果需要采用新的 `0.0.0.0` 默认值，请使用
`kproxy config edit` 修改。

在宿主机安装一次 Docker 包装器后，可直接使用 `kproxy`，无需再输入 `docker compose exec`：

```bash
sudo ./deploy/install-kproxy-wrapper.sh
kproxy health
kproxy service list
```

包装器通过 Compose 标签自动发现运行中的 daemon，并始终使用容器内与 `kproxyd` 同版本的
CLI。当前用户必须有 Docker 权限；同时运行多个 kiro-proxy Compose 项目时，设置
`KPROXY_COMPOSE_PROJECT` 选择目标项目。

加固后的服务模板位于 [`deploy/kproxyd.service`](deploy/kproxyd.service)。将 `kproxyd` 和
`kproxy` 安装到 `/usr/local/bin`，创建 `kproxy` 系统用户与用户组，安装 unit 后即可启用服务。

## 开发

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

项目使用 Rust edition 2021，MSRV 为 1.97.1。

完整的安装、启动、部署、日志、LLDB 和故障排查说明见
[安装、启动与调试指南](docs/startup-and-debugging.zh-CN.md)。

## 许可

MIT

# kiro-proxy

[English](README.md) | [简体中文](README.zh-CN.md)

`kiro-proxy` 是一个无头 Rust 服务，在 Kiro 上游服务之上提供兼容 Claude Messages 和
OpenAI Chat Completions 的 API，并包含多账号调度、自动 Token 刷新、端点切换、模型映射、
API Key 限额、TLS、Webhook、统计和运维 CLI。

> [!IMPORTANT]
> 本项目仅支持通过企业 SSO（AWS IAM Identity Center/IdC）认证的 Kiro 企业账号。
> 其他所有账号和认证类型均不支持，包括个人账号和社交登录账号。

本仓库不包含 GUI、MITM 或本机 Kiro 应用配置修改功能。

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

最简生产部署只需要 Docker Engine 和 Compose v2 插件。Compose 默认构建启用全部 feature
且包含 Chromium 的 full 镜像，使用 host network 启动 `kproxyd`，并将全部状态保存在
`kproxy-data` named volume 中。在仓库根目录执行一键脚本，它会校验环境、构建镜像、启动
服务、等待健康，并在宿主机安装 `kproxy` 命令：

```bash
./deploy/docker-setup.sh
kproxy health
kproxy status
```

默认安装到 `/usr/local/bin/kproxy`，需要时脚本会调用 `sudo`。没有 sudo 权限时可安装到
用户目录：

```bash
./deploy/docker-setup.sh --target "$HOME/.local/bin/kproxy"
```

宿主机上的 `kproxy` 是轻量包装器：它把命令转发给容器内同版本 CLI，避免 Linux 容器二进制
与 macOS 等宿主机不兼容，也无需暴露管理 Unix socket。源码升级后重新运行同一脚本即可；
如只需启动已有镜像，可增加 `--no-build`。

脚本还会在 Linux 上预检 named volume。如果 Docker 中保留了 volume 元数据、但宿主机上的
实际数据目录已经丢失，交互运行时会询问是否重建；自动化环境可显式使用
`--repair-volume`。仅当 volume 确认属于当前 Compose 项目、Docker volume 根目录正常且数据
路径确实不存在时才允许重建；数据盘或软链接异常会直接停止，避免覆盖可能恢复的数据。

全新 daemon 只开放 Unix 管理 socket，不会自动创建业务代理。显式创建首个 service，并
立即保存命令输出的 API Key：

```bash
kproxy status
kproxy service create --name main
kproxy service list
```

发送生成请求前，至少导入一个受支持的 Kiro 企业 SSO 账号：

```bash
kproxy account import --stdin < accounts.json
kproxy account probe --all
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

### Claude Code MCP Tool Search

当 `ANTHROPIC_BASE_URL` 指向第三方代理时，Claude Code 默认会关闭 Tool Search，并在请求中
一次性加载全部 MCP schema。MCP 工具较多时，应显式启用：

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:5580 ENABLE_TOOL_SEARCH=auto claude
```

`kiro-proxy` 现已兼容 Anthropic 的 `defer_loading`、regex/BM25 Tool Search 和
`tool_reference` 历史块。由于 Kiro 没有原生 Tool Search server tool，代理会在本地执行搜索，
并在同一个响应中继续生成。官方 Tool Search 输入包含 `pattern` 或 `query`，并支持可选
`limit`（1–10000，默认 5）。代理会先遵守请求的 limit，再按剩余工具数、tool token、上下文和
payload 字节预算动态装载，因此不存在固定 5 个的工作集上限；未命中的 deferred schema 不会进入
Kiro 上下文或上游 payload。Catalog 构建和搜索运行在 blocking worker，不会阻塞 HTTP runtime。

自动生成的 `[context]` 配置还通过 `max_loaded_tools` 限制已加载工作集（默认值及上游协议
硬上限均为 128）、通过 `max_tool_input_tokens` 限制已加载工具的估算 token，并通过
`max_upstream_payload_bytes` 限制序列化后的 Kiro 请求大小。超限请求会在本地返回明确的 413，
而不是留给上游返回不透明错误。

`features.tool_search_max_rounds` 默认 4、硬上限 8；单次请求达到内部轮次上限时返回 Claude
`pause_turn` 续轮状态，不再把合法的 server call 转换成 HTTP 5xx。
`features.tool_search_max_operations` 默认 32（有效范围 1–256），统一限制历史待续调用及所有内部
轮次的搜索操作总数；超出的调用会收到响应内 `unavailable` Tool Search 结果。将
`features.enable_tool_search=false` 作为回滚开关时，代理会明确拒绝原生 Tool Search 请求，
不会把 deferred 工具重新全量塞给上游。持久化请求日志会保存 Catalog/Working Set 大小、搜索
limit 与预算截断、Client/Upstream Status 和稳定错误码。错误响应保持原有 Claude/OpenAI body，
并通过 `request-id`、`x-kproxy-error-code`、`x-kproxy-error-stage`、
`x-kproxy-upstream-status`、`x-kproxy-account-error` 响应头提供诊断信息。

### Web Search

Claude 原生 `web_search` server tool 会先交给 Kiro 模型决定是否搜索及搜索词，再由代理调用
Kiro 的 `/mcp` JSON-RPC `web_search`，把真实结果作为工具结果续回同一模型轮次。代理不会把
首条用户消息直接当查询，也不会把原始搜索摘要伪装成模型最终回答。流式和非流式 Claude 响应
均使用 `server_tool_use` / `web_search_tool_result`；搜索错误作为 200 响应内的工具错误返回，
不会冷却或封禁账号。并行搜索不会被丢弃；当同一轮同时包含 server tool 和 client tool 时，
server call 会保持 pending，等客户端回传 client tool 结果后再按官方续轮协议补结果。搜索结果
携带代理自有的 AES-256-GCM opaque 内容，后续轮次可恢复 snippet，任何篡改都会在进入模型上下文
前被拒绝；Anthropic 自有 opaque 值仍可带回，但代理不会尝试解密。只有最终文本确实包含结果的
完整 URL 时才会输出结构化 `web_search_result_location` citation。代理安全上限默认 20 次；显式 `max_uses` 超过配置值时会
明确拒绝，不再静默截断。Claude Web Fetch 在兼容的服务端执行器完成前会被明确拒绝。

默认 MCP 地址是 `https://runtime.{region}.kiro.dev/mcp`。可通过
`upstream.web_search_endpoint`（支持 `{region}`）或临时环境变量 `KPROXY_MCP_URL` 覆盖；
`upstream.web_search_timeout_ms` 默认为 60000。Kiro MCP 不具备等价语义的 domain/location
过滤、code-execution caller、strict 或 eager streaming 会被明确拒绝，不会静默降级。代理生成
的加密字段明确属于 kproxy 自有格式，不宣称与 Anthropic 托管搜索的 ciphertext 互通。

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
| `web-search-replay.key` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | AES-256-GCM 回放密钥，以 `0600` 创建且永不覆盖。 |
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
kproxy service delete main
kproxy account list
kproxy account show <id|email>
kproxy account tag <id|email> --add prod
kproxy account disable <id|email>
kproxy account refresh <id|email>
kproxy account refresh --all
kproxy account probe --all
kproxy account regen-machine-id <id|email>
kproxy account rm <id|email>

kproxy config show --effective
kproxy config path
kproxy config validate
kproxy config reload

kproxy pool --watch --explain
kproxy diagnose endpoints
kproxy diagnose account --all -c 4 --timeout 45s
kproxy subscriptions
kproxy models --mapped
kproxy model-map add --name low-credit --source 'claude-opus-*' --target claude-sonnet-4.6 --below-credits-percent 10
kproxy model-map edit low-credit --below-credits-percent 15
kproxy model-map delete low-credit
kproxy model-map test claude-opus-4

kproxy apikey list
kproxy apikey list --detail
kproxy webhook add --name alerts --kind dingtalk --url https://example/hook --event token-expired
kproxy webhook edit alerts --event token-expired --event quota-exhausted
kproxy webhook delete alerts
kproxy stats --since 1h
kproxy stats --detail --since 1h --by endpoint
kproxy logs -f --level warn
kproxy tasks
kproxy tasks run status_check
kproxy help
```

所有命令都支持全局 `--json`，权威参数列表以 `kproxy --help` 和各子命令的 `--help` 为准。
破坏性操作不提供 `--yes` 跳过选项，执行时必须交互输入 `y` 或 `yes` 二次确认。直接执行
`kproxy` 会显示总帮助，`kproxy help` 会列出可用的主题帮助。

`kproxy stats` 用于查看代理流量、成功率、Token、Credits 和延迟等聚合运维指标，不替代逐条
请求日志。默认只输出紧凑汇总；增加 `--detail` 后才输出分组统计和最近请求。

动态模型探测会在 daemon 启动时立即执行、账号变化时再次触发，之后按模型缓存 TTL 刷新。
每分钟的账号状态任务只刷新额度信息，不再重复请求模型列表。

## 企业 SSO 认证

`kproxyd` 的默认构建和 Docker Compose 都启用全部 feature，包含企业 IAM Identity Center
登录所需的 SSO 支持。先用 `kproxy config edit` 设置全局 start URL：

```toml
[sso]
start_url = "https://example.awsapps.com/start"
```

然后手动添加账号时无需重复传 `--start-url`：

```bash
printf '%s\n' "$PASSWORD" | kproxy account add-sso \
  --email user@example.com \
  --password-stdin

kproxy account add-sso --batch accounts.csv -c 1
```

单次登录仍可用 `--start-url` 覆盖全局值。若明确需要更小且不含浏览器 SSO 的二进制，可用
`cargo build --workspace --no-default-features` 或 Docker 的 `runtime-slim` target。
密码只从 stdin 或两列 CSV 文件读取。遇到 MFA 或上游页面变化需要手工操作时，增加
`--headful`。该流程不会增加对非企业账号或非 SSO 认证方式的支持。

## Docker 与 systemd

```bash
docker compose up -d --build

# 不使用 Compose 时，默认镜像同样是 full：
docker build -t kiro-proxy:latest .
# 明确不需要浏览器 SSO 时可选择 slim：
docker build --target runtime-slim -t kiro-proxy:slim .
docker build --target runtime-full -t kiro-proxy:full .
```

Compose 使用 host network，因此手动创建的任意端口代理服务都会立即在 Docker 宿主机
可访问，无需修改 Compose 或重启容器。服务默认绑定 `0.0.0.0`，应通过宿主机防火墙或云
安全组限制代理端口；只需宿主机本地访问时可使用 `--host 127.0.0.1`。Linux Docker
Engine 可直接使用；Docker Desktop 4.34+ 需要在设置中启用 host networking。数据保存
在 `kproxy-data` volume，full 镜像为企业 SSO 认证额外安装 Chromium。浏览器固定为
`chromiumoxide 0.9.1` 的 CDP 定义所使用的官方 `r1566079` 快照，普通镜像重建不会再静默
升级协议。后续采用浏览器安全更新时，应同时更新并测试这两个固定版本。

Docker 会在源码更新后复用持久化的 Cargo registry 和 target 构建缓存。full 镜像只编译
一次 all-features 二进制，并让首次 Chromium 安装在 Rust release 构建完成后再执行，避免
小规格宿主机被同时进行的两个重任务压垮。Cargo 默认使用一个编译任务；只有在构建机内存
充足时才应提高，例如 `CARGO_BUILD_JOBS=4 docker compose build`。

源码更新后，重新构建并创建容器即可，不要删除 named volume：

```bash
docker compose up -d --build
docker compose exec kproxyd kproxy config show --effective
```

已有 `config.toml` 永远不会被覆盖。旧版本创建的 volume 可能仍包含
`server.host = "127.0.0.1"`；如果需要采用新的 `0.0.0.0` 默认值，请使用
`kproxy config edit` 修改。

一键构建、启动并安装 Docker 包装器：

```bash
./deploy/docker-setup.sh
kproxy health
kproxy service list
```

包装器通过 Compose 标签自动发现运行中的 daemon，并始终使用容器内与 `kproxyd` 同版本的
CLI。当前用户必须有 Docker 权限；同时运行多个 kiro-proxy Compose 项目时，设置
`KPROXY_COMPOSE_PROJECT` 选择目标项目。仅需单独重装包装器时，仍可执行
`sudo ./deploy/install-kproxy-wrapper.sh`。

加固后的服务模板位于 [`deploy/kproxyd.service`](deploy/kproxyd.service)。将 `kproxyd` 和
`kproxy` 安装到 `/usr/local/bin`，创建 `kproxy` 系统用户与用户组，安装 unit 后即可启用服务。
浏览器 SSO 还要求宿主机安装 Chrome 或 Chromium；unit 会为 Chromium 保留用户命名空间与
JIT 可执行内存，同时继续启用其余进程加固项。

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

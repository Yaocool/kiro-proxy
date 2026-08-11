# 安装、启动与调试指南

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md)

本文覆盖首次安装、本地开发、release 二进制、Docker Compose、systemd、日志、Trace ID、
LLDB、测试、升级和常见启动故障。除非特别说明，所有命令都在仓库根目录执行。

## 1. 环境要求

仓库在 `rust-toolchain.toml` 中固定 Rust 1.97.1，并声明安装 `rustfmt`、`clippy` 和
`rust-analyzer`。安装 rustup 后，进入仓库就会自动选择正确的工具链。

根据部署方式准备环境：

- Docker 部署：Docker Engine 和 Compose v2 插件。Linux 原生支持 host network；Docker
  Desktop 需要 4.34 或更高版本，并在设置中启用 host networking。
- 本地构建或开发：rustup，以及 C 工具链和链接器。

```bash
rustup show active-toolchain
rustc --version
cargo --version
```

构建普通 workspace：

```bash
cargo build --workspace --locked
```

标准 `kamd` 不包含 Chromium。如需使用基于浏览器的 IAM Identity Center 登录：

```bash
cargo build -p kamd --features sso --locked
```

## 2. 环境变量加载

本地启动前复制示例文件：

```bash
cp .env.example .env
```

`kamd` 和 `kam` 每次启动都会在解析 CLI 参数前读取 `.env`。程序从当前目录向上查找，
因此从 workspace 子目录启动时也能复用仓库根目录的文件。

环境变量优先级如下：

1. 进程环境中已经存在的变量；
2. 从向上查找时遇到的最近一个 `.env` 加载的值；
3. 存在持久化配置项时使用 `config.toml` 中的值；
4. 应用内置默认值。

`.env` 不会覆盖已存在的进程变量。找不到 `.env` 可以正常启动；文件格式错误或无法读取时，
启动会失败并返回错误。

示例使用 `KAM_HOME=.kam-dev` 隔离开发数据。主要进程级变量如下：

| 变量 | 作用 |
| --- | --- |
| `KAM_HOME` | 将配置、数据、日志和自动生成的管理 socket 放到同一目录。 |
| `KAM_HTTP_PORT` | 覆盖端口等于 `server.port` 默认值的已配置代理服务；不会创建服务。 |
| `KAM_DISABLE_HTTP=1` | 阻止所有已配置代理服务监听，但保留其配置并继续运行 Unix 管理 socket。 |
| `KAM_ADMIN_SOCKET` | 覆盖 `kam` CLI 连接的 socket，不会重新配置 `kamd`。 |
| `KAM_CODEWHISPERER_URL` | 在集成测试或受控代理环境中覆盖 CodeWhisperer 上游地址。 |
| `KAM_AMAZONQ_URL` | 在集成测试或受控代理环境中覆盖 Amazon Q 上游地址。 |
| `RUST_LOG` | 设置控制台和应用诊断的 tracing 过滤器。 |
| `RUST_BACKTRACE` | 设置为 `1` 或 `full` 时启用 Rust 调用栈。 |

持久化的服务、账号池、模型、API Key、TLS、通知和日志配置应写入 `config.toml`，而不是
`.env`。

## 3. 使用本地二进制启动

以开发模式启动 daemon：

```bash
cargo run -p kamd
```

使用示例 `.env` 时，首次启动会在 `.kam-dev/` 下创建：

- `config.toml`：daemon 配置；
- `accounts.json`：账号和凭证，Unix 权限为 `0600`；
- `daily.json`：每日额度记录；
- `stats.json`：请求聚合统计；
- `admin.sock`：本地管理 socket；
- `logs/`：按 UTC 日期和级别拆分的日志。

在另一个终端运行 CLI，它会自动读取同一份 `.env`：

```bash
cargo run -p kam -- status
cargo run -p kam -- health
cargo run -p kam -- service list
cargo run -p kam -- config path
cargo run -p kam -- config show --effective
cargo run -p kam -- account list
```

也可以使用编译后的二进制：

```bash
cargo build --release --locked
./target/release/kamd
./target/release/kam status
```

同一个管理 socket 只能由一个 daemon 占用。进程崩溃留下的失效 socket 会自动删除；如果
socket 仍能接受连接，第二个 daemon 不会删除它。

全新 daemon 默认不创建业务 API 代理服务。即使没有账号或代理服务，`kam health` 仍会
成功，因为应用健康与账号、代理服务的可用性相互独立。需要业务 API 时显式创建：

```bash
cargo run -p kam -- service create --name main
```

该命令会创建服务的首个专属 API Key 并返回明文。使用 `kam service apikeys main` 可查看
不含明文的 Key 元数据，增加 `--show-secret` 后只返回该服务绑定的明文 Key。默认监听
`0.0.0.0`；只需本机访问时使用 `--host 127.0.0.1`。

### 仅管理面模式

阻止所有已配置代理服务监听，同时保留账号和配置管理：

```bash
KAM_DISABLE_HTTP=1 cargo run -p kamd
```

这种方式适合存储维护和只使用 CLI 的测试，不会关闭 `admin.sock`。

## 4. 添加或导入账号

从 JSON 导入已有凭证：

```bash
cargo run -p kam -- account import --file accounts.json
cat accounts.json | cargo run -p kam -- account import --stdin
```

CLI 可以生成缺失的 `id`、`machine_id` 和 `created_at`。导入后检查并探测账号：

```bash
cargo run -p kam -- account list
cargo run -p kam -- account probe --all
cargo run -p kam -- models
```

启用 `sso` feature 后，还可以使用 IAM Identity Center 登录：

```bash
printf '%s\n' "$PASSWORD" | cargo run -p kam -- account add-sso \
  --email user@example.com \
  --start-url https://example.awsapps.com/start \
  --password-stdin
```

`kam` 会把管理请求发送给 `kamd`，所以运行中的 daemon 也必须使用 SSO feature 构建，
不能只构建 CLI。遇到 MFA 或需要手工验证时增加 `--headful`。

## 5. 创建并验证代理服务

如果尚未创建服务，先创建代理并保存返回的 API Key：

```bash
kam service create --name main --port 5580
kam service list
kam service apikeys main --show-secret
```

此时服务绑定 `0.0.0.0:5580`，本机业务地址为 `http://127.0.0.1:5580`。

```bash
curl -i http://127.0.0.1:5580/health

curl -i http://127.0.0.1:5580/v1/messages/count_tokens \
  -H 'authorization: Bearer <key>' \
  -H 'content-type: application/json' \
  -H 'user-agent: claude-cli/1.0 (external, debug)' \
  -d '{"model":"claude-sonnet-4","messages":[{"role":"user","content":"hello"}]}'
```

没有可用上游账号时，`GET /health` 仍返回 `status: ok`；账号数量只是诊断信息，不参与
应用健康判断。本地 Token 计数仍需使用服务 API Key。此时生成请求返回 `503` 属于预期行为。

所有新建服务都必须使用创建时生成的 API Key：

```bash
curl -i http://127.0.0.1:5580/v1/models \
  -H 'authorization: Bearer <key>'
```

创建或配置监听非回环地址的服务时，该服务必须引用至少一个已启用的 API Key，否则配置
校验会拒绝，以避免意外对公网暴露未鉴权服务。

每个业务 HTTP 响应都包含 `x-trace-id`，排查错误时应先保存这个值。

不再需要某个服务时可停止并删除它。关联 API Key 会保留以避免误删；确认不再使用后，可另行
执行 `kam apikey rm <id> --yes`。

```bash
kam service delete main --yes
```

## 6. 配置与热重载

打印实际路径并校验当前配置：

```bash
cargo run -p kam -- config path
cargo run -p kam -- config validate
```

daemon 会使用短防抖监听 `config.toml`。有效修改自动应用，TOML 格式错误或配置值非法时继续
使用上一份配置。也可以显式触发重载：

```bash
cargo run -p kam -- config reload
```

`server.host` 和 `server.port` 是 `kam service create` 使用的默认值，本身不会创建监听。
代理服务的新增和地址修改会在运行时自动协调。以下配置修改需要重启 daemon：

- `admin.socket`；
- 在共享的 HTTP 与 HTTPS 监听模式之间切换。

日志过滤、格式、输出路径、账号池行为、模型规则、通知配置和 TLS 证书内容可以在运行时更新。

首次初始化永远不会覆盖已有 `config.toml`，因此旧版本创建的数据目录可能仍保留
`server.host = "127.0.0.1"`。可使用 `kam config show --effective` 检查实际值；需要采用
当前的 `0.0.0.0` 默认值时，使用 `kam config edit` 修改。

`KAM_HTTP_PORT` 是进程级覆盖。如果该变量仍然存在，修改 `server.port` 不会覆盖它；需要
删除环境变量并重启 daemon。

## 7. 日志与 Trace ID

设置 `KAM_HOME=.kam-dev` 后，日志写入 `.kam-dev/logs/`，文件名示例：

```text
kamd-2026-08-10-info.log
kamd-2026-08-10-warn.log
kamd-2026-08-10-error.1.log
```

日志按级别和 UTC 日期拆分。默认每个分片最大 100 MB，保留三天。`log.file_path` 控制基础
路径；留空时使用数据目录下的 `logs/kamd.log` 作为基础路径。

使用响应中的 Trace ID 搜索：

```bash
rg 'trace_f028' .kam-dev/logs/
```

也可以通过管理 API 跟踪记录：

```bash
cargo run -p kam -- logs -f --level warn
cargo run -p kam -- stats --since 1h --by endpoint
```

需要更多细节时，将 `RUST_LOG` 或 `log.level` 调整为 `debug` 或 `trace`。日志不会记录
提示词、生成的回复正文或 API Key 值。

## 8. Docker Compose

构建并启动默认 slim 镜像：

```bash
docker compose config --quiet
docker compose up -d --build
docker compose ps
docker compose exec kamd kam health
docker compose logs -f kamd
```

Compose 使用 `network_mode: host`，容器内创建的代理监听会直接进入 Docker 宿主机网络
命名空间。因此任意服务端口创建后立即可用，不需要修改 Compose 或重建容器。Linux 上的
Docker Engine 可直接使用；Docker Desktop 4.34 及以上版本需要先在 Settings > Resources
> Network 中启用 host networking。

镜像设置了 `KAM_HOME=/var/lib/kam`，Compose 将 `kam-data` named volume 挂载到该目录。
不要把开发用 `.env.example` 复制进容器，也不要挂载 `.kam-dev`。持久化设置应通过
`kam config edit` 修改 `config.toml`；确需进程级覆盖时，在 Compose 中显式增加环境变量。

已有 bridge 网络部署升级后，需要重建一次本项目容器才能应用新网络模式；named volume
中的数据会保留：

```bash
docker compose up -d --force-recreate
```

日常源码升级只需原地重新构建，并保留 named volume：

```bash
docker compose up -d --build
docker compose exec kamd kam version
docker compose exec kamd kam config show --effective
```

### 在 Docker 宿主机直接使用 `kam`

Dockerfile 或 Compose 服务无法安全地直接向宿主机 `/usr/local/bin` 安装文件。在宿主机
执行一次项目提供的安装脚本即可：

```bash
sudo ./deploy/install-kam-wrapper.sh
kam health
kam status
kam service list
```

包装器通过容器的 `io.kiro-proxy.role=daemon` 标签发现运行中的 daemon，保留命令退出码、
透传 stdin，并且只在交互场景分配 TTY。这样既不需要暴露管理 Unix socket，也不存在
宿主机与容器二进制兼容问题。

安装器默认拒绝覆盖已有命令：

```bash
sudo ./deploy/install-kam-wrapper.sh --force
./deploy/install-kam-wrapper.sh --target "$HOME/.local/bin/kam"
```

当前宿主机用户必须具备 Docker 权限。同时运行多个 kiro-proxy 项目时，可以按 Compose
项目名或容器选择目标：

```bash
export KAM_COMPOSE_PROJECT=kiro-proxy
# 或：export KAM_DOCKER_CONTAINER=<容器名称或ID>
kam status
```

全新 volume 需要显式创建代理服务，并保存命令返回的 API Key：

```bash
docker compose exec kamd kam status
docker compose exec -T kamd kam account import --stdin < accounts.json
docker compose exec kamd kam account probe --all
docker compose exec kamd kam service create --name main
docker compose exec kamd kam service create --name secondary --port 6000
docker compose exec kamd kam service list
docker compose exec kamd kam service apikeys main --show-secret
docker compose exec kamd kam config show --effective
docker compose exec kamd sh -c 'ls -lh /var/lib/kam/logs'
curl -i http://127.0.0.1:5580/health
curl -i http://127.0.0.1:6000/health
```

不再需要某个代理监听时可删除 service；关联 API Key 会继续保留，需另行删除：

```bash
docker compose exec kamd kam service delete secondary --yes
```

服务默认绑定 `0.0.0.0`，可通过 Docker 宿主机的网络接口访问；应使用宿主机防火墙或云
安全组限制端口，只需本机访问时可指定 `--host 127.0.0.1`。业务请求仍必须携带该服务绑定
的 API Key。host network 会取消容器与宿主机之间的网络隔离；若无法接受，应改用入口
脚本保留的 bridge 兼容转发方案。

`docker compose down` 会保留 named volume；`docker compose down -v` 会删除配置、账号、
统计和日志，只应在明确需要重置时使用。

默认 Docker target 是 `runtime-slim`。需要浏览器 SSO 时，把 Compose target 改为
`runtime-full` 后重新构建。full 镜像会安装 Chromium，并设置容器专用的 no-sandbox 标志。

## 9. systemd

构建 release 二进制并安装 unit：

```bash
cargo build --release --locked

sudo useradd --system --home-dir /var/lib/kam --shell /usr/sbin/nologin kam
sudo install -m 0755 target/release/kamd target/release/kam /usr/local/bin/
sudo install -m 0644 deploy/kamd.service /etc/systemd/system/kamd.service
sudo systemctl daemon-reload
sudo systemctl enable --now kamd
sudo systemctl status kamd
```

如果 `kam` 用户已存在，跳过 `useradd`。unit 通过 systemd 管理的目录使用 `/etc/kam`、
`/var/lib/kam` 和 `/run/kam`。

```bash
sudo -u kam kam --socket /run/kam/admin.sock status
sudo journalctl -u kamd -f
sudo systemctl reload kamd
```

reload 会发送 `SIGHUP`。需要重启的配置仍要执行 `sudo systemctl restart kamd`。

unit 默认的加固选项适合 slim 代理。浏览器 SSO 建议使用单独的 unit，并谨慎评估 Chromium
需要的放宽项，尤其是 `RestrictNamespaces` 和 `MemoryDenyWriteExecute`。

## 10. VS Code 与 LLDB

安装 rust-analyzer 和 CodeLLDB，然后增加类似的启动配置：

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "lldb",
      "request": "launch",
      "name": "Debug kamd",
      "cargo": {
        "args": ["build", "-p", "kamd"],
        "filter": { "name": "kamd", "kind": "bin" }
      },
      "cwd": "${workspaceFolder}",
      "env": {
        "KAM_HOME": "${workspaceFolder}/.kam-dev",
        "RUST_LOG": "kamd=debug,kam_kiro=debug,kam_pool=debug",
        "RUST_BACKTRACE": "1"
      }
    }
  ]
}
```

也可以直接使用命令行 LLDB：

```bash
cargo build -p kamd
rust-lldb target/debug/kamd
```

在 LLDB 中用 `settings set target.env-vars KAM_HOME=... RUST_LOG=debug` 设置进程变量，
用 `breakpoint set --name <function>` 增加断点，然后运行进程。排查异步请求时，使用
`x-trace-id` 关联断点和日志，不要依赖线程 ID。

## 11. 测试与静态检查

提交修改前运行完整校验：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
docker compose config --quiet
```

调试时可以缩小范围：

```bash
cargo test -p kam-kiro
cargo test -p kamd http::tests::every_response_has_a_unique_trace_id
cargo test -p kam-pool refresh::tests::successful_refresh_preserves_cooling_and_exhausted_health
```

Wiremock 和端到端测试需要绑定临时回环端口，受限 CI 或沙箱环境必须允许本地端口绑定。

## 12. 常见问题

### `Address already in use`

创建服务时选择未占用端口，或对配置为默认端口的服务使用进程级覆盖：

```bash
kam service create --name main --port 5581
KAM_HTTP_PORT=5581 cargo run -p kamd
```

### 无法连接 `admin.sock`

确认 `kamd` 与 `kam` 读取了同一个 `KAM_HOME`。daemon 可连接时运行 `kam config path`，
或者显式传入 socket：

```bash
kam --socket /path/to/admin.sock status
```

注意 `KAM_ADMIN_SOCKET` 只修改 CLI 目标。要移动 daemon socket，需要修改
`config.toml` 中的 `admin.socket` 并重启 `kamd`。

### 配置修改未生效

运行 `kam config validate`，检查 warn/error 日志，并确认修改字段是否需要重启。同时检查进程
环境中是否存在 `KAM_HTTP_PORT` 覆盖。

### Claude 路由返回访问拒绝

Claude 路由默认校验客户端 User-Agent。请使用兼容 Claude Code 的 User-Agent，或仅在可信
部署中显式修改 `server.enforce_user_agent_check`。

### 生成接口返回 `503`

运行 `kam account list` 和 `kam account probe --all`。可能没有 Available 账号、请求模型不
兼容，或者所有账号都在冷却或额度耗尽状态。

### 流式请求意外中断

使用 `x-trace-id` 搜索 warn/error 日志，检查上游鉴权刷新、端点尝试、账号切换、模型降级、
客户端断连和下游写超时。

### Docker 仍然使用旧层

```bash
docker compose build --pull --no-cache
docker compose up -d
```

返回 [中文 README](../README.zh-CN.md)。

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

默认构建启用全部 feature，其中包括 Chromium SSO：

```bash
cargo build --workspace --locked
```

只有明确不需要浏览器登录并希望缩小二进制时，才关闭默认 feature：

```bash
cargo build --workspace --no-default-features --locked
```

## 2. 环境变量加载

本地启动前复制示例文件：

```bash
cp .env.example .env
```

`kproxyd` 和 `kproxy` 每次启动都会在解析 CLI 参数前读取 `.env`。程序从当前目录向上查找，
因此从 workspace 子目录启动时也能复用仓库根目录的文件。

环境变量优先级如下：

1. 进程环境中已经存在的变量；
2. 从向上查找时遇到的最近一个 `.env` 加载的值；
3. 存在持久化配置项时使用 `config.toml` 中的值；
4. 应用内置默认值。

`.env` 不会覆盖已存在的进程变量。找不到 `.env` 可以正常启动；文件格式错误或无法读取时，
启动会失败并返回错误。

示例使用 `KPROXY_HOME=.kproxy-dev` 隔离开发数据。主要进程级变量如下：

| 变量 | 作用 |
| --- | --- |
| `KPROXY_HOME` | 将配置、数据、日志和自动生成的管理 socket 放到同一目录。 |
| `KPROXY_HTTP_PORT` | 覆盖端口等于 `server.port` 默认值的已配置代理服务；不会创建服务。 |
| `KPROXY_DISABLE_HTTP=1` | 阻止所有已配置代理服务监听，但保留其配置并继续运行 Unix 管理 socket。 |
| `KPROXY_ADMIN_SOCKET` | 覆盖 `kproxy` CLI 连接的 socket，不会重新配置 `kproxyd`。 |
| `KPROXY_CODEWHISPERER_URL` | 在集成测试或受控代理环境中覆盖 CodeWhisperer 上游地址。 |
| `KPROXY_AMAZONQ_URL` | 在集成测试或受控代理环境中覆盖 Amazon Q 上游地址。 |
| `RUST_LOG` | 设置控制台和应用诊断的 tracing 过滤器。 |
| `RUST_BACKTRACE` | 设置为 `1` 或 `full` 时启用 Rust 调用栈。 |

持久化的服务、账号池、模型、API Key、TLS、通知和日志配置应写入 `config.toml`，而不是
`.env`。

## 3. 使用本地二进制启动

以开发模式启动 daemon：

```bash
cargo run -p kproxyd
```

使用示例 `.env` 时，首次启动会在 `.kproxy-dev/` 下创建：

- `config.toml`：daemon 配置；
- `accounts.json`：账号和凭证，Unix 权限为 `0600`；
- `daily.json`：每日额度记录；
- `stats.json`：请求聚合统计；
- `admin.sock`：本地管理 socket；
- `logs/`：按 UTC 日期和级别拆分的日志。

在另一个终端运行 CLI，它会自动读取同一份 `.env`：

```bash
cargo run -p kproxy -- status
cargo run -p kproxy -- health
cargo run -p kproxy -- service list
cargo run -p kproxy -- config path
cargo run -p kproxy -- config show --effective
cargo run -p kproxy -- account list
```

也可以使用编译后的二进制：

```bash
cargo build --release --locked
./target/release/kproxyd
./target/release/kproxy status
```

同一个管理 socket 只能由一个 daemon 占用。进程崩溃留下的失效 socket 会自动删除；如果
socket 仍能接受连接，第二个 daemon 不会删除它。

全新 daemon 默认不创建业务 API 代理服务。即使没有账号或代理服务，`kproxy health` 仍会
成功，因为应用健康与账号、代理服务的可用性相互独立。业务可用性监控请使用
`kproxy ready`（或代理监听器的 `/ready`）：它会报告账号不可用、监听失败、计量恢复模式和
后台任务心跳过期，但不会停止 daemon。需要业务 API 时显式创建：

```bash
cargo run -p kproxy -- service create --name main
```

该命令会创建服务的首个专属 API Key 并返回明文。使用 `kproxy service apikeys main` 可查看
不含明文的 Key 元数据，增加 `--show-secret` 后只返回该服务绑定的明文 Key。默认监听
`0.0.0.0`；只需本机访问时使用 `--host 127.0.0.1`。

### 仅管理面模式

阻止所有已配置代理服务监听，同时保留账号和配置管理：

```bash
KPROXY_DISABLE_HTTP=1 cargo run -p kproxyd
```

这种方式适合存储维护和只使用 CLI 的测试，不会关闭 `admin.sock`。

## 4. 添加或导入账号

从 JSON 导入已有凭证：

```bash
cargo run -p kproxy -- account import --file accounts.json
cat accounts.json | cargo run -p kproxy -- account import --stdin
```

CLI 可以生成缺失的 `id`、`machine_id` 和 `created_at`。导入后检查并探测账号：

```bash
cargo run -p kproxy -- account list
cargo run -p kproxy -- account probe --all
cargo run -p kproxy -- models
```

默认构建已经包含 IAM Identity Center 登录。先在 `config.toml` 中设置全局 start URL
（可用 `kproxy config edit`）：

```toml
[sso]
start_url = "https://example.awsapps.com/start"
```

之后手动添加账号时可省略 `--start-url`：

```bash
printf '%s\n' "$PASSWORD" | cargo run -p kproxy -- account add-sso \
  --email user@example.com \
  --password-stdin
```

单次登录仍可用 `--start-url` 覆盖全局值。`kproxy` 会把管理请求发送给 `kproxyd`，所以若显式
使用 `--no-default-features` 构建 daemon，浏览器登录不可用。遇到 MFA 或需要手工验证时
增加 `--headful`。只支持 Kiro 企业账号的组织 SSO，个人账号、社交登录和其他认证类型均
不支持。

## 5. 创建并验证代理服务

如果尚未创建服务，先创建代理并保存返回的 API Key：

```bash
kproxy service create --name main --port 5580
kproxy service list
kproxy service apikeys main --show-secret
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
应用健康判断。响应包含 `total_accounts` 和各健康状态数量；`used_credits`、`total_credits`
聚合共享账号池内全部已配置账号的最近一次上游用量快照，尚无用量快照的账号对两项 credits
合计均按零处理。本地 Token 计数仍需使用服务 API Key。此时生成请求返回 `503` 属于预期行为。

所有新建服务都必须使用创建时生成的 API Key：

```bash
curl -i http://127.0.0.1:5580/v1/models \
  -H 'authorization: Bearer <key>'
```

创建或配置监听非回环地址的服务时，该服务必须引用至少一个已启用的 API Key，否则配置
校验会拒绝，以避免意外对公网暴露未鉴权服务。

每个业务 HTTP 响应都包含 `x-trace-id`，排查错误时应先保存这个值。

不再需要某个服务时可停止并删除它。删除服务时会同时删除仅由该服务使用的
API Key；仍被其他服务引用的共享 API Key 会保留。

```bash
kproxy service delete main
```

删除、重置用量等破坏性命令不支持 `--yes` 跳过确认，必须在交互终端输入 `y` 或 `yes`。

## 6. 配置与热重载

打印实际路径并校验当前配置：

```bash
cargo run -p kproxy -- config path
cargo run -p kproxy -- config validate
```

daemon 会使用短防抖监听 `config.toml`。有效修改自动应用，TOML 格式错误或配置值非法时继续
使用上一份配置。也可以显式触发重载：

```bash
cargo run -p kproxy -- config reload
```

`server.host` 和 `server.port` 是 `kproxy service create` 使用的默认值，本身不会创建监听。
代理服务的新增和地址修改会在运行时自动协调。以下配置修改需要重启 daemon：

- `admin.socket`；
- 在共享的 HTTP 与 HTTPS 监听模式之间切换。

日志过滤、格式、输出路径、账号池行为、模型规则、通知配置和 TLS 证书内容可以在运行时更新。

告警策略、钉钉/飞书等通知目标和模型映射均可通过 CLI 管理，命令会校验配置、原子写入并热重载：

```bash
kproxy alert events
kproxy alert config --low-credit-threshold-percent 10 --max-notifications 5 --suppress-window 30m
kproxy alert add --name alerts --kind dingtalk --url https://example/hook --event token-expired,quota-exhausted
kproxy alert edit alerts --event token-expired --event quota-exhausted
kproxy alert delete alerts

kproxy model-map add --name low-credit --source 'claude-opus-*' \
  --target claude-sonnet-4.6 --below-credits-percent 10
kproxy model-map edit low-credit --below-credits-percent 15
kproxy model-map test claude-opus-4.6 --remaining-credits-percent 8
kproxy model-map delete low-credit
```

`kproxy alert events` 会说明每个事件的实际触发条件。一个告警目标可重复传入 `--event`，
也可使用逗号分隔订阅多个事件；`alert edit --event ...` 会整体替换该目标原有的事件列表。

带 `--below-credits-percent` 的映射按每个选中账号的剩余 Credits 判断。未配置 schedule 时
默认全天生效；剩余额度低于阈值时命中，次月额度恢复到阈值以上后自动停止命中。

模型自动探测与显式模型映射彼此独立。自动探测在 daemon 启动时执行一次、账号变化后再次
触发，之后遵循 `models.cache_ttl_ms`；账号 `status_check` 任务只刷新额度，不会再发起一轮
模型列表请求。

首次初始化永远不会覆盖已有 `config.toml`，因此旧版本创建的数据目录可能仍保留
`server.host = "127.0.0.1"`。可使用 `kproxy config show --effective` 检查实际值；需要采用
当前的 `0.0.0.0` 默认值时，使用 `kproxy config edit` 修改。

`KPROXY_HTTP_PORT` 是进程级覆盖。如果该变量仍然存在，修改 `server.port` 不会覆盖它；需要
删除环境变量并重启 daemon。

## 7. 日志与 Trace ID

设置 `KPROXY_HOME=.kproxy-dev` 后，日志写入 `.kproxy-dev/logs/`，文件名示例：

```text
kproxyd-2026-08-10-info.log
kproxyd-2026-08-10-warn.log
kproxyd-2026-08-10-error.1.log
```

日志按级别和 UTC 日期拆分。默认每个分片最大 100 MB，保留三天。`log.file_path` 控制基础
路径；留空时使用数据目录下的 `logs/kproxyd.log` 作为基础路径。

使用响应中的 Trace ID 搜索：

```bash
rg 'trace_f028' .kproxy-dev/logs/
```

通过管理 API 查看当前日志目标并发现实际分片文件：

```bash
kproxy logs path
kproxy logs files
kproxy logs files --level error
```

`logs files` 输出 daemon 文件系统中的完整路径。容器部署时这些是持久化数据卷内的容器路径，
在宿主机直接使用 wrapper 执行相同命令即可，无需进入容器。

查看或持续跟踪结构化请求记录：

```bash
cargo run -p kproxy -- logs show --tail 100
cargo run -p kproxy -- logs follow --level warn
cargo run -p kproxy -- stats --since 1h
cargo run -p kproxy -- stats --detail --since 1h --by endpoint
```

旧的 `kproxy logs --tail ...` 与 `kproxy logs -f` 用法继续兼容。

`kproxy stats` 用于查看请求量、成功率、Tokens、Credits 和延迟等聚合运维指标。默认只显示
紧凑汇总，`--detail` 才显示最近请求和按 model/account/apikey/endpoint 的分组统计；逐条
故障信息仍应使用 `kproxy logs` 和 Trace ID。

需要更多细节时，将 `RUST_LOG` 或 `log.level` 调整为 `debug` 或 `trace`。日志不会记录
提示词、生成的回复正文或 API Key 值。

## 8. Docker Compose

构建并启动默认 full 镜像（启用全部 feature 并包含 Chromium SSO）：

```bash
./deploy/docker-setup.sh
kproxy health
```

该脚本会完成 Compose 配置校验、镜像构建、服务启动、健康等待和宿主机 `kproxy` 命令安装。
默认目标是 `/usr/local/bin/kproxy`；无 sudo 权限时可使用
`--target "$HOME/.local/bin/kproxy"`。以下是对应的手工命令，适合调试：

```bash
docker compose config --quiet
docker compose up -d --build
docker compose ps
docker compose exec kproxyd kproxy health
docker compose logs -f kproxyd
```

Linux Docker Engine 上如果出现 `failed to populate volume`，且错误指出
`.../volumes/kiro-proxy_kproxy-data/_data` 不存在，说明 Docker 保留了 named volume 元数据，
但实际目录已经丢失。新版一键脚本会在构建前检测这一状态：交互终端会请求确认后重建，CI
或其他非交互环境可运行：

```bash
./deploy/docker-setup.sh --no-build --repair-volume
```

修复只针对带有当前 Compose 项目标记且数据路径已经不存在的 volume。如果 Docker volume
根目录、数据盘挂载或软链接本身异常，脚本会停止并要求先恢复 Docker 存储，不会自动删除。

Compose 使用 `network_mode: host`，容器内创建的代理监听会直接进入 Docker 宿主机网络
命名空间。因此任意服务端口创建后立即可用，不需要修改 Compose 或重建容器。Linux 上的
Docker Engine 可直接使用；Docker Desktop 4.34 及以上版本需要先在 Settings > Resources
> Network 中启用 host networking。

镜像设置了 `KPROXY_HOME=/var/lib/kproxy`，Compose 将 `kproxy-data` named volume 挂载到该目录。
不要把开发用 `.env.example` 复制进容器，也不要挂载 `.kproxy-dev`。持久化设置应通过
`kproxy config edit` 修改 `config.toml`；确需进程级覆盖时，在 Compose 中显式增加环境变量。

已有 bridge 网络部署升级后，需要重建一次本项目容器才能应用新网络模式；named volume
中的数据会保留：

```bash
docker compose up -d --force-recreate
```

日常源码升级只需原地重新构建，并保留 named volume：

```bash
docker compose up -d --build
docker compose exec kproxyd kproxy version
docker compose exec kproxyd kproxy config show --effective
```

### 在 Docker 宿主机直接使用 `kproxy`

Dockerfile 或 Compose 服务无法安全地直接向宿主机 `/usr/local/bin` 安装文件。一键脚本会在
宿主机安装项目提供的包装器，并将 Compose 服务拉起至健康状态：

```bash
./deploy/docker-setup.sh
kproxy health
kproxy status
kproxy service list
```

包装器通过容器的 `io.kiro-proxy.role=daemon` 标签发现运行中的 daemon，保留命令退出码、
透传 stdin，并且只在交互场景分配 TTY。交互命令会把宿主机的 `TERM` 传入容器；如果镜像
不支持该终端类型，则自动回退为 `xterm-256color`。镜像内置完整的 `vim` 和扩展 terminfo，
`kproxy config edit` 默认直接使用 `vim`，可正常处理方向键。这样既不需要暴露管理 Unix
socket，也不存在宿主机与容器二进制兼容问题。

升级已有部署时，wrapper 和容器镜像都需要更新：

```bash
sudo ./deploy/install-kproxy-wrapper.sh
docker compose up -d --build
kproxy config edit
```

也可在宿主机显式选择容器内已安装的编辑器，例如 `EDITOR=vim kproxy config edit`。

批量 SSO 导入时，wrapper 会识别 `--batch` 指向的可读宿主机文件，并通过 stdin 直接流式
传入容器，不需要 `docker cp`，也不会在容器中留下 CSV：

```bash
kproxy account add-sso --batch ./accounts.csv --start-url 'https://example.awsapps.com/start'
```

CLI 也原生支持 `-` 表示 stdin，因此在宿主机和容器内都可显式使用
`kproxy account add-sso --batch - < accounts.csv`。如果宿主机不存在指定文件，wrapper 会保留
参数，由容器按自己的文件系统解析该路径。

需要只安装包装器时可直接运行底层安装器。它会自动更新由本项目管理的包装器，并默认拒绝
覆盖其他已有命令：

```bash
sudo ./deploy/install-kproxy-wrapper.sh
./deploy/install-kproxy-wrapper.sh --target "$HOME/.local/bin/kproxy"
# 只有明确要替换其他同名命令时才使用：
sudo ./deploy/install-kproxy-wrapper.sh --force
```

当前宿主机用户必须具备 Docker 权限。同时运行多个 kiro-proxy 项目时，可以按 Compose
项目名或容器选择目标：

```bash
export KPROXY_COMPOSE_PROJECT=kiro-proxy
# 或：export KPROXY_DOCKER_CONTAINER=<容器名称或ID>
kproxy status
```

宿主机包装器还可直接管理 Docker 服务生命周期：

```bash
kproxy restart
kproxy stop
kproxy uninstall
kproxy uninstall --backup-dir /srv/kproxy-backups
```

`restart` 会等待容器健康检查通过，`stop` 后仍可用 `restart` 启动。`uninstall`
会先停服并把 `/var/lib/kproxy` 备份到宿主机，默认位置为 `~/.kproxy/backups`。备份失败
时原数据不会删除，原容器会重新启动。交互执行会询问是否保留备份；`--yes` 默认保留，
使用 `--delete-backup` 才会在成功卸载后删除。容器、数据卷、未共享镜像和已安装的
包装器会被删除，源码目录始终保留。

全新 volume 需要显式创建代理服务，并保存命令返回的 API Key：

```bash
docker compose exec kproxyd kproxy status
docker compose exec -T kproxyd kproxy account import --stdin < accounts.json
docker compose exec kproxyd kproxy account probe --all
docker compose exec kproxyd kproxy service create --name main
docker compose exec kproxyd kproxy service create --name secondary --port 6000
docker compose exec kproxyd kproxy service list
docker compose exec kproxyd kproxy service apikeys main --show-secret
docker compose exec kproxyd kproxy config show --effective
docker compose exec kproxyd sh -c 'ls -lh /var/lib/kproxy/logs'
curl -i http://127.0.0.1:5580/health
curl -i http://127.0.0.1:6000/health
```

不再需要某个代理监听时可删除 service；其专用 API Key 会同时删除，被其他服务共享的
API Key 则会保留：

```bash
docker compose exec kproxyd kproxy service delete secondary
```

服务默认绑定 `0.0.0.0`，可通过 Docker 宿主机的网络接口访问；应使用宿主机防火墙或云
安全组限制端口，只需本机访问时可指定 `--host 127.0.0.1`。业务请求仍必须携带该服务绑定
的 API Key。host network 会取消容器与宿主机之间的网络隔离；若无法接受，应改用入口
脚本保留的 bridge 兼容转发方案。

`docker compose down` 会保留 named volume；`docker compose down -v` 会删除配置、账号、
统计和日志，只应在明确需要重置时使用。

默认 Docker target 是 `runtime-full`，会安装固定的 Chromium 官方快照 `r1566079`、启用
全部 feature，并设置容器专用的 no-sandbox 标志。该快照正是 `chromiumoxide 0.9.1` 的
CDP 定义所使用的 revision；升级时应同时更新并测试二者，不能让操作系统包单独改变 CDP
协议。只有明确不需要浏览器 SSO 时，才把 Compose target 改为 `runtime-slim` 后重新构建。

BuildKit 会跨构建保留 Cargo registry 和 target 缓存。full target 只构建 all-features
二进制，并在 release 构建完成后才执行未命中的 Chromium 安装层，从而限制小规格宿主机的
峰值内存和磁盘压力。Compose 默认把 `CARGO_BUILD_JOBS` 设为 `1`；只有构建机内存充足时
才应提高。

## 9. systemd

构建 release 二进制并安装 unit：

```bash
cargo build --release --locked

sudo useradd --system --home-dir /var/lib/kproxy --shell /usr/sbin/nologin kproxy
sudo install -m 0755 target/release/kproxyd target/release/kproxy /usr/local/bin/
sudo install -m 0644 deploy/kproxyd.service /etc/systemd/system/kproxyd.service
sudo systemctl daemon-reload
sudo systemctl enable --now kproxyd
sudo systemctl status kproxyd
```

如果 `kproxy` 用户已存在，跳过 `useradd`。unit 通过 systemd 管理的目录使用 `/etc/kproxy`、
`/var/lib/kproxy` 和 `/run/kproxy`。

```bash
sudo -u kproxy kproxy --socket /run/kproxy/admin.sock status
sudo journalctl -u kproxyd -f
sudo systemctl reload kproxyd
```

reload 会发送 `SIGHUP`。需要重启的配置仍要执行 `sudo systemctl restart kproxyd`。

使用 `kproxy account add-sso` 前还要在宿主机安装 Chrome 或 Chromium。提供的 unit 支持
默认 full 构建：它会为 Chromium 保留用户命名空间和 JIT 可执行内存，同时继续启用
`NoNewPrivileges`、文件系统保护、空 capability 集合等加固项。如果宿主机内核禁用了非特权
用户命名空间，优先在系统层启用；最后的兼容手段是通过 `systemctl edit kproxyd` 设置
`KPROXY_CHROMIUM_NO_SANDBOX=1`。该选项会关闭 Chromium 自身的 sandbox，只应在评估宿主机
隔离边界后使用。

## 10. VS Code 与 LLDB

安装 rust-analyzer 和 CodeLLDB，然后增加类似的启动配置：

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "lldb",
      "request": "launch",
      "name": "Debug kproxyd",
      "cargo": {
        "args": ["build", "-p", "kproxyd"],
        "filter": { "name": "kproxyd", "kind": "bin" }
      },
      "cwd": "${workspaceFolder}",
      "env": {
        "KPROXY_HOME": "${workspaceFolder}/.kproxy-dev",
        "RUST_LOG": "kproxyd=debug,kproxy_kiro=debug,kproxy_pool=debug",
        "RUST_BACKTRACE": "1"
      }
    }
  ]
}
```

也可以直接使用命令行 LLDB：

```bash
cargo build -p kproxyd
rust-lldb target/debug/kproxyd
```

在 LLDB 中用 `settings set target.env-vars KPROXY_HOME=... RUST_LOG=debug` 设置进程变量，
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
cargo test -p kproxy-kiro
cargo test -p kproxyd http::tests::every_response_has_a_unique_trace_id
cargo test -p kproxy-pool refresh::tests::successful_refresh_preserves_cooling_and_exhausted_health
```

Wiremock 和端到端测试需要绑定临时回环端口，受限 CI 或沙箱环境必须允许本地端口绑定。

## 12. 常见问题

### `Address already in use`

创建服务时选择未占用端口，或对配置为默认端口的服务使用进程级覆盖：

```bash
kproxy service create --name main --port 5581
KPROXY_HTTP_PORT=5581 cargo run -p kproxyd
```

### 无法连接 `admin.sock`

确认 `kproxyd` 与 `kproxy` 读取了同一个 `KPROXY_HOME`。daemon 可连接时运行 `kproxy config path`，
或者显式传入 socket：

```bash
kproxy --socket /path/to/admin.sock status
```

注意 `KPROXY_ADMIN_SOCKET` 只修改 CLI 目标。要移动 daemon socket，需要修改
`config.toml` 中的 `admin.socket` 并重启 `kproxyd`。

### 配置修改未生效

运行 `kproxy config validate`，检查 warn/error 日志，并确认修改字段是否需要重启。同时检查进程
环境中是否存在 `KPROXY_HTTP_PORT` 覆盖。

### Claude 路由返回访问拒绝

Claude 路由默认校验客户端 User-Agent。请使用兼容 Claude Code 的 User-Agent，或仅在可信
部署中显式修改 `server.enforce_user_agent_check`。

### 生成接口返回 `503`

运行 `kproxy account list` 和 `kproxy account probe --all`。可能没有 Available 账号、请求模型不
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

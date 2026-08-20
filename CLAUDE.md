# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 项目定位

`kiro-proxy` 是无 GUI 的 Rust 常驻服务，把 Kiro 上游（CodeWhisperer / Amazon Q / Kiro MCP）包装成 Claude Messages 与 OpenAI Chat Completions 兼容 API。仅支持 Kiro **企业版 SSO（AWS IAM Identity Center / IdC）** 账号，不支持个人账号或社交登录；仓库刻意不含 GUI、MITM 与本地 Kiro 应用改写。

## 常用命令

```bash
# 提交前的完整校验集（缺一不可，clippy 以 -D warnings 运行）
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
docker compose config --quiet

# 缩小范围调试
cargo test -p kproxy-kiro
cargo test -p kproxyd http::tests::every_response_has_a_unique_trace_id
cargo test -p kproxy-pool refresh::tests::successful_refresh_preserves_cooling_and_exhausted_health
```

工具链由 `rust-toolchain.toml` 钉在 1.97.1（edition 2021），无需手动 `rustup override`。

### 本地启动

`.env.example` 里的 `KPROXY_HOME=.kproxy-dev` 把配置/数据/日志/admin socket 隔离到仓库内，开发时应保持该约定，避免污染 XDG 目录。

```bash
cp .env.example .env
cargo run -p kproxyd                      # 首次启动生成 config.toml/accounts.json/daily.json/stats.json
cargo run -p kproxy -- health              # 另一个终端；CLI 通过 Unix socket 连 daemon
cargo run -p kproxy -- service create --name main
```

`kproxyd` 与 `kproxy` 都在解析 CLI 参数前从当前目录逐级向上查找 `.env`，因此在 workspace 子目录里执行命令也能复用仓库根的那份。已存在的进程环境变量优先级最高，永不被 `.env` 覆盖。常用进程级开关：`KPROXY_HTTP_PORT`（仅覆盖端口等于 `server.port` 默认值的服务，不创建服务）、`KPROXY_DISABLE_HTTP=1`（只保留管理面）、`KPROXY_ADMIN_SOCKET`（只影响 CLI 侧）、`KPROXY_CODEWHISPERER_URL` / `KPROXY_AMAZONQ_URL` / `KPROXY_MCP_URL`（集成测试指向 wiremock）。

wiremock 与端到端测试会绑定临时 loopback 端口，沙箱 CI 必须允许本地监听。

## 架构

### 双控制平面

进程只有一个入口 `crates/kproxyd/src/main.rs`，但对外暴露两个互不相通的平面：

- **业务面**：`crates/kproxyd/src/http/`，由 `ProxyServiceManager` 按 `config.toml` 里的 proxy service 列表动态增删 axum listener。每个 service 有独立 host/port 与 API key 白名单，`router_for_service` 为其单独构造 Router，`ServiceHttpState` 携带 `{app, service, allowed_api_key_ids}`。新增/删除 service 无需重启 daemon。
- **管理面**：`crates/kproxyd/src/admin/`，Unix socket 上的行分隔 JSON-RPC。`kproxy-ipc::protocol` 定义 `Request{id, method, params}` / `Response`（untagged Ok|Err）与全部结果 DTO；`admin/handlers.rs` 用一张 `match request.method.as_str()` 大表分发到 `method::*` 常量。**新增 CLI 命令必须三处同步**：`kproxy-ipc` 的 method 常量 + 结果结构、`admin/handlers.rs` 的分发分支、`crates/kproxy/src/commands/` 的 clap 子命令。

### crate 依赖方向

```
kproxy-core  ← 领域模型、默认值、config 校验（无 I/O 依赖）
kproxy-store ← 原子落盘、.env 加载、bootstrap、config 热重载 watcher
kproxy-ipc   ← daemon/CLI 共享协议
kproxy-translate ← Claude/OpenAI ↔ Kiro 转换、校验、token 估算
kproxy-kiro  ← Kiro HTTP client、Event Stream 解码、endpoint 状态、模型发现（依赖 translate）
kproxy-pool  ← 账号健康、额度预留、并发、加权调度（依赖 core/translate/kiro/store）
kproxy-notify← webhook 投递与抑制
kproxyd / kproxy ← 二进制，聚合以上全部
```

### 单请求链路

入口 `http/handlers.rs::claude_messages`（该文件 6200+ 行，是全仓最热的一处）先做准入与读体，再交给同文件的 `handle_claude` 完成其余步骤：

1. 两级准入：`state.connections.try_acquire()` 与 `state.admission.try_acquire()`，任一失败直接返回 overloaded，再经 `read_bounded_body` 走 `BodyBudget`。
2. 反序列化 `ClaudeRequest` → `validate_claude` → `context::apply_context_management_edits` / `apply_compaction_boundary`（Claude 上下文压缩在本地模拟，未知 edit 家族是前向兼容 no-op）。
3. `map_model` 按别名/替换/额度阈值规则解析目标模型 → `check_context_limit` 校验换算后的目标窗口 → `claude_to_kiro(&request, &options)` 生成 Kiro payload。
4. Tool Search 与 catalog 索引跑在 `spawn_blocking`，不占 HTTP runtime 线程；`features.tool_search_max_rounds`（默认 4，硬夹 8）限制续跑轮数，`features.tool_search_max_operations`（默认 32，clamp 到 1–256）限制单次检索操作数。
5. `AccountPool::acquire` 加权选号并预留额度，`KiroClient::generate` 打上游，`http/stream.rs` 把 Kiro Event Stream 转成 Claude/OpenAI SSE；多轮工具调用经 `auto_continue_payload` 续接，上限 `features.auto_continue_rounds`（硬夹 30 轮）。
6. 失败经 `record_failed_request` 计入 `stats` 与 `meter`；账号级错误反馈 `record_error` / `record_quota_error` / `mark_banned`。

`AppState`（`kproxyd/src/state.rs`）是唯一共享状态，内部大量 `RwLock<...>` 字段（pool、kiro、notifier、refresher、tls_config、runtime_config）配合 `account_mutation` / `config_mutation` 两把 `Mutex` 串行化写操作。改动这些字段时优先复用 `apply_config_transaction`，不要新开锁顺序。

### 服务端工具的本地执行

Kiro 没有原生 Tool Search / Web Search server tool，代理在本地补齐并**续接同一次模型回合**：

- Tool Search（`translate/tool_search.rs`）：接受 Anthropic `defer_loading`、regex/BM25 检索与 `tool_reference` 历史块，尊重客户端 `limit`（1–10000，默认 5），再按剩余 tool 数 / tool token / context / payload 字节预算打包。`features.tool_search_max_rounds`（默认 4，硬夹 8）控制续跑轮数，`features.tool_search_max_operations`（默认 32，clamp 1–256）控制单轮检索操作数；耗尽时用 Claude `pause_turn` 续跑而不是转成 5xx。
- Web Search（`translate/web_search.rs`）：由 Kiro 决定是否搜索并给出 query，代理调 Kiro `/mcp` JSON-RPC 后把真实结果作为 tool result 回灌。`features.web_search_max_rounds`（默认 20）是代理侧上限，客户端请求值不得超过它。结果附带**代理自有**的 AES-256-GCM replay 内容（密钥 `web-search-replay.key`，0600，永不覆写），被篡改的记录在进入模型上下文前拒绝；Anthropic 自有的 opaque 值照收但不本地解密。

### 错误码约定

HTTP `413` / `request_too_large` **只**用于真实入站 body 超过 50 MiB。tool 预算、context 预算、翻译后 payload 超限一律用 `400`，否则 Claude Code 会误报成 32 MB 附件失败。错误响应保持标准 Claude/OpenAI body，诊断信息走 header：`request-id`、`x-kproxy-error-code`、`x-kproxy-error-stage`、`x-kproxy-upstream-status`、`x-kproxy-account-error`。

## 编码约定

- **依赖全部精确锁定**（`=1.0.86` 形式）且集中在根 `[workspace.dependencies]`，crate 内只写 `xxx.workspace = true`。升级依赖要连带评估 `Cargo.lock` 与 Docker 缓存层。
- `chromiumoxide 0.9.1` 与 Chromium 快照 `r1566079` 是**成对**钉住的（CDP 协议兼容），改一个必须同时改另一个并实测。
- `kproxyd` 的 `sso` feature 默认开启（引入 chromiumoxide）；`--no-default-features` 对应 Docker `runtime-slim` 目标，浏览器 SSO 相关代码必须置于 `#[cfg(feature = "sso")]` 之后。
- 注释与文档字符串中英混排是既有风格（`//!` 模块头多为英文，行内解释常为中文），跟随所在文件的既有语言，不要统一改写。
- 配置校验集中在 `kproxy-core/src/config.rs`；新增配置项需同时补默认值、校验分支与热重载路径，非法 TOML 必须保留上一份有效配置而不是崩溃。
- 账号文件 `accounts.json` 含凭据、以 `0600` 创建；导出默认带凭据，诊断分享前用 `--redact`。测试与日志中不得回显 token。

## 文档

`docs/startup-and-debugging.md`（含 zh-CN 版）覆盖环境加载、原生/Docker/systemd 启动、日志与 trace ID、VS Code + LLDB、故障排查。两份上下文相关的方案文档互为前提，改动压缩链路前都应先读：

- `docs/model-mapping-context-window.md` —— 模型映射下源/目标模型输入窗口不一致的处理方案（服务端兜底压缩，借 compaction 块携带边界标记）。决定"什么时候压"。改动 `handlers.rs` 的压缩触发、`check_context_limit` 或 `map_model` 前先读。
- `docs/context-compaction-quality.md` —— 压缩质量本身：现状是字符截断而非语义摘要，含一处现存 bug 与官方 compaction 的对照方案。决定"压得好不好"。改动 `tokenizer.rs` 的 `compact_kiro_payload` 前先读。

# 多提供源与 GitHub Copilot 接入

`kproxyd` 现在通过 provider registry 管理模型来源。未配置 `[[provider]]` 时仍自动创建
`kiro`，已有配置和请求行为不变；一旦显式配置 provider，需要把仍要使用的 Kiro 实例也保留为
`id = "kiro"`、`kind = "kiro"`。内置 Kiro 的实例 ID 固定为 `kiro`，Copilot 可以创建多个实例，
例如 `copilot-personal`、`copilot-team`，各自拥有独立账号文件、token 缓存、并发限制和模型缓存。

## 快速配置 Copilot

先添加 provider。`--setting` 接受 `KEY=TOML_VALUE`，字符串值需要保留 TOML 引号：

```bash
kproxy provider add \
  --id copilot \
  --kind copilot \
  --setting 'client_id="Iv1.replace-me"' \
  --max-concurrent-per-account 2

kproxy provider show copilot
```

Device Flow 需要 GitHub OAuth 应用的 client ID。也可以跳过 Device Flow，从标准输入导入已有
GitHub token，避免凭证出现在命令行参数和 shell 历史中：

```bash
kproxy account add --provider copilot --auth device-flow

# 或者
printf '%s\n' "$GITHUB_TOKEN" | \
  kproxy account add --provider copilot --token-stdin
```

Device Flow 会先取得 GitHub 用户 token，再通过
`GET /copilot_internal/v2/token` 交换短期 Copilot API token。动态返回的 API endpoint 会校验
HTTPS 与主机名。账号 token、刷新 token 和探测结果保存在
`$KPROXY_HOME/providers/<provider-id>/accounts.json`，使用原子写入和 `0600` 权限；逐账号模型目录
持久化到同目录的 `models.json`，成功返回空目录时也会清除旧缓存，失败时只在配置的 stale 窗口内
使用最后一次成功结果。
启用了 expiring user token 的 OAuth 应用可以通过 `client_secret` 设置支持 refresh token 轮换：

```bash
kproxy provider edit copilot \
  --setting 'client_secret="replace-me"'
```

生产环境不要启用 `allow_insecure_http`。GitHub Enterprise 或自建测试端点可配置
`github_host`、`github_api_base`、`oauth_base` 和 `allowed_endpoint_hosts`；完整字段和默认值见
`kproxy config show provider` 或生成的默认配置文件。

adapter 会发送 Copilot 要求的 `Editor-Version`、`Editor-Plugin-Version`、
`Copilot-Integration-Id` 和 `User-Agent` 身份头。Claude 原生请求只转发已经验证可用的 beta，
并移除 Copilot 明确拒绝的 `fallbacks`、`container`、`mcp_servers`；图片请求自动添加 vision
标记，Chat 流式请求自动请求最终 usage 事件。

## 创建单来源或多来源服务

创建 Copilot-only 服务时，服务与自动生成的 API key 会使用相同 provider 范围：

```bash
kproxy service create \
  --name copilot \
  --host 127.0.0.1 \
  --port 5581 \
  --provider copilot \
  --default-provider copilot
```

一个监听也可以同时开放 Kiro 和 Copilot：

```bash
kproxy service create \
  --name unified \
  --host 127.0.0.1 \
  --port 5580 \
  --provider kiro \
  --provider copilot \
  --default-provider kiro
```

多来源服务的 `GET /v1/models` 会返回 `kiro/<model>`、`copilot/<model>` 形式的 ID，避免不同
来源的同名模型冲突。请求也可显式使用该前缀；未带前缀时使用服务的 `default_provider`。
provider 范围同时受服务和 API key 限制，取两者交集。API key 还可限制模型 glob：

```bash
kproxy apikey edit <KEY_ID> \
  --provider copilot \
  --model 'copilot/claude-*' \
  --model 'copilot/gpt-*'
```

模型权限按映射、别名解析与回退后的最终模型判断；受限 API key 不能借助 Kiro 的账户级映射、默认模型或容量回退访问白名单外模型。

## 协议和模型映射

Copilot adapter 原生转发以下接口，并保留 JSON 或 SSE 响应：

- `POST /v1/messages`
- `POST /v1/messages/count_tokens`
- `POST /v1/chat/completions`
- `POST /v1/responses`
- `GET /v1/models`

模型映射共用同一套规则引擎，可以按 provider、代理服务和 API key 限定：

```bash
kproxy model-map add \
  --name copilot-fast \
  --provider copilot \
  --service svc_example \
  --source team-fast \
  --target gpt-5-mini

kproxy model-map test team-fast \
  --provider copilot \
  --service svc_example
```

旧规则没有 `providers` 时只作用于 Kiro，新增 Copilot 不会意外继承旧映射。跨 provider 的映射
需要在源 provider 上显式开启，再把目标写成 `provider/model`：

```bash
kproxy provider edit kiro --allow-cross-provider-fallback true
kproxy model-map add \
  --name kiro-to-copilot \
  --provider kiro \
  --source team-copilot \
  --target copilot/gpt-5-mini
```

跨 provider 规则会在账号调度前完成一次目标选择，包括同时含本地与跨源目标的
`loadbalance`。`max_remaining_credit_percent` 依赖已经选中的 Kiro 账号，因此只能映射到
Kiro 本地模型；配置成跨 provider 目标会在配置校验时被拒绝，避免把 provider 前缀误发给
Kiro 上游。

`[provider.routing].default_model_id` 是该来源没有命中显式规则时的默认目标。Kiro 仍保留原有的
账号额度条件、动态模型解析，以及 `enable_model_fallback` 控制的上游容量错误同族 fallback。
Copilot 映射在 provider adapter 前执行，并在调度前按账号模型目录和协议能力过滤；它不会把一个
不支持目标协议的同名模型当作可用模型，也不会对上游容量错误自动更换模型，需使用显式映射或
默认目标控制该行为。

## 统一和按来源管理

不指定范围的读取命令会聚合 provider；写操作要求能唯一定位账号，也可显式指定 provider：

```bash
kproxy provider list
kproxy status --provider all
kproxy ready --provider copilot
kproxy account list --provider all
kproxy account list --provider copilot
kproxy account show copilot/<ACCOUNT_ID>
kproxy account tag --provider copilot <ACCOUNT_ID> --add team-a
kproxy account probe --provider copilot <ACCOUNT_ID>
kproxy account reset-health --provider copilot <ACCOUNT_ID>
kproxy account refresh --provider copilot --all
kproxy account export --provider all --redact
kproxy models list --provider all
kproxy models list --provider copilot --refresh
kproxy tasks run model_cache_refresh --provider copilot
kproxy pool --provider copilot --model gpt-5-mini
kproxy service list --provider copilot
kproxy apikey list --provider copilot
kproxy apikey usage <KEY_ID> --provider copilot
kproxy stats --provider copilot --detail --by model
kproxy logs show --provider copilot
kproxy subscriptions --provider all
```

provider 本身可通过 `add`、`edit`、`enable`、`disable`、`delete` 管理。删除 provider 只删除配置，
不会删除其账号文件；配置校验会阻止删除仍被服务、API key 或映射规则引用的 provider。

Copilot 响应中的标准 token usage 会写入 API key 用量和持久化统计。流式响应按 SSE 事件增量
解析，没有 usage 的上游响应会标记为 `unreported`，不会伪造 token 数。`copilot_usage` 会以带
schema 版本的 provider 原生计费对象原样保存；存在 `total_nano_aiu` 时同时记录精确的
`nano_aiu` 整数。Copilot 的 Kiro credits 维度保持 `0`，两种计量单位不会相互换算。

`status`、`ready`、账号、模型、服务、API key 用量、统计和日志都支持 provider 范围。订阅查询
在 `--provider all` 中会明确标记每个来源是否支持；Copilot 当前没有权威订阅接口，因此返回
`supported = false`，不会伪造空套餐。Kiro 的 `diagnose all/endpoints` 仍是 Kiro 上游专项诊断；
Copilot 使用 `account probe --provider <id>` 验证 GitHub 用户、token exchange 和模型目录链路。

## 配置示例

等价 TOML 如下：

```toml
[[provider]]
id = "kiro"
kind = "kiro"
enabled = true

[[provider]]
id = "copilot"
kind = "copilot"
enabled = true

[provider.settings]
client_id = "Iv1.replace-me"
allowed_endpoint_hosts = ["api.githubcopilot.com"]
user_agent = "GitHubCopilotChat/0.31.0"

[provider.pool]
max_concurrent_per_account = 2

[provider.models]
cache_ttl_ms = 300000
max_stale_ms = 1800000

[provider.routing]
default_model_id = ""
enable_model_fallback = false
allow_cross_provider_fallback = false
```

`client_id` 由部署者提供，项目不内置第三方 OAuth 身份。没有真实 Copilot 账号时可以完成编译、
配置、mock 协议与持久化测试，但最终上线前仍应使用目标 GitHub 组织的真实账号验证 Device Flow、
token exchange、模型列表、三种生成协议和流式 usage。

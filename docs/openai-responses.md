# OpenAI Responses 与 Codex 接入

业务面提供 `POST /v1/responses` 和 `POST /responses`，支持非流式 JSON 与流式 SSE。
请求复用现有 OpenAI → Kiro 翻译、账号池、模型映射、计费、预算、重试和工具参数校验。
流式模式逐段转换上游输出；最终响应仍含完整 output 和 usage。

## Codex 配置

在用户级 `~/.codex/config.toml` 中配置 provider；请保留已有的其他配置：

```toml
model = "claude-sonnet-4.5"
model_provider = "kiro"

[model_providers.kiro]
name = "Kiro Proxy"
base_url = "http://127.0.0.1:5580/v1"
wire_api = "responses"
env_key = "KPROXY_API_KEY"
requires_openai_auth = false
```

将 `KPROXY_API_KEY` 设置为该代理服务生成或允许的 API key，然后启动 Codex。
服务地址和 model 按实际部署及账号可用模型调整。配置字段见
[Codex 官方配置参考](https://learn.chatgpt.com/docs/config-file/config-reference)。

## 客户端准入

`server.enforce_user_agent_check` 默认为 `true`，旧配置无需增加新字段：

| 路由 | 允许的客户端 |
| --- | --- |
| `/v1/messages`、`/messages`、`/anthropic/v1/messages` 及对应 `/count_tokens` | Claude Code |
| `/v1/responses`、`/responses` | Codex |
| `/v1/chat/completions`、`/chat/completions` | Codex |
| `/v1/models`、`/models` | Codex |

检查使用 User-Agent 中的客户端产品标识，覆盖 Codex CLI、exec、编辑器和桌面端。
仅有 `originator` 请求头或 User-Agent 中随意包含 `codex` 不会通过检查。
认证先于客户端检查；拒绝请求使用对应协议的错误格式，且不访问 Kiro 上游。
健康检查及遥测路由保持原有行为。

需要其他协议客户端接入时，可将此开关设为 `false` 并重载配置。
API key 认证、服务级 key 白名单、额度和并发限制继续生效。

## 支持范围

| Responses 能力 | 处理方式 |
| --- | --- |
| `input` 字符串 | 转换为 user 消息 |
| 消息数组 | 支持 system、developer、user、assistant；支持 input_text 和 output_text |
| `instructions` | 转换为受保护的 system 上下文 |
| `input_image` | 支持公开 HTTP(S) URL 和 base64 data URL，复用图片下载与校验限制；工具结果也可返回图片 |
| `function_call` / `function_call_output` | 保留 call_id、名称、JSON 参数和结果；校验调用与结果配对 |
| `custom_tool_call` / `custom_tool_call_output` | 自由文本工具映射为 Kiro 的 input 字符串参数，返回时恢复原格式 |
| function、custom、namespace 工具 | 同时读取顶层 `tools` 与 Responses Lite 的 `input[].additional_tools`；展平命名空间后复用名称规范化，返回时恢复 namespace 与名称；拒绝名称冲突 |
| `tool_choice` | 支持 auto、none、required、指定 function/custom 工具，以及仅限 function/custom 的 `allowed_tools` |
| `parallel_tool_calls` | 沿用 Chat Completions 的工具选择与提示约束 |
| `store` / `previous_response_id` | 默认启用的受限进程内续轮；`store: false` 显式关闭，按服务与 API key 隔离，不写磁盘 |
| 工具结果后的空上游轮次 | 无可见文本、无后续工具且未触发输出上限时重试一次；再次为空则返回 502 / `response.failed`，不误报完成 |
| `max_output_tokens` | 映射为 Kiro maxTokens；省略时不补默认生成上限 |
| `temperature`、`top_p` | 复用现有参数校验和映射，保留零值 |
| `reasoning.effort` | 复用现有 effort 映射；none 关闭本次推理 |
| 推理输出与历史 | 返回 reasoning summary 事件/条目；回传的明文摘要保留在 assistant 上下文 |
| `text.verbosity` | 转为回答详略的 system 提示；不提供硬性字数保证 |
| `text.format`、工具 `strict` | 作为兼容提示接收并忽略；不因 JSON Schema 或 strict=true 拒绝请求 |
| `metadata` | 回显为响应元数据，不作为客户端认证凭据 |
| usage | 返回 input_tokens、output_tokens、total_tokens、cached_tokens、reasoning_tokens |

`custom.format` 的 grammar 定义会附加到 custom 工具描述中作为模型输入提示；Kiro 不提供原生语法约束，
因此这不是服务端强校验。
`reasoning.summary/context`、`include`、`service_tier` 和未使用的 `stream_options` 键宽松接收；
输出使用 Kiro 可提供的数据，不按官方参数枚举额外拒绝。未知的 `text.verbosity` 不添加详略提示。
`prompt_cache_key` 用作 Kiro 会话/Prompt Cache 亲和提示，经 API key 隔离后哈希为 UUID，不原样传给
Kiro。`prompt_cache_retention`、`safety_identifier`、`user` 和 `client_metadata` 作为客户端元数据
接受，不传给 Kiro；缓存命中仍由既有缓存规则和上游决定。
可选的 `stream`、`include`，以及 function 工具的 `description`、`parameters`、`strict`
显式传 `null` 时按省略处理；`parameters: null` 使用无参数工具的默认 schema。

## 无状态与受限状态续轮

与 OpenAI Responses 一致，`store` 缺省或为 `true` 时，完成的 response 会暂存于代理进程内；
下一轮可只提交新输入和
`previous_response_id`。代理会恢复此前 input/output，并在新请求省略两种工具声明渠道时继承此前的
function/custom 工具及 tool choice。显式 `tools: []` 或 `additional_tools` 控制项会提供新的有效
工具集，而不是叠加进已存目录；此时也不继承此前的 tool choice。`additional_tools` 本身不会作为
对话历史反复存储，解析出的有效工具会单独继承。续轮复用父 response 已解析出的 Kiro conversation ID，不受后续请求缺失或更换
session-affinity 请求头影响。

显式传 `store: false` 时保持无状态：下一轮需把上一轮的 `output` 和对应工具执行结果追加到
`input`。`function_call_output.call_id` 对应调用的 `call_id`，不是 `fc_...` 条目 ID。

这是有边界的兼容实现，而非 OpenAI 的持久化存储服务：会话仅在进程存活期间可用，空闲 30 分钟
过期，最多保留 256 个会话；单会话最大 2 MiB、总量最大 32 MiB。无法保存状态时，非流式请求
返回服务端错误，流式请求以 `response.failed` 结束，避免成功返回一个无法续轮的 response。
会话按代理 service 和已认证 API key 隔离，重启后全部清空；不提供
response 的查询、删除或取消端点。按照官方语义，父 response 的 `instructions` 不会自动带入
续轮，续轮可单独设置新的 `instructions`。

以下参数仍需要尚未实现的执行/数据链路，因此返回 HTTP 400：

- `conversation`、`background: true`。
- `truncation: auto`、`context_management`、`max_tool_calls`。
- defer_loading 工具、托管工具（如 web_search、file_search、computer、MCP server 执行）。
- Files API 的 file_id、input_file；附加控制字段则兼容忽略。

输入历史里无法转换的条目**不再拒绝**，而是跳过后继续处理，因为 Codex 会把上一轮收到的条目原样回传，
拒绝会让第二轮起的整段会话不可用：

- `reasoning` 条目的非空 `encrypted_content`（Kiro 无法解密）：丢弃该不透明块，保留同条目内的明文
  `summary`/`content`。
- `item_reference`、托管工具调用条目（如 `web_search_call`）及其他未识别类型：跳过。
- 上述跳过会记录 debug 事件 `proxy.compatibility.controls_ignored`，只含固定字段/类型名。
- 若输入**只**由这些可跳过条目组成、不含任何真实消息，仍按缺少会话内容拒绝。

Codex 常用的 `include: ["reasoning.encrypted_content"]` 可以提交，但本代理只返回明文推理，
不会伪造加密 replay token。这里不提供 WebSocket 或 `/responses/compact`；长上下文仍应由客户端
维护或自行裁剪。

## 流式事件

流按顺序发送 `response.created`、`response.in_progress`、output item/content part 的 added、
文本/推理/工具参数的 delta 和 done，再发送 `response.output_item.done`。
事件带连续递增的 `sequence_number`，item_id、output_index、call_id 在同一响应中保持稳定。

正常结束使用 `response.completed`；达到输出上限使用 `response.incomplete`，
`incomplete_details.reason` 为 `max_output_tokens`。上游中断或工具 JSON 损坏使用
`response.failed`，不发送成功结束事件，也不把未完成的工具调用标记为可执行。
Responses 不使用 Chat Completions 的 `[DONE]` 结束标记。

协议结构依据 [Responses 官方参考](https://developers.openai.com/api/reference/resources/responses)
和 [流式事件参考](https://developers.openai.com/api/reference/resources/responses/streaming-events)。

## 参考项目核查

2026-09-04 检查四个项目的默认分支快照：

| 项目与快照 | Responses 实现 |
| --- | --- |
| [jwadow/kiro-gateway · a5292ca](https://github.com/jwadow/kiro-gateway/blob/a5292ca04c7c6231e0b47673ac3f981f5a706e1e/kiro/routes_openai.py) | 无；OpenAI 路由为 Chat Completions 和模型列表 |
| [hj01857655/kiro-account-manager · c5c4776](https://github.com/hj01857655/kiro-account-manager/blob/c5c477647f8cba4c9b9f07e8fb41e403672adf36/src-tauri/src/gateway/mod.rs) | 有；gateway 路由、converter、proxy 中包含 Responses 转换和输出 |
| [chaogei/Kiro-account-manager · 447adcd](https://github.com/chaogei/Kiro-account-manager/blob/447adcdb468157312621b1f09448278bd9bca748/Kiro-account-manager/src/main/proxy/proxyServer.ts) | 有；Responses 转 Chat，再输出 Responses JSON/SSE |
| [ZyphrZero/kiro.rs · f357292](https://github.com/ZyphrZero/kiro.rs/blob/f3572929fbc2c0c090c29b13a7c285d1b2777dcd/src/anthropic/responses.rs) | 有；额外覆盖 Responses Lite `additional_tools`、function/custom/namespace 桥接、工具结果后空响应重试、Responses SSE 与服务端 WebSearch loop |

hj01857655、chaogei 和 ZyphrZero 三个实现的许可分别为 CC BY-NC-SA 4.0、AGPL-3.0 与 MIT。
本实现参考其能力拆分，按官方协议在本项目的 Rust 执行链中独立实现，未复制其源代码；本仓库继续使用 MIT 许可。

回归覆盖请求转换、可空参数、命名空间/自由文本工具、图片工具结果、协议准入矩阵、SSE 分片和错误，
以及真实 daemon 对模拟 Kiro 上游的两轮工具交互（完整历史 replay 和 `store`/`previous_response_id`
续轮）、空工具续轮的单次恢复与重复失败，涵盖两个别名、两种流模式和工具缓冲开关。
工具分片用例包含交错的 function/custom 调用，以及只在首个片段发送工具名的上游输出。

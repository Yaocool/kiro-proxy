# Kiro 网关兼容性基线

参考各项目的实际接入、转换和输出行为，并核对当前 Claude Code 的客户端契约。
2026-09 的 Claude 兼容修复优先采用 kirocc 的 effort/runtime 实现及 kiro.rs 的工具恢复边界。

## 参考快照

| 项目 | 固定版本 | 本轮参考行为 |
| --- | --- | --- |
| [jwadow/kiro-gateway](https://github.com/jwadow/kiro-gateway/blob/a5292ca04c7c6231e0b47673ac3f981f5a706e1e/kiro/models_anthropic.py#L297-L340) | `a5292ca` | 请求允许额外字段；Claude/OpenAI 转换器不使用输出格式参数 |
| [hj01857655/kiro-account-manager](https://github.com/hj01857655/kiro-account-manager/blob/c5c477647f8cba4c9b9f07e8fb41e403672adf36/src-tauri/src/gateway/models.rs#L484-L504) | `c5c4776` | Claude 请求反序列化忽略 output_config 等额外字段 |
| [chaogei/Kiro-account-manager](https://github.com/chaogei/Kiro-account-manager/blob/447adcdb468157312621b1f09448278bd9bca748/Kiro-account-manager/src/main/proxy/translator.ts#L994-L1024) | `447adcd` | Claude 转换器不读取 output_config.format；OpenAI response_format 仅有类型声明，转换时忽略 |
| [d-kuro/kirocc](https://github.com/d-kuro/kirocc/tree/e0850b11e1a61cbb84aa55739a41b21d4a5c5fcf) | `e0850b1` | Claude effort、envState、Kiro API key、regional runtime / management |
| [ZyphrZero/kiro.rs](https://github.com/ZyphrZero/kiro.rs/tree/f3572929fbc2c0c090c29b13a7c285d1b2777dcd) | `f357292` | 文本工具恢复：行首、代码围栏外、已声明工具名三重检查 |
| [pinealctx/kiro-gateway](https://github.com/pinealctx/kiro-gateway/tree/9f614d4b270e8e80e13c7d3603cbb1b090f327c4) | `9f614d4` | Claude Code 非 Claude 模型的 anthropic. 发现别名 |

hj 的 OpenAI 路径另有 JSON 提示词引导，但 json_schema 主要使用名称/描述，没有传入完整 Schema。
Claude/OpenAI 格式兼容保留 jwadow/chaogei 的接收并忽略策略；Claude task budget 也作为未使用的
附加提示接收。没有新增提示词、结果校验或生成重试来模拟这些约束。
三个项目存在差异时，应明确采用哪一条实际路径，而不是宣称三者实现完全相同。

## 当前兼容策略

- Claude output_config.effort 按模型元数据映射，显式 effort 优先于 thinking budget；取值校验为
  low/medium/high/xhigh/max。消息级 effort 仅允许用于 system 消息，从后续 user 轮起生效，保留经过
  compaction 的有效 effort；该映射不承诺 Anthropic 原生 per-message prompt-cache 行为。
- Claude output_config.format/task_budget 接收并忽略；只记录字段名的 debug 诊断，不影响 effort 映射。
- Chat Completions 的 response_format 和 Responses 的 text.format 接收并忽略。
- 工具 strict=true、Claude eager_input_streaming=true 接收为兼容提示，保留原有工具输入 Schema。
- Claude 顶层、消息、工具附加字段，以及 OpenAI/Responses 的额外请求字段，均不因未知而拒绝。
- service_tier、未使用的流式选项、Responses 的 include、prompt_cache_retention、
  reasoning.summary/context 不强制官方枚举；已有输出行为不变。
- 已知忽略字段记录 debug 事件 proxy.compatibility.controls_ignored；只记录固定字段名，
  不输出用户 Schema、提示词或任意参数值。默认日志级别不产生额外告警。
- 不把这些字段原样塞进 additionalModelRequestFields；Haiku 缺少原生参数元数据时仍省略整个扩展。

- Responses 输入历史中无法转换的条目跳过而非拒绝：`reasoning.encrypted_content` 丢弃不透明块并保留
  明文摘要，`item_reference` 与托管工具调用条目跳过。初始依据是 chaogei 的 `/v1/responses` 实现
  （`translator.ts` 的 `responsesToOpenAIChat`）只校验它实际读取的字段，且把 `reasoning` 定义为
  `unknown` 从不检查；Codex 会回传上一轮条目，拒绝会使第二轮起整段会话失效。输入只含可跳过条目时仍拒绝。
- Responses Lite 的 `input[].additional_tools` 是已知控制项，不属于可跳过历史。依据 ZyphrZero/kiro.rs
  与 Codex 0.144+ 的实际请求形态，它和顶层 `tools` 一起组成有效工具目录；状态存储只保留解析后的目录，
  不把控制项反复追加到对话历史。两处完全相同的声明去重，名称相同但定义冲突时仍拒绝。

宽松接收不代表新增原生 JSON Schema 约束，返回仍是正常 Kiro 生成结果。实现和文档须准确描述
哪些参数实际使用、哪些参数忽略，不把 HTTP 成功和严格结构保证混为一谈。

## 校验边界

保留读取已知数据所需的类型检查、模型/内容必填、工具引用完整性、请求/Schema/附件资源上限、
远程附件访问安全和客户端认证。不得因为本次放宽附加字段而绕过这些检查。

这次调整覆盖可忽略的格式和附加提示；Responses 默认存储（显式 `store=false` 时关闭），
`previous_response_id` 使用受限的
进程内历史引用存储（按 service/API key 隔离、会过期且不落盘）。它不等同于持久化存储服务，仍不实现
后台任务、Files API 数据获取或新的托管工具执行器。已有这些能力的限制仍保留；未来对齐时需要一起实现
对应数据/执行路径。

添加新的拒绝条件前，检查参考项目的对应路径，说明拒绝是实际转换所需、Kiro 能力限制，
还是资源/安全边界。不能仅因 Kiro 没有可用的原生映射，就把此前接受的附加提示改成请求拒绝。
已实测会导致上游错误的参考逻辑（如缺少元数据时猜测发送 adaptive thinking）不照搬。

## 回归验证

运行 `cargo test -p kproxy-translate --test compatibility_controls` 检查参数接收与真实序列化结果；
运行 `cargo test -p kproxyd --test end_to_end compatibility_controls::` 检查三种 API 的
流式/非流式路径。端到端测试使用本地模拟上游，断言正常内容和工具 Schema 保留、兼容字段不泄漏，
不需要生产账号，也不产生上游用量。

新增回归：`cargo test -p kproxy-translate --test claude_gateway_controls` 覆盖 effort、envState
及发现别名；kproxyd 的 ToolLeakFilter 测试覆盖跨分片围栏、无效 XML 和未声明工具；
kproxy-kiro 的 mock 测试覆盖 API key runtime、OAuth management 和区域隔离。

## 复查补充（2026-09-07）

复查时再次核对 kirocc、ZyphrZero/kiro.rs、pinealctx/kiro-gateway 的 GitHub HEAD，
与上面的固定快照一致。针对本地实现补上以下边界，而非直接复制参考项目的启发式行为：

- XML 恢复排除 `<thinking>` 内容；其中未闭合的 Markdown 不影响后续工具调用，代码示例里的
  thinking 标签也不会错误开启 reasoning 状态。所有分片边界均有回归。
- XML 参数不再裁掉首尾空白；Schema 声明为字符串的参数不再误转为布尔值、数字或 JSON 对象。
- `/v1/models` 的已识别 Codex User-Agent 优先于附带的 `anthropic-version`，避免误判为 Claude。
- `ksk_` 回显在错误分类前脱敏，KiroError 的 Display/Debug 也脱敏；区域 runtime/management
  和 Q/CodeWhisperer 地址统一清理，额度分支不会绕过这些保护。
- API key 被拒绝时跳过 OAuth 刷新协调和刷新失败告警；若其他请求已完成手动密钥轮换，允许复用新密钥。

后续复查继续补上流处理边界：

- usage/metadata 等非文本事件不会终止或打断跨分片的 thinking 标签；隐藏设置持续有效。
  thinking 过滤与工具恢复使用相同的 Markdown 位置判定，不再删掉代码示例中的标签。
- tagged reasoning 只保留可能组成结束标签的短后缀，正文按开关立即输出或丢弃，避免缓冲随
  reasoning 长度增长；输出仍是原始 tagged text，不伪造原生签名。
- 已恢复工具的参数不作为 Markdown 解析；代码/thinking 内未闭合的 XML 示例也不会吞掉
  后续真正的工具调用。
- XML 字符串类型提示支持本地 `$ref`、enum/const、allOf/anyOf/oneOf（含根 Schema 组合）。
  外部引用、缺失引用及超过遍历预算的结构保留为原 XML，不联网解析或猜测参数；这不是完整 Schema 校验器。
- 非流式累积器不再二次替换工具标签或文本字节，代码示例与 SSE 内容保持一致，不因分片方式而丢字。

`cargo test -p kproxyd --test end_to_end claude_gateway::` 覆盖真实 daemon 加本地模拟上游，
包括 XML 逐字符分片、流式/非流式、工具缓冲开关、thinking 开关的组合。
`cargo test -p kproxyd --test end_to_end thinking_markup_survives_interleaved_usage` 覆盖
Claude、Chat Completions、Responses 三种协议，在 thinking 开关和流式开关下的 12 种组合。
这不是生产 Kiro 服务可用性验证，不消耗真实账号额度。

生产回归修正：`1871981` 曾将非 null 的 Claude `output_config.format/task_budget` 改为本地 400，
导致升级前可用的请求在账号选择之前失败。现恢复接收并忽略的兼容行为，保留 effort 校验和映射。
回归用例必须保留非 null 的真实格式 Schema，覆盖 Messages/别名/token counting、流式/非流式，
并断言格式 Schema 和 task budget 不进入 Kiro 请求；不得用 null 替换格式字段来规避兼容性验证。

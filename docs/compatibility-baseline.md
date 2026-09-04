# Kiro 网关兼容性基线

优先参考三个项目的实际接入、转换和输出行为。Claude/OpenAI 官方资料用于理解客户端字段形状，
不因官方附加能力无法完整复刻，就增加参考网关没有的拒绝规则。

## 参考快照

| 项目 | 固定版本 | 本轮参考行为 |
| --- | --- | --- |
| [jwadow/kiro-gateway](https://github.com/jwadow/kiro-gateway/blob/a5292ca04c7c6231e0b47673ac3f981f5a706e1e/kiro/models_anthropic.py#L297-L340) | `a5292ca` | 请求允许额外字段；Claude/OpenAI 转换器不使用输出格式参数 |
| [hj01857655/kiro-account-manager](https://github.com/hj01857655/kiro-account-manager/blob/c5c477647f8cba4c9b9f07e8fb41e403672adf36/src-tauri/src/gateway/models.rs#L484-L504) | `c5c4776` | Claude 请求反序列化忽略 output_config 等额外字段 |
| [chaogei/Kiro-account-manager](https://github.com/chaogei/Kiro-account-manager/blob/447adcdb468157312621b1f09448278bd9bca748/Kiro-account-manager/src/main/proxy/translator.ts#L994-L1024) | `447adcd` | Claude 转换器不读取 output_config.format；OpenAI response_format 仅有类型声明，转换时忽略 |

hj 的 OpenAI 路径另有 JSON 提示词引导，但 json_schema 主要使用名称/描述，没有传入完整 Schema。
本轮格式兼容选择 jwadow/chaogei 的接收并忽略策略，不引入提示词、结果校验或生成重试。
三个项目存在差异时，应明确采用哪一条实际路径，而不是宣称三者实现完全相同。

## 当前兼容策略

- Claude 的 output_config（format、effort 及额外键）接收并忽略。
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

添加新的拒绝条件前，先检查三项目的对应路径，说明拒绝是实际转换所需、已观察到的 Kiro 限制，
还是资源/安全边界。不要仅因某字段不在官方枚举中，或没有公开的 Kiro 文档，就默认拒绝。
已实测会导致上游错误的参考逻辑（如缺少元数据时猜测发送 adaptive thinking）不照搬。

## 回归验证

运行 `cargo test -p kproxy-translate --test compatibility_controls` 检查参数接收与真实序列化结果；
运行 `cargo test -p kproxyd --test end_to_end compatibility_controls::` 检查三种 API 的
流式/非流式路径。端到端测试使用本地模拟上游，断言正常内容和工具 Schema 保留、兼容字段不泄漏，
不需要生产账号，也不产生上游用量。

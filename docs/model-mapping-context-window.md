# 模型映射的上下文窗口一致性方案

## 结论

当前 compact 已经具备可用的语义压缩主链路，模型映射问题不再是“补一个自动触发条件”这么简单。要让源模型、映射模型、账号实际模型之间的上下文窗口真正一致，需要同时补齐四件事：

1. 在翻译阶段提前保护 system prompt，使自动触发的 compact 不会丢掉系统指令。
2. 在第一次上游调用前，按路由得到的映射模型窗口主动触发 compact。
3. 保证摘要请求本身装得下待压缩上下文；否则大窗口映射到小窗口时，语义摘要会先超限，只能退回抽取式压缩。
4. 账号选定后按实际解析模型再做一次精确校验；窗口更小时只允许一次重规划，且同一主请求最多生成一次语义摘要。

第一阶段只对 Claude Messages 的首次上游调用启用自动 compact。OpenAI 协议没有可回传的 `compaction` 块，已经开始输出后的 Tool Search 轮次也不能安全地补发位于响应首部的 compact 边界，这两类场景继续返回明确的上下文错误，不能静默删历史。

## 实现状态（2026-08-20）

第一阶段已经按本文方案落地，配置开关 `context.auto_compact_on_overflow` 默认仍为 `false`。已实现映射窗口主动触发、摘要 payload 容量预检、可复用的 `CompactionArtifact`、账号实际窗口下的单次重规划、独立摘要用量迭代、流式首块顺序和压缩后的保守 prompt-cache 断点。流式 compaction prelude 会暂存到首个成功且通过工具参数校验的语义轮次，避免在上游尚可切号重试时过早提交客户端数据；摘要超时后后台任务只在有界宽限期内继续独立结算，到期会主动取消流并结算已解码 usage。分块滚动摘要、prepared-dispatch 重构以及 Tool Search 已开始输出后的 pause/resume compact 仍是后续工作。

## 目标与约束

方案必须满足：

1. **映射硬生效。** 目标模型装不下时不能回退到源模型，否则降本规则失效。
2. **客户端尽量无感。** Claude Code 获得正常响应，并通过协议内 `compaction` 块携带服务端边界。
3. **token 上报真实。** `count_tokens` 延续现有语义，返回应用客户端边界/编辑后的真实 token，并在有编辑时保留 `original_input_tokens`；生成响应的 `usage.input_tokens` 返回实际发给主模型的 token，不通过虚报诱导客户端压缩。
4. **一次请求有界。** 主请求最多生成一次语义摘要、最多做一次账号级精确重规划，不允许递归 compact。
5. **工具配对和系统指令不能被破坏。** 压缩边界必须保持 tool use/result 配对，system prompt 必须存在于压缩后的有效上下文中。

## 当前实现基线

以下是 2026-08-20 的代码事实。

| 能力 | 当前状态 | 实现位置与影响 |
| --- | --- | --- |
| 客户端 compact 边界 | 已实现 | `apply_compaction_boundary` 会丢弃最后一个 `compaction` 块之前的历史 |
| 语义摘要 | 已实现 | `compaction_summary_payload` 生成普通 Kiro 请求，`generate_compaction_summary` 独立调用摘要模型 |
| 近期轮次保留 | 已实现 | `plan_kiro_compaction` / `apply_semantic_compaction` 按 turn 边界保留近期结构化内容 |
| 摘要失败降级 | 已实现 | 超时、模型错误或摘要不满足目标时回退 `compact_kiro_payload` 抽取式压缩 |
| 上下游摘要一致 | 已实现 | 同一个最终 checkpoint 同时进入 Kiro payload 和 Claude `compaction` 块 |
| 流式 compact 输出 | 已实现 | `compaction` block 位于 resumed server events 及 text/tool block 之前，与非流式首块一致 |
| 触发来源 | 已实现 | 客户端 `compact_*` edit 与开启开关后的映射窗口溢出都进入统一 `CompactionDecision` |
| 账号级窗口校验 | 已实现 | 首次 `ContextLimit` 可按实际模型窗口复用 artifact 重规划一次，第二次仍超限则直接返回 |
| 摘要模型容量保护 | 已实现 | 摘要前独立估算 payload；元数据窗口装不下时跳过上游调用并记录 `summary_capacity_insufficient` |
| Tool Search 中途 compact | 未实现 | 内部轮次增长后超限直接失败；流式响应开始后也无法安全补发首部边界 |

当前主链路可以概括为：

```text
Claude 请求
  -> 应用历史中的 compaction 边界
  -> 提前启用 compact_mode 保护 system prompt
  -> 按 route.mapped 翻译、估算 token 和生成统一压缩决策
  -> 可选：Kiro 语义摘要 -> semantic compact / extractive fallback
       -> 摘要模型容量预检
  -> 按 route.mapped 做本地窗口校验
  -> 预留主请求额度
  -> execute_upstream 选择账号
       -> 按账号额度重新映射
       -> 解析该账号实际 Kiro model
       -> 再次校验实际 model 窗口
       -> 若窗口更小，释放额度并复用 artifact 重规划一次
       -> 调用 Kiro
```

### 方案所解决的关键矛盾

假设输入为 180k，源模型窗口 1M，映射模型窗口 128k：

- 主请求按映射模型的安全窗口会超限，需要 compact。
- 摘要 payload 仍然接近 180k。
- `compaction_summary_model` 为空时，摘要也使用 128k 映射模型。
- 如果没有容量预检，摘要调用会在窗口校验处失败，最终只能使用抽取式 fallback。

因此，自动触发不能只复用旧 compact 代码。当前实现会先做摘要容量预检；生产配置足够大的 `compaction_summary_model` 时走一次语义摘要，装不下时不发起注定失败的请求并明确降级。摘要容量仍是本方案的 P0 条件，不是后续优化项。

## 官方行为对齐

[Anthropic 官方 Compaction 文档](https://platform.claude.com/docs/en/build-with-claude/compaction)定义的核心行为是：达到 input-token trigger 后生成摘要、把摘要放入类型化的 `compaction` 块、继续生成；后续请求忽略该块之前的内容。官方还明确：

- `compact_20260112` 的默认 trigger 为 150k，客户端显式 trigger 最小为 50k。
- compaction block 必须随响应继续进入后续消息。
- 流式 compaction block 由 start、单个完整 summary delta、stop 构成。
- system prompt 应使用独立 cache breakpoint，避免摘要变化使系统提示缓存一起失效。
- `pause_after_compaction` 可用于先产出边界、再由调用方保留近期消息后续跑。

本项目不是把请求转发给 Anthropic 的原生 compact API，而是在 Kiro 上模拟相同协议语义。因此必须由 kproxy 自己保证摘要调用容量、边界顺序、近期消息保留和下轮裁剪的一致性。

## 窗口分层

不要再用单一的“目标模型窗口”描述整个请求。实际至少存在五个窗口：

| 名称 | 含义 | 判定时机 |
| --- | --- | --- |
| `W_client` | 客户端认定的源模型窗口 | 服务端不可控制 |
| `W_mapped` | handler 初次 `map_model` 得到的映射模型安全输入窗口 | 第一次上游调用前 |
| `W_resolved` | 账号、剩余额度、动态别名解析后实际 Kiro 模型的安全输入窗口 | `execute_upstream` 选定账号后 |
| `W_summary` | 摘要模型能接收的安全输入窗口 | 发起语义摘要前 |
| `W_round` | Tool Search / Web Search / auto-continue 内部轮次增长后的实际窗口 | 每个内部轮次前 |

一致性规则是：

```text
主请求首次发送：input_after_compact <= min(W_mapped, W_resolved)
语义摘要调用：summary_input <= W_summary
内部续跑：next_input <= 当前账号实际模型的 W_round
```

`context.safe_input_ratio` 继续用于普通主请求，`compact_safe_input_ratio` 只用于摘要/compact 相关请求。自动触发点应使用普通安全窗口，压缩目标仍使用 `compact_target_tokens` 的 75% 余量。

## 采纳方案

### 1. 明确协议范围

新增配置仅控制 Claude Messages：

```toml
[context]
# 映射后窗口装不下时，在第一次上游生成前自动 compact。
auto_compact_on_overflow = false

# 已有配置；生产启用自动 compact 时应显式选择能容纳长上下文的模型。
compaction_summary_model = ""
compaction_summary_timeout_ms = 30000
compaction_preserve_recent_turns = 3
```

首个版本默认关闭，完成灰度和质量观测后再改默认值。不要新增 `auto_compact_max_retries`：重规划次数是正确性边界，应固定为一次，而不是可调策略。

OpenAI `/v1/chat/completions` 第一阶段不启用自动 compact。它没有 `compaction` content block，服务端无法把边界可靠带回下一轮；静默压缩会导致每轮重复摘要并使客户端历史与上游历史永久分叉。

### 2. 翻译阶段保护 system prompt

当前 `TranslationOptions.compact_mode` 只在客户端显式携带 compact edit 时开启。该标志会把 system prompt 放到 current turn，避免旧历史被裁掉后系统指令一起消失。

自动模式必须在翻译前开启同样的保护；Claude generation handler 和 `count_tokens` 的两处 `TranslationOptions` 都要同步：

```rust
options.compact_mode = compact_trigger.is_some()
    || config.context.auto_compact_on_overflow;
```

不能等估算 token 后才修改它，因为此时 Claude 请求已经完成到 Kiro payload 的翻译。

### 3. 统一 compact 决策

引入显式原因，避免日志和重试逻辑继续依赖多个 `bool`：

```rust
enum CompactionReason {
    ClientTrigger,
    MappedWindowOverflow,
    ResolvedWindowOverflow,
}

struct CompactionDecision {
    reasons: Vec<CompactionReason>,
    model: String,
    trigger_tokens: u64,
    target_tokens: u64,
    maximum_tokens: u64,
}
```

第一次上游调用前的决策：

```rust
let mapped_max = context_maximum(&state, false, &route.mapped);
let client_trigger = compact_trigger.map(|value| value.min(mapped_max));
let overflow_trigger = (config.context.auto_compact_on_overflow
    && input_tokens > mapped_max)
    .then_some(mapped_max);

let effective_trigger = client_trigger
    .into_iter()
    .chain(overflow_trigger)
    .min();
```

两类 trigger 同时存在时取更小值，并同时记录两个原因。客户端 trigger 的 50k 最小值继续在请求校验层处理；服务端根据实际模型得到的动态 overflow trigger 不受这个协议下限约束。`trigger_tokens` 只决定何时压缩，`target_tokens` 是期望余量，`maximum_tokens` 才是不可恢复上下文错误使用的真实安全上限。

如果 payload 没有可移除历史，或 current turn 加工具定义本身已经超过真实 `maximum_tokens`，应直接返回上下文错误，不要先浪费一次摘要调用。不可拆内容只超过 trigger/target 但仍低于 maximum 时允许放宽操作目标。

### 4. 先保证摘要请求装得下

生成语义摘要前，必须单独估算 `summary_payload`，并按摘要模型做窗口检查：

```text
summary_input <= context_maximum(summary_model, compact = true)
```

处理顺序：

1. 优先使用显式 `context.compaction_summary_model`。
2. 为空时保持当前行为，使用 `route.mapped`，但必须记录它是否具备足够窗口。
3. 元数据预检已经确定装不下时，不发起注定失败的 Kiro 请求，直接进入 fallback 并记录 `summary_capacity_insufficient`。
4. 元数据预检能装下、账号实际解析后仍装不下时，按普通摘要失败处理。

生产启用自动 compact 时，应将 `compaction_summary_model` 配置成能覆盖 `W_client` 主要流量的模型。主请求仍严格使用降本后的映射模型；摘要模型只是一次内部操作，不改变主请求映射结果。

为了彻底消除对大窗口摘要模型的依赖，后续应实现分块语义摘要：按完整 user/assistant turn 和 tool use/result 配对切块，每块控制在 `W_summary` 的约 75%，使用“上一个 checkpoint + 下一块历史”滚动归并，最后再与近期保留轮次组合。分块前禁止按字符切割 JSON、tool 参数或 tool result。

分块摘要属于质量增强，不阻塞第一阶段功能上线；但在未配置大窗口摘要模型时，自动 compact 的质量 SLO 必须按 extractive fallback 统计，不能按 semantic 成功率笼统计算。

### 5. 账号实际模型只允许一次精确重规划

`execute_upstream` 会基于具体账号剩余额度重新执行映射，并通过账号模型缓存解析实际 model，因此 `W_resolved` 可能比 `W_mapped` 小。当前代码在调用 Kiro 前已经会返回 `ExecuteError::ContextLimit`，所以可以安全地在尚未产生任何上游输出时处理。

第一阶段采用最小改造：

1. 先按 `W_mapped` 做预压缩。
2. `execute_upstream` 返回 `ContextLimit(limit)` 时，若开启自动模式且尚未做过账号级重规划，则释放主请求额度 reservation。
3. 按 `limit.model` 和 `limit.maximum` 计算更严格目标。
4. 如果本请求已经生成过语义摘要，复用同一份摘要和原始 `source_payload`，重新执行 `apply_semantic_compaction`；只缩短 checkpoint 或减少近期保留轮次，不再调用摘要模型。
5. 如果之前走的是抽取式 fallback，则从同一份 `source_payload` 按新目标重新抽取。
6. 重新估算 token 和 credits、重新预留额度，再调用一次 `execute_upstream`。
7. 第二次仍因账号实际模型窗口不足时直接返回错误，不继续循环。

当前实现通过以下 compact 中间产物把复用范围保留到首次 dispatch 完成：

```rust
enum CompactionArtifact {
    Semantic {
        source_payload: KiroPayload,
        plan: KiroCompactionPlan,
        summary: String,
    },
    Extractive {
        source_payload: KiroPayload,
    },
}
```

长期可将 `execute_upstream` 拆成“选择账号并解析 model”和“实际 generate”两段，使 handler 在额度预留和生成前直接拿到 `W_resolved`。这能消除一次失败式预检，但改动会触及账号切换、模型 fallback 和流式重试，建议在第一阶段稳定后再做。

### 6. 额度处理

语义摘要已有独立 `/internal/compact` 用量记录和 reservation。自动 compact 需要继续遵循：

- 摘要和主请求分别记账，不能把摘要 token 混进主响应 usage。
- 摘要调用达到客户端等待超时时，主链路立即进入 extractive fallback；已被上游接受的任务只在 250ms–5s 的有界后台宽限期内继续完成结算。宽限到期后主动取消响应流，并用已经解码的 usage（缺失时使用输入估算）完成 lease、reservation、usage 与失败 stats 结算；本地清理再设 5s 硬上限，禁止无限占用账号和 stream slot。
- 账号级精确重规划发生在主 reservation 之后时，先 drop 旧 reservation；当前 `CreditReservation::drop` 会释放未结算额度。
- compact 后重新按实际 `input_tokens` 估算并预留主请求额度。
- 摘要成功后，主请求仍可能因额度不足失败。这是两个独立上游操作无法原子结算的已知代价；后续可增加 combined preflight，但不能把摘要伪装成免费操作。

### 7. 流式和内部轮次边界

首次上游调用前发生的 compact 复用现有流式和非流式编码器。当前流式实现先构造 `compaction_summary` 与 `resumed_server_events`，但在首个成功上游语义事件前暂存；确认该轮可以提交后再按 compaction、resumed events、模型内容的顺序 flush。这样既保持所有路径的 content 首块一致，也不会因为本地 prelude 提前把 `data_started` 置为 true 而关闭鉴权刷新、限流 fallback 和切号重试。

第一阶段不要在已经开始返回数据后自动 compact：

- Tool Search、Web Search 或 auto-continue 使 `W_round` 超限时，继续返回明确错误。
- 如果流式尚未发送任何客户端可见数据，可以在未来复用首次调用前的流程。
- 如果已经发送数据，新的 compaction block 无法再成为 response content 首块；应使用 `pause_turn`/下一次客户端请求完成边界切换，而不是把块插到响应中间。
- 非流式内部轮次理论上可以在最终组装响应前压缩，但应与流式保持同一语义，作为第二阶段统一实现。

### 8. 错误语义

只有以下情形返回不可恢复的上下文错误：

- OpenAI 协议自动 compact 未启用。
- current turn、工具定义或单个不可拆内容本身超过窗口。
- extractive fallback 也无法满足目标。
- 账号级精确重规划后仍遇到更小的实际模型窗口。
- 已开始输出后的内部轮次超限。

沿用 HTTP 400 和 Anthropic `invalid_request_error`，message 必须包含实际解析模型、实际 input tokens 和实际安全窗口。压缩目标通常是安全窗口的 75%，它只是期望余量，不是模型 maximum；不可拆 current turn 高于目标但仍低于安全窗口时，可放宽到安全窗口继续执行，只有最终 payload 超过真实安全窗口才返回上下文错误。不要在此处擅自把 `max_tokens` 与输入窗口相加：项目当前模型元数据分别提供 `maxInputTokens` 和 `maxOutputTokens`，除非上游明确声明共享总窗口，否则应分别校验。

`413 request_too_large` 继续只用于真实 HTTP body 大小超限。

## 数据诚实性与缓存

- `count_tokens` 继续应用请求中已经存在的 compaction boundary 和客户端 context edits，返回编辑后的真实 `input_tokens`；它不执行服务端自动 compact，也不发起有费用的摘要调用。
- 生成响应的 `usage.input_tokens` 返回 compact 后发给主模型的真实 token。
- `count_tokens` 在请求携带 context edits 时继续通过 `context_management.original_input_tokens` 返回编辑前数值。生成路径发生服务端自动 compact 时也应返回对应的压缩前数值，用于解释 usage 差异。
- prompt cache profile 必须在 compact 完成后按最终 payload 计算。system prompt 独立 cache breakpoint 的优化与 Anthropic 官方建议一致；压缩后的缓存 token 必须直接分词计算稳定 system prefix，不能因 profile 只包含 system block 而按整个 compacted payload 的固定比例估算。

`count_tokens` 与生成 usage 数值不同并不构成污染：它们分别描述“客户端提交了多少”和“服务端实际向主模型发送了多少”。需要通过结构化字段和日志解释差异，而不是伪造其中任一数值。

## 可观测性

在现有 `original_tokens`、`compacted_tokens`、`removed_messages` 基础上增加：

- `compaction_reason`: `client_trigger` / `mapped_overflow` / `resolved_overflow`，允许多值。
- `compaction_mode`: `semantic` / `extractive_fallback` / `none`。
- `compaction_summary_model`、摘要模型实际解析结果和 `summary_input_tokens`。
- `summary_capacity_insufficient`、`summary_timeout`、`summary_upstream_error` 等 fallback reason。
- `mapped_context_maximum`、`resolved_context_maximum` 和 `resolved_replanned`。
- 内部摘要 request id；与主请求共享 trace id，但保持独立用量记录。

建议灰度期间重点观察：

- 自动 compact 触发率和成功率。
- semantic 与 extractive fallback 占比。
- 因摘要模型窗口不足导致的 fallback 占比。
- compact 后首次主调用成功率。
- 账号级重规划率和二次仍超限率。
- compact 前后 token 比例及 Tool Search 后超限率。

## 已知风险

### 双重压缩

Claude Code 会按它认定的源模型窗口执行客户端 compact，而服务端按映射后小窗口更早 compact。客户端已验证会回传 compaction block，但不会据此裁掉本地历史，因此长会话最终仍可能发生客户端二次摘要。服务端无法完全消除该结构性问题，只能通过高质量 checkpoint、保留近期轮次和可观测性减轻损失。

### 摘要模型成本

使用大窗口模型生成摘要会增加一次内部调用。它不违反主请求映射硬生效，但会降低单次降本幅度。需要分别统计摘要成本和主请求节省量，不能只看主请求 credits。

### 动态账号映射漂移

账号剩余额度参与映射时，两次 `execute_upstream` 可能选到不同账号和不同模型。一次精确重规划只能保证有界，不能保证第二次一定命中同一窗口。长期应采用 prepared-dispatch 两阶段结构解决。

### 摘要 fallback 不等价

抽取式 fallback 是可用性兜底，不是语义摘要的同质量替代。监控和验收必须区分两者；不能因最终请求返回 200 就认定 compact 质量达标。

## 落地顺序

1. [x] 增加 `auto_compact_on_overflow`，并在 generation / `count_tokens` 翻译前同步设置 `options.compact_mode`。
2. [x] 抽出统一 `CompactionDecision` 和 `CompactionArtifact`，按 `W_mapped` 启动自动 compact。
3. [x] 增加摘要 payload 容量预检、fallback reason 和相关日志；生产配置大窗口 `compaction_summary_model`。
4. [x] 捕获首次 `ExecuteError::ContextLimit`，复用 artifact 完成一次 `W_resolved` 精确重规划和额度重预留。
5. [x] 修正流式 resumed server events 与 compaction block 的顺序；OpenAI 和内部轮次保持硬错误。
6. [ ] 根据 fallback 数据决定是否实现分块滚动摘要。
7. [ ] 最后再评估 prepared-dispatch 重构和内部 Tool Search 的 pause/resume compact。

## 验证

```bash
cargo test -p kproxy-translate
cargo test -p kproxyd http::handlers::tests
cargo test -p kproxyd http::stream::tests
cargo test --workspace --all-features --locked
```

必须覆盖：

- 180k 输入映射到 128k 模型，自动 compact 后主请求仍使用映射模型并返回 200。
- 自动模式即使最终未触发 compact，system prompt 的位置也符合 `compact_mode` 设计。
- 摘要模型能容纳完整输入时只发生一次语义摘要调用。
- 摘要模型装不下时不发注定失败的请求，记录 `summary_capacity_insufficient` 并进入抽取式 fallback。
- `W_mapped` 足够但账号实际 `W_resolved` 更小时，精确重规划一次后成功。
- 已产生语义摘要后遇到更小 `W_resolved`，只重新应用 artifact，不发生第二次摘要调用。
- 第二次账号模型仍更小时返回 400，不出现循环。
- 单条 current message 或工具定义本身超限时不调用摘要模型。
- 流式响应的 compaction block 位于 resumed server events 及任何 text/tool block 之前；非流式 content 顺序一致。
- 下一轮回传 compaction block 后，上游只保留 checkpoint 之后的历史。
- `count_tokens.input_tokens` 为应用客户端边界/编辑后的真实值，`usage.input_tokens` 为服务端 compact 后主模型实际值，`original_input_tokens` 可解释压缩前差异。
- OpenAI 协议和已经开始输出的 Tool Search 中途超限不会静默删历史。

## 已验证的客户端前提

Claude Code 2.1.235 的 mock-server 实测结果仍有效：

- assistant 响应中的 `compaction` block 会在下一轮原样回传。
- 流式 start/delta/stop 生成的 compaction block 也会完整保留。
- 客户端不会按这个块裁掉自己的旧历史，裁剪必须由服务端 `apply_compaction_boundary` 完成。
- 客户端普通请求只携带 `clear_thinking_20251015`，不主动发送 `compact_20260112`。

客户端大版本升级后应重跑该兼容性验证。如果客户端开始丢弃未知块，服务端方案会退化为每轮重新摘要，届时应自动关闭 `auto_compact_on_overflow`，而不是继续产生隐藏成本。

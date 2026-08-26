# 上下文压缩质量提升方案

## 定位

本文只谈**压缩本身的质量**：摘要怎么生成、保留什么、丢弃什么。压缩何时被触发（模型映射导致的窗口不一致）见 `model-mapping-context-window.md`，两份文档互为前提：那份决定"什么时候压"，本文决定"压得好不好"。

## 实现状态（2026-08-26）

本文的语义摘要主方案已经落地；下文“现状”章节保留的是改造前问题记录。当前实现具有以下行为：

- compact 触发后，kproxy 会先用本地 tokenizer 把有效历史预处理成有界 checkpoint，再转换为一个**无 tools 的普通 Kiro 对话请求**。工具调用与工具结果会转成可读文本，当前用户轮以 JSON 字符串作为不可信源数据放入摘要 prompt；模型被要求只返回 `<summary>` checkpoint，不回答原任务。原始超长会话不会直接发送给摘要模型。
- 摘要 checkpoint 覆盖完整历史和当前用户轮。主请求同时尽量保留最近 `compaction_preserve_recent_turns` 轮结构化原文；payload 中注入的 checkpoint 与返回客户端的 compaction block 是同一份内容，下一轮应用 compaction boundary 后不会换成另一份字符碎片。
- 摘要调用独立走 `AccountPool`、额度预留、usage 与 stats，内部统计路径为 `/internal/compact`，不会混入主响应的顶层 token usage。
- 摘要调用超时、额度不足、上游失败、返回空/非法摘要或无法压到目标窗口时，会恢复原 payload 并使用 extractive fallback；日志中的 `compaction_mode` 可区分 `semantic` 与 `extractive_fallback`。超时立即结束主链路等待，已启动任务只在有界后台宽限期内继续结算；到期后取消摘要流，并保留已经解码的 usage、credits 与失败 stats。
- 已加入 `compaction_summary_model`、`compaction_summary_timeout_ms`、`compaction_preserve_recent_turns` 三个配置项及校验，均随现有配置热更新机制生效。
- `auto_compact_on_overflow` 默认开启；模型映射窗口溢出会在首次调用前触发，上游返回 `prompt is too long`/`context length exceeded` 时会按保守窗口重新压缩并只重试一次。摘要容量会在本地预处理后预检，账号实际窗口更小时只复用同一压缩产物重规划一次。
- 流式和非流式响应都先返回同一份 compaction checkpoint；流式 prelude 在首个成功语义事件前暂存，以保留 pre-data retry。摘要与主调用分别记账，Claude usage 可通过 `iterations` 观察摘要与累计主采样，内部续轮的 input tokens 不会只上报第一轮。

尚未实现的质量优化项是摘要内容哈希缓存和分块滚动摘要；自动触发的范围、限制及后续 prepared-dispatch 工作见 `model-mapping-context-window.md`。

## 现状：截断拼接，不是摘要

`kproxy-translate/src/tokenizer.rs` 的 `compact_kiro_payload` 分三步：

1. **从最老开始成对删除** history（user+assistant 一对；孤立的 assistant 也会被清掉，因为 Kiro 要求 history 以 user 轮开头），直到降到 `history_target = target - summary_budget` 以下。`current_message` 不动，所以当轮的 tools 与 system 侧内容完整保留（`compaction_removes_old_turns_but_preserves_current_tools` 测试覆盖了这点）。
2. **把删掉的内容渲染成"摘要"** 塞回 history 最前面。所谓摘要即 `compact_excerpt`：超长则取头 2/3 加 `" … [compressed] … "` 加尾 1/3。
3. 塞回后仍超 target 就撤掉摘要、`summary_char_budget` 乘 3/4 再来一遍，直到 `<= 256` 为止。

"保留最近若干轮"这一点通过删最老实现，是成立的。但**更早的历史不是被摘要，而是被截断成碎片**：`render_compaction_summary` 倒序遍历、`remaining` 递减，越老的轮次分到的字符预算越少，`remaining == 0` 直接 `break`，最老的部分连碎片都不留。全程不调用任何模型。

### 五处具体缺陷

**1. 返回给客户端的摘要与发给上游的摘要不是同一个东西（最严重，属现存 bug）。**

```rust
if compacted_tokens <= target_tokens || summary_char_budget <= 256 {
    compaction_summary = Some(render_compaction_summary(
        &original_history,          // ← 完整原始 history
        summary_char_budget,
    ));
```

塞进 payload 给上游的是 `render_compaction_summary(&removed, ...)`（仅含被删部分），而返回给调用方、最终成为客户端 compaction 块的是 `render_compaction_summary(&original_history, ...)` —— 整个原始历史，包含那些本该以完整形态保留的近期轮次。

结合已验证的客户端行为（见 `model-mapping-context-window.md` 的"已验证的前提"），后果是：

- **第一轮**上游收到"被删部分的碎片 + 近期轮次的完整内容"，质量尚可。
- **第二轮**客户端带回 compaction 块，`apply_compaction_boundary` 丢弃它之前的一切，上游于是收到"整个原始历史的碎片"——**近期轮次的完整内容被降级成碎片**。

上下文质量在第二轮不是持平而是断崖下降。且 `original_history` 远大于 `removed`，却复用同一个（可能已被收缩过的）`summary_char_budget`，压缩比更狠。无注释可证明这是设计意图，倾向判定为 bug。

**该客户端触发路径在改造前是休眠的。** 实测确认 Claude Code 普通请求发送的是 `clear_thinking_20251015`，不是 `compact_20260112`，所以不能依赖客户端 edit 触发。当前默认开启的 `auto_compact_on_overflow` 已按映射窗口和上游真实拒绝主动点亮新链路，缺陷 1 也已修复；本段保留为历史风险记录。

**2. char/token 换算对中文是错的。** `summary_char_budget = summary_token_budget * 3` 隐含 1 token ≈ 3 chars，这是英文比例。中文在 cl100k 下 1 汉字约 1~2 tokens，`* 3` 严重高估可容纳字符数，于是中文对话的摘要几乎必然超预算，直接落入收缩重试循环。

**3. 收缩循环开销不小。** 每次迭代都要 `render` 加 `splice` 加**全量** `estimate_kiro_payload`。`summary_char_budget` 从最大 24576 按 3/4 衰减到 256 约需 16 次迭代，即最坏 16 次全量估算；外层删除循环同样每删一对就全量估算一次。有 LRU 按文本 hash 缓存兜底，实际未必是 O(n²)，但中文场景大概率走满这条路径。

**4. 摘要被伪造成 user 轮。** `compaction_summary_pair` 把摘要塞成 user 消息，再配一句伪造的 assistant 回复 `"I will preserve and use the compacted conversation context above."`。模型看到的是"用户说了一大段摘要"，可能将其中内容当作用户指令而非背景。

**5. 可能"压了但没压到位"。** `summary_char_budget <= 256` 是兜底出口，走到这里时 `compacted_tokens` 仍可能大于 `target_tokens`。当前调用方会验证最终 token 是否达到目标；无法压缩的 current turn 或 tools 直接返回上下文错误，账号级重规划固定最多一次，不通过可配置重试掩盖未达标结果。

## 官方 compaction 对照

Anthropic 的服务端 compaction（`compact_20260112` 策略 + `compact-2026-01-12` beta 头）是**独立的采样调用**：检测到 input tokens 触及阈值（默认 150,000，最低 50,000）→ 调模型生成语义摘要 → 在 assistant 响应开头发出 `compaction` 块 → 用压缩后的上下文继续。默认 prompt 要求模型把摘要包在 `<summary></summary>` 内，并明确指示写下 state / next steps / learnings，"写给一个拿不到原始历史的读者"。

Agent SDK 的 `compaction_control` 是同一思路的客户端版：注入一个 user turn 要求摘要，拿到结果后整段替换历史，并允许 `model` 字段指定更便宜的模型专做摘要。cookbook 实测 5 个工单的工作流从 208,838 tokens 降到 86,446，减少 58.6%。

**根本差异**：官方的摘要是模型生成的语义压缩，保留决策、状态、下一步；本项目的 `compact_excerpt` 是字符截断，保留"开头 2/3 与结尾 1/3 的原文"。前者丢细节留结论，后者丢结论留碎片。这不是参数调优能弥补的差距。

参考链接：

- <https://platform.claude.com/docs/en/build-with-claude/compaction>
- <https://platform.claude.com/cookbook/tool-use-automatic-context-compaction>

### 可直接对齐的四处规范

**1. `pause_after_compaction` 的重建模式。** 官方在收到 `stop_reason: "compaction"` 后，用 `[compaction_block] + messages[-3:]` 重建消息列表——**最近若干条 verbatim 保留，只有更早的才被摘要替换**。本项目现状恰好相反（缺陷 1）。照此结构实现，缺陷 1 自然消失。

**2. prompt cache 的正确姿势。** 官方指引：在 system prompt 末尾放 `cache_control` breakpoint，使 compaction 不失效 system prompt 缓存，只有新摘要那段需要重写；compaction 块本身也可挂 `cache_control`。所以缓存击穿是可控的，前提是断点位置正确。

**3. 流式格式是确定的。** compaction 块发 `content_block_start` → **单个** `compaction_delta` 承载完整摘要（不增量流式）→ `content_block_stop`。本项目流式对齐应照此实现，不要自行发明。

**4. 成本口径有官方答案。** 官方把 compaction 的 token 单列在 `usage.iterations` 里（`{"type":"compaction",...}` 与 `{"type":"message",...}` 两条），顶层 `input_tokens` **不含** compaction 消耗。另：`count_tokens` 会应用已有 compaction 块但**从不触发新的**，并返回 `context_management.original_input_tokens` —— 本项目 `count_tokens` 已在返回该字段，方向正确。

## 双重压缩：两套机制叠加

Claude Code 的压缩是**客户端触发**的（`/compact` 命令，以及一个客户端侧的 auto-compact 窗口阈值），与 API 的服务端 compaction 是并列的两套方案——官方文档把服务端 compaction 定位为"避免 client-side summarization code"的替代品，而非 Claude Code 所使用的机制。

因此本服务若加入自己的压缩，就存在两套互不知情的压缩机制：

| | 触发方 | 阈值依据 | 压缩方式 |
| --- | --- | --- | --- |
| 客户端压缩 | Claude Code 自身 | 它认定的源模型窗口（社区逆向称约 76~84%，未经官方确认，版本相关） | 调模型做语义摘要 |
| 服务端压缩 | 本服务 | 映射后目标模型窗口 | 现状为字符截断 |

映射到小窗口模型时服务端阈值先到，按时间顺序发生：

1. **服务端压一次** —— 删旧轮次、生成摘要、注入 compaction 块，客户端存下该块。
2. **客户端历史继续增长** —— 已实测确认客户端不按 compaction 块裁剪自身历史，其 token 计数照旧上涨。
3. **客户端撞到自己的阈值再压一次** —— 而它此时要压的历史**已包含服务端注入的那份摘要**，等于对摘要再做摘要。

信息损失是复合的：第一次已把若干轮对话压成一段摘要，第二次把这段摘要连同新内容再压一遍，原始细节被过滤两遍。

两个附带问题：

- **用户的压缩偏好只对客户端生效。** 官方支持 `/compact <指令>` 与 CLAUDE.md 中的 compact instructions，二者均在客户端侧，服务端压缩读不到。用户配置"重点保留测试输出"时，服务端压缩无从知晓，可能正好丢弃用户最在意的内容。
- **无法通过改进服务端压缩算法消除。** 即使按下文提升方案把服务端压缩改为语义摘要，双重压缩依然存在——它是两套独立机制共存的结构性后果，不是质量问题。

可缓解的方向（均需权衡，尚未定）：让服务端压缩的目标更激进，压到远低于客户端阈值以延后其触发；或让服务端压缩读取项目 CLAUDE.md 的 compact instructions，使两套压缩的语义偏好一致。

## 提升方案

Kiro 上游没有 compaction 能力，官方那套服务端行为需由本项目自行实现。核心改动是**把 `compact_excerpt` 的截断换成一次真实的 Kiro 调用**，即 SDK cookbook 的做法。

**1. 摘要走模型生成。** 用被删除的历史加一个摘要 prompt，向 Kiro 发一次独立请求，取 `<summary>` 内容。prompt 采用官方结构：Task Overview / Current State / Important Discoveries / Next Steps / Context to Preserve。cookbook 提供了完整文本可参考。

**2. 用便宜模型做摘要。** 对应 SDK 的摘要模型配置。这对本项目尤其重要：摘要调用会占用账号额度与并发，需走 `AccountPool` 并计入 `meter`。

```toml
[context]
# 摘要生成使用的模型，留空则复用当轮的映射后模型
compaction_summary_model = ""
# 摘要生成的超时，超时后回落到截断降级路径
compaction_summary_timeout_ms = 30000
# 摘要之前保留的完整轮次数（对齐官方 messages[-3:] 的思路）
compaction_preserve_recent_turns = 3
```

**3. 近期轮次 verbatim 保留。** 按官方 `messages[-3:]` 思路，保留最近 N 轮完整内容，只摘要更早部分。N 由 `compaction_preserve_recent_turns` 控制。

**4. 保留截断作为降级路径。** 摘要调用失败、超时或额度不足时，回落到现有 `compact_excerpt` —— 有损但不中断请求。这一点比官方更必要，因为本项目多了一层上游依赖。降级发生时必须记日志，便于统计降级率。

**5. 摘要结果缓存。** 同一段历史的摘要在后续轮次可复用（官方亦称"重复应用已有 compaction 块不额外收费"），避免每次触发都重新调模型。缓存键可用被摘要历史的内容 hash。

### 一并修掉的两处

- **缺陷 2 的换算**：`summary_char_budget` 的推导不应假定固定 char/token 比。摘要生成改为模型调用后，该预算的作用从"字符截断长度"变为"要求模型输出的目标长度"，应直接以 token 为单位约束 `max_tokens`，绕开换算问题。
- **缺陷 4 的伪造 user 轮**：Kiro 不认 `compaction` 块类型，塞回 payload 时仍须借 user 轮承载，这层没得选。但摘要文本应明确标注其为系统生成的上下文而非用户发言，降低模型误判为用户指令的概率。

## 代价

摘要调用是真实成本：一次额外的 Kiro 请求、额度消耗、以及首次触发时的延迟增加。官方亦承认其 compaction "contributes to rate limits and billing"。便宜模型加摘要缓存能压住大部分开销。

这个代价换来的是长会话真正可用 —— 现有截断方案在长任务中基本等同于丢失上下文。

## 落地顺序

1. [x] **修缺陷 1** —— payload 与客户端使用同一个最终 checkpoint。
2. [x] **近期轮次 verbatim 保留** —— 按完整 turn 边界保留近期结构化内容。
3. [x] **摘要走模型生成** —— 包含独立调用、降级路径、配置和记账。
4. [ ] **摘要缓存** —— 优化项，可后置。
5. [x] **prompt cache 断点与流式格式对齐** —— 压缩后采用保守断点，compaction 块保持首块。

## 验证

```bash
cargo test -p kproxy-translate tokenizer
cargo test --workspace --all-features --locked
```

建议覆盖的用例：

- 返回给调用方的摘要与塞入 payload 的摘要，其覆盖范围一致（缺陷 1 的回归测试）。
- 连续两轮触发压缩，第二轮上游收到的近期轮次仍为完整内容而非碎片。
- 摘要调用失败或超时，回落截断路径且请求不中断，降级被记录。
- 中文长对话触发压缩，不因 char/token 换算落入收缩重试循环。
- 同一段历史重复触发压缩时命中摘要缓存，不重复调用模型。
- 摘要生成占用的额度与 token 被单独记账，不混入主请求计数。

## 已确认的记账口径

摘要请求独立走 `AccountPool`、额度 reservation、meter 与 `/internal/compact` stats，不与主请求顶层 usage 混算。Claude 响应的顶层 `input_tokens` / `output_tokens` 仍只表示主生成；发生语义摘要时，`usage.iterations` 分别报告摘要采样和主采样。摘要成功后主请求仍可能因额度不足失败，这是两个独立上游操作无法原子结算的已知代价。

# Tessivum 长会话压缩恢复与 Goal 状态一致性开发计划

> 状态：压缩恢复、Goal 增量一致性与 persistence-backed 事件分页已实施；分页、千轮同 session、磁盘重启恢复和二十万事件压力验收已完成。
> 计划日期：2026-09-21
> 源码基线：Windows PR #6 合并后的 `main`，`a182b4b40ccc939850683513b7dcc48c6052837f`。
> OMP 对照：`v18.1.17`，`3b3a6dc9bbd85102ce19d0b1c11bf6870915f6ec`。
> 范围：共享 Rust Host / Agent Runtime；Windows、Linux、macOS 使用同一修复，不增加 Windows 特例。

## 1. 目标与完成定义

让已有长会话在超过单次压缩边界后仍能通过有界处理恢复运行，而不只是避免新会话再次触发问题；另外定位并修复截图中 `get_goal({})` 的目标状态读取失败。

完成必须同时满足：

1. 历史条数或字符量超过单次摘要上限时，能选择预算内的合法区间，分批缩小模型输入。
2. 保留当前请求和必要的近期上下文，不拆散工具调用/结果，不删除原始持久日志。
3. 自动压力维护、模型上下文溢出恢复和手动压缩共用边界规划，不形成三套实现。
4. 已恢复的当前回合继续执行，不重新执行已完成工具，不通过不断追加“继续”尝试解除阻塞。
5. Goal 持久事件、会话归属、内存投影及重启回放保持一致；截图中具体错误的根因需有复现证据，不能用推测宣布已解决。
6. Browser 可观察恢复成功、失败和可操作的下一步；不能只让服务层单测通过。

关联文档：[总体开发计划](DEVELOPMENT_PLAN.md)、[运行时架构](ARCHITECTURE.md)、[兼容基线](COMPATIBILITY_BASELINE.md)、[Windows 验收记录](WINDOWS_CHECKPOINT_20260916.md)。

## 2. 已知事实、未知项与问题边界

### 2.1 用户提供的现象

截图依次出现：

- 任务清单显示 `5/5` 完成；随后 `get_goal({})` 返回 `goal "goal-…" was not found`。
- 本轮失败：`COMPACTION_INPUT_TOO_LARGE`，提示输入超过 codepoint bound。
- 用户发送“继续”后，连续返回 `COMPACTION_REGION_TOO_LARGE`。

这说明存在目标读取异常和压缩阻塞，但不证明二者有直接因果关系，也不证明业务文件或持久会话已经丢失。任务清单和 Goal 是不同状态来源，不能以清单完成替代 Goal 完成。

### 2.2 源码已确认的问题

| 位置（上述基线） | 当前行为 | 后果 |
|---|---|---|
| `src/compaction.rs:27–29`、`CompactionConfig` | 默认单次最多 512 条消息、65,536 个输入 Unicode 码点、16,384 个摘要码点 | 均为本地资源边界；码点不是 token，也不是 UTF-8 字节 |
| `src/agent_loop.rs:950–977` | 存在历史 `request/header` 且 surface 条数达到 `max_surface_messages` 后才触发压力压缩 | 不按字符量提前触发；新回合输入、工具批次可跨过条数上限 |
| `CompactionService::compact_automatic` | 跳过 seed 前缀后，把全部 live surface 选为一个区间 | 超限历史没有预算内分段恢复路径 |
| `CompactionService::plan_from_surface` | 先检查条数，再检查工具配对和序列化码点；超限即返回错误 | 摘要模型尚未调用，恢复就失败；条数报错可能遮蔽仍存在的字符超限 |
| `run_turn` | 先追加用户输入、技能/工作区上下文，再执行压力检查；压缩失败结束回合 | 反复“继续”没有缩小旧历史，并可能继续增大 surface |
| `src/host.rs:2994–3001` | Host 为压缩服务使用启动配置的 provider/model 与默认限制 | 不等于每个会话当前选择的模型；正常生成预算与摘要预算必须分别计算 |
| `CompactionService::execute`、`Session::append_if_surface` | 摘要与 replacement 采用追加式持久事件，替换前核对 surface | 已有事务、并发保护可复用，无需第二套会话存储 |

模型溢出路径已有一次恢复门槛 `context_overflow_recovered`，重试前也会重新构建 `request.messages`。本次应保留这些约束，修复区间选择和维护结果判断，而不是另加通用无限重试。

### 2.3 Goal 尚不能定案

- `GetGoalTool` 只接受空对象，从工具运行上下文按 session 路由；截图不是模型显式传错 `goal_id` 的证据。
- `GoalService::new` 从完整 `session.events()` 回放 `goal/change`；`current()` 也从事件读当前目标。
- `model_value()` 还依赖内存 `GoalState`、round counter 和 activation；不能把任何错误统一当作“没有目标”。
- 基线的 `sync_locked` 只向 `apply_goal_round` 交付增量事件，写入成功路径自行更新 state 并推进 `observed_events`；本次已验证并修复这一同步边界，但没有据此认定截图根因。
- 压缩修改模型 surface，不删除 `goal/change` 事件。不能预设“压缩丢了 Goal”，也不能仅凭截图将错误绑定到当前基线版本。

实施前收集：实际二进制版本/提交、session ID、相关 `goal/change` 与 Goal 来源 `user/message` 的事件序号和顺序、失败工具结果、最近压缩事件及 seed 信息。只导出必要范围并脱敏；不读取或公开无关会话、凭据和工具秘密输出。

当前尚未获得该截图会话的原始日志；本计划没有声称实机复现了 Goal 错误。

## 3. OMP 对照：借鉴机制，不照搬框架

### 3.1 参考来源

已阅读随附 `omp://compaction.md`，并对照固定版本的公开文档及预算/切点源码：

- [OMP 压缩机制文档](https://github.com/can1357/oh-my-pi/blob/3b3a6dc9bbd85102ce19d0b1c11bf6870915f6ec/docs/compaction.md)
- [OMP compaction.ts](https://github.com/can1357/oh-my-pi/blob/3b3a6dc9bbd85102ce19d0b1c11bf6870915f6ec/packages/agent/src/compaction/compaction.ts)：`calculateContextTokens`、`compactionContextTokens`、`resolveBudgetReserveTokens`、`resolveThresholdTokens`、`findCutPoint`。

文档描述的策略能力不代表本次 OMP 会话实际启用了全部策略，也不代表 Tessivum 已有这些能力。这里只借鉴设计，不引入 OMP 依赖。

### 3.2 机制与取舍

| OMP 机制 | 本次 Tessivum 决策 |
|---|---|
| 按模型窗口与 reserve 计算压力阈值；固定 token 阈值优先，保留下一请求/输出余量 | 采用“软触发水位与硬边界分开”；复用现有 context-window resolver。不得把摘要单批 512 条当作模型窗口 |
| 上下文占用参考 provider usage，同时以存储历史估算作下限，避免压缩后的 wire usage 掩盖历史增长 | 采用双视角；旧 usage 在模型切换、surface 替换后不能不加区分地复用 |
| 工具循环安全边界可进行 mid-turn 维护 | 采用；不能等整轮结束才压缩长工具循环 |
| `findCutPoint` 从最新消息向前按预算选择保留区，不能在 `toolResult` 处切开 | 采用近期窗口思想，同时以 Tessivum 已有 `validate_tool_pairs` 校验整个调用/结果组 |
| 压缩保留 summary 与近期原始消息；下一轮摘要更新纳入 previous summary | 采用；旧摘要参与下一段合并时也计入预算，不能反复丢失更早约束或无限堆叠摘要 |
| split-turn 时区分历史摘要和当前回合前缀上下文 | 采用语义约束，但不照搬双摘要调用；保护当前真实用户请求，仅压缩已完成工具组 |
| pruning / shake 优先减少旧工具结果，要求净收益；不足时前进到后续策略 | 复用现有 `prune_tool_result` 的持久 replacement，不增加策略注册器；必须保留来源和截断提示 |
| overflow 与普通 threshold 维护分开；溢出恢复不走重用原始超限输入的 handoff | 采用；不能把同一超限原文换个提示再发一次 |
| 持久压缩边界与展示 transcript 分离 | 采用现有 event log / surface 分离，Browser 历史展示不得伪装成用户删除了记录 |
| provider-native Responses 压缩、snapcompact 位图归档、多方法顺序、模型晋升 | 本次不移植：需要模型/协议能力，不能解决所有 provider 的公共边界，也不为此引入新依赖 |
| speculative 异步压缩、idle 维护、notes-backed context windows | 本次不做；先完成同步、有界、可重放的恢复路径 |

OMP 文档默认 `keepRecentTokens=20000`，reserve 通常至少 16,384 tokens 或窗口的 15%，小窗口另行处理。这些值不是 Tessivum 的直接默认值：Tessivum 当前摘要输入上限是 65,536 码点，单位、模型及序列化方式均不同。

重要区别：本计划提出的“对已超过单批资源上限的历史进行多段有界恢复”是针对 Tessivum 缺陷的方案，不宣称 OMP 对任意超大输入、任意策略都具有相同保证。

## 4. 固定不变量与非目标

### 4.1 必须保持

- 原始事件日志为事实来源；压缩只更新模型可见 surface。不得覆盖旧日志来让错误消失。
- 区间使用当前 surface 位置，来源使用 event seq；不能混用两者，成功替换后旧索引立即失效。
- 每个工具调用和其结果处于同侧；一个 assistant 消息内的并行调用及其结果视为不可拆的组。
- 当前真实用户请求、未完成工具调用及必要近期上下文受保护；`role=user` 的工具结果或插件摘要不等于真实用户请求。
- 压缩租约覆盖一次维护操作，避免自动/手动维护交错；计划必须基于同一个 surface 快照。
- 并发写入导致预期 surface 不一致时不提交旧摘要；取消/写盘失败不能改变未成功提交的区间。
- 一段成功、后段失败时，前段持久结果保留；下次从当前 surface 恢复，不能再按旧范围重放。
- 摘要视作历史材料，不提高为系统指令；不得借摘要改变权限、Goal 状态或用户授权。
- 不重复派发已完成工具；恢复只重建下一次模型请求。

### 4.2 不做

不提高或关闭硬上限来掩盖选区错误；不切换模型冒充修复；不增加向量库、第二套日志、后台压缩服务或 Windows 专属分支；不自动清空/重建 Goal，不从摘要猜出目标 ID。

## 5. 压缩恢复设计

### 5.1 预算分离

区分以下预算，避免不同单位相加：

1. **正常模型请求预算**：当前会话模型 context window，减去输出 reserve 和必要余量；估算覆盖 system、工具 schemas、消息、插件注入及媒体成本。
2. **摘要模型请求预算**：实际摘要模型的窗口，减去摘要 system、格式包装、输出 reserve；不能使用正常生成模型的窗口代替。
3. **本地单批硬边界**：保留 `max_surface_messages`、`max_input_codepoints`、`max_summary_codepoints`；每个摘要调用单独满足。

软水位低于对应硬边界。已知正常模型窗口时，仅按其扣除输出 reserve 后的 75% 触发压力维护；摘要单批消息数/码点上限不限制正常模型的整段输入。未知窗口时，才以单批消息数/码点上限的 75% 作为本地维护触发和恢复目标。此校准来自长历史及不可改写 seed 的 Browser 回归；每次摘要仍满足原单批硬边界，不抬高资源限制。连续检查发生在每次模型请求前，仍必须容忍一轮直接跨越单批上限。

复用已暴露的 usage 和 context-window resolver。`tokens_for(codepoints) = ceil(codepoints/4)` 只能作为历史粗估，不能视为中文、代码或图片的安全上界。需要模型 token 预算时优先使用已有适配器数据并加上尚未计入的新消息估算；缺少模型窗口时保留本地硬边界与 provider overflow 恢复，不伪造窗口大小。无需在本次先引入 tokenizer 依赖。

### 5.2 合法区间规划

在 `CompactionService` 内扩展已有规划逻辑，不建立多策略抽象：

1. 获取 surface 快照，标记 seed、已有摘要、当前真实用户请求、pending tool group 与近期保护区。
2. 从最旧的可压缩内容开始，在调用/结果完整边界上积累连续区间；累计消息数和实际序列化码点，在超预算前停止。
3. 近期保护窗口初值不超过可用历史预算的四分之一，向合法工具组边界对齐；当前请求和 pending tool group 属于硬保护，不能为了满足比例而裁掉。
4. 长单回合不能因为其开头的用户请求受保护就整个无法维护：保留该请求，允许压缩它之后、近期保护区之前的已完成连续工具组；不跨保护锚点合并成一个伪连续范围。
5. 旧摘要需要更新时与后续候选内容一起计入预算；每次合并至少消费新的旧历史，不允许只把同一摘要再次摘要。
6. 复用 `validate_tool_pairs` 做最终校验。只有规划成功后才复制所选消息并调用 LLM；避免先克隆全部历史再报超限。

本次保留自动路径不改写 seed 前缀的边界。若超大不可压缩 seed 或受保护的单个用户输入本身已经用尽窗口，应明确报告阻塞源，不丢弃 seed、不伪造成功。普通 live 历史超限必须走分段恢复，不能被归入此例外。

### 5.3 单条/单组过大

只有超大工具结果确实阻塞合法选区时，才应用确定性裁剪：

- 优先旧的、已完成工具结果；必要时对导致溢出的最新超大工具结果做显式有损呈现，不能默默突破近期保护约定。
- 复用 `prune_tool_result` 与来源序号，保留调用 ID、`is_error` 和原始持久事件；模型可见文本包含截断声明与原始来源序号。
- 检查完整 replacement 序列化大小，不能只算截断正文却忽略 JSON、提示及 wrapper。
- 原实现会把图片/嵌套块转成文本；首轮只对可安全处理的文本结果自动裁剪。其他结果应返回明确边界错误，不能假装保留了图片语义。
- 巨大用户输入、工具参数、不可分并行调用组不靠静默截断解决；无合法可缩小单元时，停止并告诉用户应缩小的输入类别。

原日志可回查不等于模型已有通用原始结果读取工具。本次不得伪造 OMP 的 `artifact://`/`history://` 接口；来源可通过现有历史导出定位，模型自行检索能力另行决策。

### 5.4 分段执行与进展判定

```text
组装下一请求所需上下文
  → 评估压力/溢出恢复需求
  → 在保护区外选择预算内合法段
  → 必要时先裁剪阻塞段的文本工具输出
  → 有界摘要并检查净缩减
  → 条件追加 replacement
  → 重新读取 surface、重新计算预算
  → 达到恢复目标：重建下一请求
  → 无可压缩区间/无净缩减/取消/持久化失败：明确终止
```

- 以单批预算分段，而不是 `compact_now` 递归调用自身；保留一个会话租约，内部复用规划/执行方法，避免重复获取租约变成 `Busy`。
- 净进展以实际序列化输入大小下降为必要条件；消息数不得增加。被压缩的原始单元必须前移，不重新摘要无新内容的同一区间。
- 输出虽小于 `max_summary_codepoints`，但如果不比选中输入小，也不能发布为成功 replacement。该检查应在 replacement 前完成。
- 循环迭代受初始可处理单元数量和每步净进展约束；不能因 LLM 返回等长摘要而无限循环，也不引入通用失败重试。
- 每段记录现有 `compaction/start`、`compaction/summary`、replacement、`compaction/end`；必须保留当前取消、失败、CAS stale 的行为契约。
- 后一段摘要需读当前 surface，计入已生成摘要的成本；不把多段原文重新拼接成一个超限的最终摘要请求。

### 5.5 Agent Loop 与手动入口

- 压力检查移到能够计入当前技能、工作区和运行时上下文的位置；复用实际请求组装结果，避免为了计数写两次 context 事件。
- 自动维护只在工具结果完成后的安全边界执行；pending tool group 不被压缩。
- 模型 overflow 与正常压力维护使用同一规划器，但沿用现有本轮一次 overflow 恢复门槛。没有实际缩减不能返回 `Compacted` 并启动无效重试。
- 成功后由当前 loop 继续下一请求；不新增一条“继续”用户消息，也不让维护层和 loop 同时启动回合。
- 现有 `compact_now` 使用有界规划；显式 `compact_region` 保持精确区间语义，过大时继续拒绝，不能暗中只压缩用户指定区间的一部分。
- Host 列出了 `compact` 命令，但基线 `command_execute_inner` 的本地执行分支没有处理它；源码调用搜索也未发现产品代码调用 `compact_now`。实施时追踪完整命令桥接，补齐或复用真实执行入口，不以菜单可见或预置压缩事件的 Browser 夹具代替验证。
- 手动入口与正在运行回合串行协调，复用现有取消/命令机制；未确认运行面之前，不把 `/compact` 作为当前已可用的恢复建议。

### 5.6 错误与 UI

保留低层 `COMPACTION_INPUT_TOO_LARGE`、`COMPACTION_REGION_TOO_LARGE`，它们继续描述显式非法区间，不通过吞错让测试变绿。自动路径正常可恢复历史不应再原样落到这两种终止错误。

不可恢复时给出具体类别：受保护输入过大、无完整工具组、摘要无缩减、模型请求失败、surface 已变化、写盘失败或用户取消。具体新增错误码在实现前对齐现有协议，禁止设计一套平行 error envelope。

当前 `compaction_failure` 转成 `LlmFailure` 时只保留 code/message 等字段，不能假设 `TessivumError.details` 自动显示在 Browser。优先使用现有错误文本和压缩事件说明实际值/上限、已处理段数及下一步；不得记录完整敏感输入。沿用现有 event/wire 格式，不为此重构 Web UI。

## 6. Goal 一致性定位与修复门槛

### 6.1 定位顺序

1. 获取脱敏会话副本，确认版本与 session 归属，从头回放；对比 `GoalService::new`、`current()`、`model_value()` 与工具调用错误路径。
2. 检查创建、编辑、完成、清除事件是否完整，以及 Goal 回合事件引用的 ID/revision 是否在当时合法。
3. 检查持久事件顺序与 `observed_events` 的推进，尤其非 Goal 事件穿插、并发追加、服务重建及 CAS 失败后的同步；验证增量折叠是否与冷启动折叠等价。
4. 对比压缩前后和重启后结果；压缩事件不得改变 Goal 的语义状态。检查 seed/import 是否在缺少创建历史时保留了后续引用。
5. 核对 Host 与 `GoalToolRouter` 是否使用同一 session-owned 服务；不能搜索其他会话中的同名目标来“修复”缺失。

### 6.2 按证据选择最小修复

| 确认结果 | 修复方向 |
|---|---|
| 持久事件合法，内存投影落后或游标错误 | 统一受影响事件的折叠语义与游标提交点；必要时复用同一个内部折叠函数供冷启动/增量同步使用 |
| 真正没有目标或合法 clear | 保持既有 `{"goal": null}` 语义，不恢复已清除目标 |
| Goal complete 但未 clear | 保持可读取 completed Goal，不能把完成当删除 |
| 外部写入或 seed/import 造成不完整事件链 | 在引入损坏的边界修复或拒绝；报告具体来源，不静默跳过非法事件 |
| 会话路由错配 | 修复 owning-session 绑定；保持跨会话隔离 |

修复前保留一个能触发相同可观察错误的最小复现，修复后同一复现通过。没有原会话日志时可以验证合成场景，但不得把合成场景成功写成截图问题已定位。压缩工作不依赖 Goal 根因已查清，两项分别提交、分别标明验收状态。

## 7. 实施拆分与文件范围

下表保留原实施拆分；A/B/C 和 D 的合成回归修复已落地，原截图取证与跨平台发布门槛仍独立保留。

| 顺序 | 工作 | 首选位置 | 交付门槛 |
|---|---|---|---|
| A | 固定长历史复现与预算/边界行为 | `tests/compaction.rs`、现有 Agent Loop 测试 | 能区分字符超限、消息跳跃超限、工具组不可拆 |
| B | 分段规划、超大文本结果处理、净进展与事务 | `src/compaction.rs` | 已超限 live 会话能缩减，失败不丢持久数据 |
| C | 请求前水位、overflow 重试及手动入口 | `src/agent_loop.rs`、`src/host.rs`，确有桥接需要时才改 API/Bridge | 当前回合继续且不重复工具；实际 Browser 可操作 |
| D | Goal 证据定位与最小修复 | `src/goal.rs`；仅归属问题才改 Host/Router | 原复现通过，冷/热回放一致、会话隔离不退化 |
| E | 跨平台定向回归和发行记录 | 既有 CI、相关验收/发行文档 | 最终同一提交三平台通过；发布结论与证据一致 |

只扩展已有服务和测试文件；不新增压缩框架、不改 Core、不创建 Windows 专属实现。测试按行为合并覆盖，不为每个内部字段、转发调用或默认常量新增测试。

## 8. 验收矩阵

以下为验收标准；本机实现和验证证据见第 10 节，不以合成场景替代 G1，也不以 macOS 结果替代三平台 CI。

| 编号 | 场景 | 必须观察到的结果 |
|---|---|---|
| C1 | 已有 live 历史大于 512 条，包括 511 条后一批直接跨界 | 每次摘要均在单批边界内，最终下一模型请求可执行；不再整段拒绝 |
| C2 | 条数不足 512，序列化码点超过 65,536；包含中文与转义字符 | 字符预算独立触发，按实际序列化值分段，不混用 token/字节 |
| C3 | 单个文本工具结果过大 | 显式有损 replacement、保留原文和来源，之后可摘要并恢复 |
| C4 | 并行工具结果乱序到达、候选切点落在调用/结果中间 | 区间在所有配对完成边界对齐；pending 不压缩，无孤立结果发送给模型 |
| C5 | 单个长回合与当前用户请求夹在历史中 | 保留当前请求和近期必需上下文；已完成旧工具组仍可压缩 |
| C6 | 第二次压缩：旧摘要 A + 未压缩 B + 新增 C | A 的约束不凭空消失；B/C 各处理一次，无重复展开或遗漏 |
| C7 | 摘要等长、变长、异常终止或空摘要 | 不提交无效 replacement；有限步骤退出，不能形成无效压缩循环 |
| C8 | 取消、持久化失败、摘要期间 surface 改变 | 未提交段保持不变，已成功段可重放；无跨会话影响 |
| C9 | 已超限会话重启后恢复；存在 seed 或先前部分成功段 | 按当前 surface 重新规划；不可压缩 seed 明确报阻塞，不误删旧事件 |
| C10 | 主模型与摘要模型窗口不同、小窗口、未知窗口、模型切换 | 分别计算预算，不复用失效 usage，不产生负预算或假成功 |
| C11 | 实际 overflow 与普通 pressure 两条入口 | 恢复后的请求内容变小且保留当前任务；已完成工具只执行一次 |
| C12 | Browser 手动压缩、失败后恢复、查看历史 | 走真实 Host/服务，状态与事件可见；旧 transcript 保留，不靠 seeded 假事件验收 |
| G1 | 原会话 Goal 失败复现 | 相同读取路径恢复正确；必须记录实际根因与版本 |
| G2 | create/edit/pause/resume/complete/clear、重启与增量同步 | 合法状态和 revision 一致；complete 可读，clear 后为空 |
| G3 | Goal 事件穿插普通事件、压缩前后与写入失败 | 冷启动与增量投影一致，不跳读/重复消费 Goal 回合 |
| G4 | 两会话、损坏或缺失事件链 | 不跨会话借用目标，不隐藏损坏，不根据摘要重建授权/状态 |

验证策略：先在已有测试套件保留少量能击中真实缺陷的回归，再运行实际 Host/Browser 的长会话恢复。最终 Windows、Linux、macOS 使用同一候选提交；使用隔离数据目录，禁止拿用户原会话做破坏性试验。报告分别标注合成复现、真实会话复现、Browser 场景与 CI，不相互替代。

## 9. 当前行动与发布顺序

- [x] 核对 Windows 合并基线，阅读现有压缩/Goal 路径与 OMP 固定版本参考，完成本文设计。
- [x] 完成压缩恢复 A/B/C，包括实际 Host 手动入口及 Browser 操作。
- [ ] 获得脱敏日志并完成 Goal 定位 D；缺日志时明确保留 G1 未验收状态，不阻塞可独立完成的压缩修复。
- [x] 完成本机适用回归和实际 Host/Browser 恢复场景，更新实施与验收记录。
- [ ] 最终提交通过跨平台 CI，再决定新版本打包与发布；不替换历史 Alpha.29 资产。

本次实现不升级版本、不替换历史 Alpha.29 资产、不修改用户原会话。后续通过独立修复分支和 PR 推进同候选 CI，不自动合并或发布；既有 Windows 验收结论不作为本次候选的验证证据。

## 10. 本机实施与验收记录（2026-09-22）

### 10.1 已实现边界

- `CompactionService` 以当前 surface 重规划有界批次，统一 manual、pressure、overflow 三个入口；显式 `compact_region` 仍严格执行请求区间。摘要单批的 512 条、65,536 输入码点、16,384 输出码点硬限制不变。
- 保护 seed、当前真实用户请求、当前工作区/运行时上下文、近期窗口和 pending tool group；完整并行工具组不能拆开。近期窗口不因纳入一条巨大的旧消息而吞掉所有可恢复历史。
- 只对完整文本工具结果做带来源提示的确定性裁剪，核对整个 replacement 的序列化大小；原始日志不改写。每批必须净缩减，已有摘要参与后续合并但不能单独反复摘要。
- 主请求和摘要请求分别使用实际路由的窗口及输出余量。使用序列化 UTF-8 字节与媒体余量作保守 token 代理，不宣称是 tokenizer；不在模型、system/tools 或 surface 已变化后复用旧 usage。
- Agent Loop 在完整请求组装和工具结果安全边界上维护；overflow 只重建并重试下一请求，不重新派发已经完成的工具。
- `/compact` 通过现有 Host 命令生命周期执行。每会话 execution gate 同时约束 driver 和手动维护；已有活动回合返回 `SESSION_BUSY`，后来的 prompt 等待维护结束。取消及 shutdown 能取消 idle 状态下的手动摘要，并完成 `command/done`，不等到全局 teardown 才释放 admission。
- Goal 冷回放与热同步复用合法事件折叠；所有读取显式传播损坏错误，写入在持久追加锁内重新校验 revision，避免普通事件插入后两个视图同时通过 CAS。外部事件只同步持久状态，不重新武装已 disarm 的热视图；冷启动原有 rearm 策略不变。

### 10.2 行为证据

| 验收项 | 本机证据 |
|---|---|
| C1/C2/C6/C9 | `oversized_live_history_recovers_after_restart_without_replaying_originals`：515 条旧消息与不足 512 条但超码点的中文/转义历史，冷恢复后分批缩减；第二轮摘要包含前轮约束，原始历史不会重复输入摘要；另有 seed 和部分成功后重启回归。 |
| C3/C4/C5 | 巨型中文文本工具输出只更新模型呈现；完整并行调用/乱序结果配对、长单回合当前请求与 pending call 保持；原文仍在日志中。 |
| C7/C8 | 无净收益、摘要请求失败、取消发生在 summary/replacement 写入、surface CAS 冲突均保留未提交历史；注入 replacement 持久追加失败后，冷恢复仍得到原 surface，随后可重新压缩。Host 覆盖活动回合拒绝、摘要期间排队 prompt、cancel/shutdown 与单次命令终结。 |
| C10/C11 | 主/摘要窗口不同、极小及未知窗口、切换请求模型；pressure 和受控适配器返回 overflow 的错误入口重建请求后继续，已完成工具只有一次 call/result，不是外部供应商窗口实测。 |
| G2/G3/G4 | create/edit/pause/resume/complete/clear、普通事件交错与并发 CAS、另一视图完成后的 `get_goal({})`、压缩前后和 Host 重启仍可读 completed Goal、跨会话隔离、损坏链的冷/热读取拒绝、显式 disarm 不被外部 resume 撤销。 |

### 10.3 实际 Browser 场景（C12）

使用独立临时数据目录启动实际 `tessivum web`，导入 520 条普通历史消息（1,560 个合法持久事件），没有预置任何压缩事件。模型为现有 recorded provider，不访问真实供应商；真实 Host、会话持久化、WebSocket 和 Browser 命令链路未替换。

从命令菜单调用 `compact`：先用非正常结束的摘要流得到可见的 `INVALID_COMPACTION_SUMMARY`；随后再次执行，实际提交三个摘要批次，模型 surface 从 520 条缩到 34 条，Browser 显示 `Conversation history compacted.` 与三个压缩记录。继续一次手动操作也在不刷新页面的情况下显示完成。原有 1,560 事件的文件前缀字节完全不变；Browser 的 Session export 返回 HTTP 200 / ZIP，包含原始历史标记和生成摘要。验证后关闭临时 Host 和 Browser，不操作用户原会话。

### 10.4 未关闭的取证与发布门槛

- **G1 未验收**：缺少截图会话的脱敏原始日志和准确二进制版本。本机按截图 Goal UUID 搜索未找到对应会话；已复现并修复的是独立合成的增量投影/CAS 缺陷，不能据此认定截图根因。
- **E 待同候选 CI**：本地验收完成后，通过 `fix/context-recovery` 独立分支和 PR 固定候选。现有 `ci.yml` 在 Ubuntu 和 Windows 运行 Rust 全目标检查，在 macOS 运行 Browser；本机结果只代表 macOS ARM64。CI 结论以该 PR 同一 head SHA 的检查和验收评论为准，三平台通过前不关闭此门槛，不复用旧 Windows/Alpha.29 报告。
- 本次未修改 Core、前端源代码、安装器、版本号或平台分支；不增加压缩策略框架或新依赖。

### 10.5 最终本地检查

以下命令在本次 macOS ARM64 工作树通过：

```text
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked -- --test-threads=4
python3 scripts/check_compat_baseline.py
python3 scripts/check_plugin_verification.py
python3 scripts/check_release_facts.py
```

Rust 输出合计 598 passed，0 failed；兼容基线检查仍为 RPC 52/52、Remote 24、Host events 11、Node kinds 26、Web source graph 38。此处没有声称运行了完整前端 E2E 或远端 CI；Browser 证据仅指第 10.3 节的实际操作场景。

### 10.6 同候选 CI 暴露的预算混用与修正

- 初始候选 `d0b886f8ae5dba21f79429462f5863999e531e30` 的 [CI 35683564581](https://github.com/wavetao2010/tessivum/actions/runs/35683564581)：Ubuntu `verify`、Windows `windows` 通过，macOS `browser-e2e` 失败；不能把总体结果写成通过。
- 失败集中在 `chat-long-interactions`、`chat-scroll-contract`、`trajectory-virtualization`。原实现把摘要单批的 75% 本地水位无条件应用于正常请求；即使正常模型容量足够，也会消费仅为普通回复准备的 replay，或阻塞不可改写的分支 seed。
- 修正已知窗口的压力判定，不改摘要单批硬上限；未知窗口仍保留本地维护水位。新增回归覆盖超过 512 条且超过 65,536 码点的普通历史和分支 seed：正常模型可容纳时不调用摘要、不改变历史。该回归修复前失败、修复后通过。
- Browser 测试的 recorded 模型默认窗口由 128,000 校准为 256,000，给保守 UTF-8 字节估算下的长历史及请求开销留出空间；显式测试环境变量可覆盖该默认值。只改变测试模型设置，不提高产品或摘要服务硬上限，不缩短长历史 fixture，不删除 Browser 断言。
- 本机 `compaction` 与 `agent_loop` 专项共 53 项通过；上述三个 Browser 文件共 7 个实际 Host/Chromium 场景通过，包括分支继续、流式滚动及 Trajectory 虚拟化。新候选仍须重新取得三个平台的同 head CI 结果，不继承初始候选的 Ubuntu/Windows 结论。
- 修正后的本机完整检查：`cargo fmt --all`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked -- --test-threads=4` 通过，Rust 共 599 passed；兼容基线、插件台账、发布事实三项脚本通过。未用这些结果替代新候选远端 CI。
- 第二候选 `a54c529bda1be885ee0aabc1cbfb69e063069680` 的 [CI 35685949402](https://github.com/wavetao2010/tessivum/actions/runs/35685949402) 中，原失败的 7 个长历史 Browser 场景全部通过；Browser 仅余 `steering` 的 FIFO flush 用例在队列展开前超时。该夹具原先未等待初始请求进入工具等待态，队列操作可与初始领取竞争。四个 steering 场景统一等待已经截获的真实 `question/requested` 事件后再操作，仍在手势后才释放问题 UI；不改产品逻辑、不增加重试、不放宽 FIFO/持久化断言。本机四场景各运行三遍，共 12 passed。

## 11. 长期会话优化计划与验收记录

本节记录 bounded recovery 修复之上的长期会话优化、已交付实现与本机验收证据。

### 11.1 目标与边界

长期会话保持同一个 session 身份，并通过滚动的模型可见 surface 持续对话。分离三类预算：

- **模型上下文预算**：system、tools、surface、当前输入和 pending tool group 组成的实际请求窗口；
- **单次 compaction 预算**：一次维护允许执行的摘要批次、摘要输入/输出大小和耗时；
- **运行时存储预算**：完整事件日志、surface projection、近期上下文在进程内的驻留范围。

Compaction 只替换模型可见的旧 surface，不删除原始事件日志。长期会话不依赖强制新建 session；原始日志的归档或分页属于独立的存储运行时工作。

### 11.2 对齐 Harness 的增量压缩策略

参考 DeepSeek Harness 的 `agent/pre-step` 压力检查和 `BasicCompactionEngine`：

- 按实际 provider/model 的上下文窗口提前检查压力，默认目标为窗口约 80% 的压力阈值；
- 保留近期 surface，优先选择最老且已完成的安全范围；
- 保持 tool call/result 配对，不拆分 pending tool group、当前输入和必要 seed；
- 先对可安全处理的巨大文本 tool result 做 model-free pruning，再决定是否调用摘要模型；
- 正常压力路径一次只做一个 summary replacement，完成后重建请求，不在一个请求内追赶整个历史；
- replacement 必须实际缩小模型可见输入，并继续合并已有 checkpoint，不反复摘要同一范围。

现有 `d0b886f`、`a54c529`、`2dd9ec5` 的窗口选择、分批边界、pruning、CAS 和工具重试保护继续保留。本计划不替换现有 session 事件格式。

### 11.3 限制超限恢复的总资源消耗

将“每批有界”扩展为“每次调用有界”：

- 普通 pressure compaction 最多执行一个 recovery batch；
- provider-confirmed overflow 最多执行一次 pruning 和一次最大安全 summary recovery，并最多重试模型一次；
- 只有 `surface.replaceGeneration` 确实前进时才允许重试；
- 超过批次、摘要调用或取消预算时，返回明确的 `SESSION_CONTEXT_TOO_LARGE` 类错误，不继续无限压缩；
- 巨大不可拆分的用户输入、tool 参数、system/tools envelope 或 pending tool group 不通过静默截断解决；
- 已完成的工具调用不得因 overflow recovery 被重新派发。

目标是防止一次请求内部出现数十个连续 compaction batch，同时保留正常长期对话的增量维护路径。

### 11.4 降低超大日志的运行时内存占用

第一步只做低风险优化：避免每个请求重复复制完整 `events`、`surface` 和 derived messages；compaction 只读取当前 surface 与必要来源事件；避免每个 batch 重新扫描完整事件数组；summary request 不携带未选中的完整历史。

后续如长期 session 仍随原始日志线性增加内存，再将完整 event log 改为 persistence-backed 分页读取；进程内只保留 header、surface projection、近期 tail、来源索引和当前 turn 状态。该阶段不与 compaction 边界修复混合实施。

### 11.5 长期会话验收计划

新增行为验收，而不是仅验证内部字段：

- 连续数百至一千个 turn 使用同一个 session，自动压缩后仍可继续请求；
- 正常 pressure 请求最多产生一个 summary batch；
- provider overflow 只在 surface 实际前进后重试一次，已完成工具不重复执行；
- 515/520 条历史、中文/转义文本、并行工具组和重启恢复继续通过；
- 二十万级合成事件日志不会在一次请求内执行无界 recovery loop，超过边界时返回明确错误；
- 取消、shutdown、summary 失败、replacement CAS 冲突不破坏原始 surface；
- 内存优化阶段验证运行时驻留主要随 live surface/recent tail 增长，而不是随完整历史复制增长。

本节完成前，不应在 `CHANGELOG.md` 中宣称长期会话优化已经交付；实现后再补充同一候选版本的代码、测试和资源使用证据。

### 11.6 第一阶段低风险内存优化记录（2026-09-24）

- `Session` 保留兼容快照 API，同时提供 `read_events`、`fold_events`、`find_latest_event` 和 `SessionEventReader`，持久化恢复只驻留近期 tail 与 projection 状态。
- Agent Loop、Goal、Compaction、Permission、Projection、Planning、Host/Subagent 等高频路径已迁移到有界读取或增量 fold；显式导出仍按请求物化完整历史。
- JSONL/SQLite reader 支持分页恢复；冷恢复使用 8192 条扫描页避免逐 256 条重复扫描，普通随机读取仍限制为 256 条页。
- 验收：`cargo test --locked --test persistence_jsonl --test agent_loop --test compaction` 为 66 passed；二十万事件 ignored 压力测试为 1 passed，resident history 全程 256，采样 allocator RSS 均为 35,340,288 bytes。

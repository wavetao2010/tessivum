# Tessivum 已批准范围实施清单

> 原始清单日期：2026-10-10；证据对账更新：2026-10-13。范围沿用已确认决策，不重新征求范围批准、不引入更新版上游功能。
> 依据：[范围决策记录](SCOPE_DECISION_DRAFT.md)中已确认的 G01—G16、D01—D08（D06 分五项）、X01—X05。原文“未授权”等表述保留其历史含义，不撤销后续逐项执行授权，也不自动授权数据迁移、真实外部调用或发布。
> 从 `tessivum-context-recovery/docs/` 恢复至主仓库；原件不变，后续维护以本文件为准。本文记录目标、资产、差集、依赖和验收，不以测试总数代替范围结案。

## 1. 如何使用

- **已定位**：找到实现或场景入口，完整契约仍需核对；不是已完成。
- **部分覆盖／明确差集**：范围调查或本轮源码核对已指出具体差异；只对列明的差异作结论，不推断整个模块缺失。
- **验收缺口**：缺同候选、对应平台或真实供应商的运行证据；不能据此要求重写已有实现。
- 每项分别记录范围、批次、实现、验收四维状态，沿用范围决策第 6 节。第 3—9 节保留原调查与历史执行记录；当前新增证据以第 2 节为准。未被新证据明确覆盖的验收条件继续待执行。
- G 是基础能力，D 是这些能力的已批准细化或差异，X 是独立产品轨道；重叠项交叉引用，不重复计算工作量或完成率。
- 每个实施单元先复读目标候选及调用链，再修实际差集。达到替换门槛后删除被替代的重复执行路径，但不能因此删除 D05 的新旧 JS 插件扩展能力。
- 本文的验收列是执行入口；对应范围草案 Q01—Q10 中的详细场景仍全部有效，不因这里压缩表述而缩小范围。

## 2. 固定事实与证据优先级

### 2.1 当前候选与已取得证据（2026-10-13）

| 对象／单元 | 固定身份与实际证据 | 可以关闭的边界／仍未关闭的范围 |
|---|---|---|
| 冻结上游 | Harness `0.1.0-rc.5` / `47f943859bef60e4160492346772ded9b24f765a` | 既有 G/D/X 范围不变；不追加较新上游功能 |
| Core 发行基线 | [v0.2.0](https://github.com/wavetao2010/tessivum-core/releases/tag/v0.2.0) / `6cc4cfd40d6c69c55fd95917dda9fa8a0d4678b9`；Rust/Node/WASM v1 wire 不变 | 发行标签未改；后续 Native 配置修复提交为 `b69d0ed86f30adb0b2d6138d458593487336aa08`，不是新发行 |
| 当前产品源码 | 基于 `306ecf3f393f44859ab05ae50a6e1214b78ee7c2`，包版本仍为 `0.1.0-alpha.30`，Cargo/lock 与五处 CI/打包 checkout 固定 Core `b69d0ed86f30adb0b2d6138d458593487336aa08`；保留 PR #7/#8 | 不再依赖临时 path override；原调查 recovery 工作副本仍不假定已全部合入 |
| 原调查工作副本 | `tessivum-context-recovery` 的 `e12cada3b84464b7aef0be1c4247e096c56fac29`；Alpha.31 / Core 0.1.8 `eb1bc9fd320f880f0939689adf9af1b666a4b682` | 单独保留候选与证据；Alpha 数字较大不代表包含当前主线的 Core 升级，不回退主线 pin，不据局部源码相同推定整棵树等价 |
| C1 目录差分 | [当前 Core 报告](../../tessivum-core/docs/CONFORMANCE_REPORT.md)及 release 资产 `conformance-0.2.0.json`：lifecycle 12、service 9、严格注册 3、event 11、Loader 10、依赖生命周期 5；六份夹具均执行真实 Rust/Bun 并逐项比较轨迹 | 关闭这些**已列举目录用例**的对照缺口；旧文“TS 只有三个场景／Loader unsupported”已过时，不能继续按缺实现处理；不能外推为全部 live 配置求值/HMR 语义等价 |
| C2 与 D07 | [Native 运行时](../../tessivum-core/crates/tessivum-core/src/native.rs)、[Extism 运行时](../../tessivum-core/crates/tessivum-extism/src/lib.rs)是已有资产；本次对账不重新执行 Guest 验收 | 不能把目录差分或产品 CI 当真实 Guest Host Functions、全部资源限制共同矩阵，更不能据此关闭 TS/JS、Go、Python |
| 历史消费方同候选验证 | 产品 `306ecf3` / Core `6cc4cfd`：macOS ARM64 601 项 Rust 测试、22 项过滤、严格 Clippy；实际 CLI 初次运行与续接，事件 24→46、日志前缀保留；[产品 CI](https://github.com/wavetao2010/tessivum/actions/runs/38342882283) 的 verify、windows、browser-e2e 全通过 | 只关闭该旧候选的既有集成门槛；不把现有 Browser suite 通过当 D01—D08 全契约已实现，不把源码 CI 当发行归档/安装验收 |
| 证据位置 | `test-artifacts/alpha30-core020-integration/` 保留命令日志、结果、manifest/lock 与 remote-ci.json；旧 Alpha.29 的 574 项结果分别留在原探针目录 | 本地证据未随 Git 提交；公共证据由以上固定 release/CI 链接提供。禁止将旧 574 项结果冒充 Alpha.30 的验收 |

**C1/G03 本轮代码修复（2026-10-13，未发布）：** Loader 不再先消耗 Native 的服务配置表达式，而是经 `instantiate_configured` 交给 Native 保留源配置。Native 在所声明的 JSON-valued 服务依赖就绪后求值；provider 替换/恢复重新读取当前投影，移除则撤销 consumer effects。求值、schema 或更新失败保留已提交实例与服务；持久树仍保存源表达式。同步修正 activation 返回旧快照状态的竞态，并保留更新候选自身新增的依赖声明。

**本机证据：** 先运行新增回归，复现缺 provider 时在 import 阶段错误退出；修复后 Core workspace/all-targets 128 项测试、严格 Clippy 通过，activation 竞态回归重复 100 次通过。产品现有 Composition 8 项与 Agent Loop 29 项测试通过；实际 `CompositionRegistry` define→validate→run→替换→移除→恢复→stop 探针通过。产品现有 admission 仍在首次缺依赖时返回 `COMPOSITION_START_FAILED`，补服务后显式重试；不把它写成产品已接纳 Pending。冻结上游真实 Loader 探针与产品探针的配置/撤销观测均为 `null → first → second → null → restored → null`，且双方保留源表达式。这是该场景的对照，不是整个配置契约通过。

**上一步临时联调证据与边界：** `test-artifacts/c1-live-config/` 保留 Core 基于 `6cc4cfd` 时的本地补丁、产品基于 `306ecf3` 时的临时 Cargo path override 运行输出、实际轨迹及源码 SHA-256；该历史结果不改写。修复现已提交并推送为 `b69d0ed86f30adb0b2d6138d458593487336aa08`，产品 manifest/lock 与 Host checkout 已切换到该 Git 修订。该单元只支持已声明依赖的显式 JSON 服务投影；不隐式序列化任意 typed service、不执行 JS，也不把 WASM/Legacy 动态配置、所有 HMR/文件事务或完整 C1/G03 标为完成。

**正式 Git 依赖复验：** Core `b69d0ed` 固定后，Cargo metadata 确认 Core／Extism／Node Bridge 均解析到该 Git 修订，没有本地 Core path override。macOS ARM64 全部 601 项 Rust 测试（22 项过滤）、严格 Clippy、兼容基线及发行事实检查通过。实际产品 Composition 的缺依赖 admission、配置重求值、移除／恢复、源表达式保留均通过，轨迹仍为 `null → first → second → null → restored → null`；CLI 初次与续接分别取得 1／2 个成功的实际 bash 工具结果，日志记录 25→47 且旧前缀不变。独立证据保存在 `test-artifacts/c1-live-config-git-pin/`；[Core CI](https://github.com/wavetao2010/tessivum-core/actions/runs/38366582275) 通过，不外推产品新候选 Windows／Browser 或新发行验收。

**产品资产保留门槛：** recovery 中的 [Legacy 实际插件回归](../../tessivum-context-recovery/tests/community_plugins.rs)包含 `legacy_plugin_system_prompt_contribution_reaches_native_runtime_and_reaps`，实际加载插件、验证 Native/Legacy prompt 顺序及卸载撤销；当前主线 `tests/community_plugins.rs` 没有该用例。主线已有 `systemPrompt@1` 的注册路由，不应因此重写桥接实现；但实现存在也不能代替这份回归。在进入相应产品单元时迁移并以 Core 0.2.0 或后续固定候选复验。原工作副本与原测试保留未改，本轮未执行它，不把此次有限对账称为工作副本的完整合并。

### 2.2 原始调查快照（2026-10-10，非当前版本声明）

以下表格与后文保留原调查事实；其中相对源码链接是定位入口，不保证当前源码仍与当时快照相同。Core 目录缺口、候选版本及 CI 结论按 §2.1 更新，不机械重做已关闭用例。

| 对象 | 本轮核对结果 | 实施前处理 |
|---|---|---|
| 冻结上游 | Harness `0.1.0-rc.5` / `47f943859bef60e4160492346772ded9b24f765a`；行为以其 vendored Cordis 与本地补丁为准 | 不跟进较新上游功能；独立 Cordis 仅辅助比较 |
| 当前产品声明 | [Cargo.toml](../Cargo.toml) 为 `0.1.0-alpha.31`；Core、Extism、Node Bridge 均为 `0.1.8` / `eb1bc9fd320f880f0939689adf9af1b666a4b682`；[Cargo.lock](../Cargo.lock) 一致 | 不存在 manifest/lock pin 不一致证据。实施时固定产品候选与 Core 候选；本地相邻 Core 工作树不自动等于产品锁定源码 |
| Core 差分 | [历史报告](../../tessivum-core/docs/CONFORMANCE_REPORT.md)记录 Rust lifecycle/service/event/loader 的 12/9/11/10 个 case；[TS oracle](../../tessivum-core/oracle/src/scenarios.ts)实际只有 effect-dispose、isolate-realm、emit 三类场景，Loader 返回 `UNSUPPORTED_SCENARIO` | 补真正的上游对照，不能把 Rust case 数量或 Loader execution-only PASS 写成完整差分通过；oracle 还有本机绝对路径，需使冻结输入可移植 |
| 长会话与 Goal | [恢复计划](CONTEXT_RECOVERY_DEVELOPMENT_PLAN.md)第 10—11 节有分段压缩、Goal CAS、分页恢复与低风险内存优化的历史实现/证据 | 不重做这些能力。原截图 Goal 个案缺脱敏日志和准确二进制版本，仍不能认定根因；同候选跨平台证据单列 |
| Windows | [报告](WINDOWS_CHECKPOINT_20260916.md)后半记载注册表 PATH、进程树等修复；最后记录 `e0b81a5d3ccfb004364fc95f0fe74b7da8365482` 三平台 hosted CI 成功，并有更早 ZIP/安装器行为证据 | 报告开头的旧阻塞已被后文推进，不能重新标为缺实现；这些仍不自动覆盖当前 Alpha.31/Core 0.1.8、当前归档或正式发行。安装器“发行未关闭”不等于“没有安装器” |
| 性能 | [30 样本报告](PHASE9_BENCHMARK_REPORT.md)同时保留 Legacy readiness 与 Core loader_update 回归 | 只作固定 workload/环境的历史及回归基线，不移植为新版本 SLO |

实施证据必须携带产品/Core/upstream revision、未提交补丁标识（若有）、配置、运行时版本、平台、命令、产物摘要与结果。日期或相同版本字符串不能替代候选身份。旧报告内按时间追加的修复和最终结果优先于开头的起点描述；无法确定是否包含在目标候选时标“待核对”，不猜测。

## 3. 第一阶段：独立 Core

Core 只拥有 Context、服务、生命周期、事件、配置/Loader 与插件运行时；不引入 Agent、Session、Tool 等产品领域类型。Harness 到 Core 的依赖保持单向。

| 范围 | 目标契约 | 现有实现／证据 | 实际缺口与实施动作 | 依赖／建议批次 | 验收条件（待执行） |
|---|---|---|---|---|---|
| G01 | Context、required/optional 依赖、晚到服务、替换/撤销、isolate/intercept、旧 handle 失效 | [Core API](../../tessivum-core/docs/API.md)、[services 场景](../../tessivum-core/crates/tessivum-core/tests/services.rs)及 race/realm 场景已定位 | 实现已定位；TS 对照不覆盖全部声明 case。逐行为补对照并仅修实测差异，不复制 JS Proxy | B0 → C1 | consumer-first 等待后激活；optional 缺失不阻塞；替换/撤销使旧 handle 不可用；兄弟隔离与显式共享、intercept 优先级可观察 |
| G02 | 失败回滚、逆序/重入释放、卸载静默；事件顺序、短路、错误传播及监听器释放 | [lifecycle](../../tessivum-core/crates/tessivum-core/tests/lifecycle.rs)、[events](../../tessivum-core/crates/tessivum-core/tests/events.rs)已有 Rust 场景 | 验收缺口：TS 的简单 effect-dispose/emit 不证明异步释放及各事件模式。保留 Rust 并发不变量检查，同时补可比的上游行为证据 | B0 → C1，与 G01 同一 Core 候选 | 父释放等待子/异步清理；失败无残留、重复/重入释放收敛；emit/parallel/serial/bail/waterfall 的顺序/短路/异常及撤监听竞态分别通过 |
| G03 | Loader 分层 patch、依赖激活、配置求值、候选验证、失败回滚、持久化、监听关闭 | [loader.rs](../../tessivum-core/crates/tessivum-core/src/loader.rs)、[config.rs](../../tessivum-core/crates/tessivum-core/src/config.rs)、[事务场景](../../tessivum-core/crates/tessivum-core/tests/loader_transactions.rs) | 明确差集：TS Loader oracle 缺失；当前 import 的快照求值不等于 live service 依赖就绪后求值，provider 替换重求值需与产品接线一起补。D04 JS 留在可信 Legacy 边界，不放入原生 Core 求值器 | G01/G02 → C1；产品接线在 H1 | consumer-before-provider、pending update、替换/失败/恢复无过早求值；插入后再 patch、config 整体替换；失败保留最后有效配置，回滚失败可见；写回原子且监听关闭。恢复新实例不冒充旧 handle 复活，文件原子写不冒充内存/磁盘联合事务 |
| G04 | Native 与 Extism 独立加载/调用/更新/取消/卸载，ABI、资源上限与 capability 授权 | [Extism Host](../../tessivum-core/crates/tessivum-extism/src/lib.rs)、[Rust PDK](../../tessivum-core/crates/tessivum-pdk/src/lib.rs)、[ABI 场景](../../tessivum-core/crates/tessivum-extism/tests/abi.rs)、[runtime smoke](../../tessivum-core/examples/runtime-smoke/src/main.rs) | Native/Extism 实现已定位，真实语言产物的共同验收不足；TS envelope 和 ABI 文档有具体冲突，Go/Python Guest 缺失，见 D07 | C1 的服务/释放契约 → C2；语言扩展 W2/W3 | Native 和真实 Rust Guest 均经注册→调用→更新→取消→卸载；非法 envelope、trap、越权、内存/输出/时间上限、旧 handle 撤销失败封闭；其余语言不能借 Rust/WAT 结果通过 |

**C1 关闭的是 Core 行为与 Loader 基础门槛，不是整项多语言承诺。** 第二阶段可以依赖已验收的 Core 契约推进；D07 全部语言仍在第一阶段最终交付范围内，未完成时不可宣布“独立 Core 全范围完成”。

## 4. 第二阶段：Harness 基础能力

下表“核对”指有具体契约的差分/场景工作，不是要求先全仓重写。批次依赖详见第 7 节；D 项提供更细的交付单元。

| 范围 | 目标契约 | 现有资产 | 缺口／实施动作 | 依赖／建议批次 | 验收条件（待执行） |
|---|---|---|---|---|---|
| G05 | Agent turn/step、流式块、请求记录、工具配对、错误/取消；逐协议模型、usage、retry/replay | [agent_loop](../src/agent_loop.rs)、[llm](../src/llm.rs)、[tools](../src/tools.rs)、[Responses](../src/openai_responses.rs) | 主干已定位；完整 provider、原生鉴权、目录/配置与 replay 差集见 D06.1。不要重写已有 Responses replay，也不要将 RecordedLlmAdapter 当供应商证明 | G01/G02/G07/G10 → H2；provider 在 H3 | 一轮含真实工具及后续模型请求，先持久块再组装消息；完成锚点、usage/finish/retry 与取消唯一结算；重启/overflow 不重复已完成工具；逐协议回传历史 |
| G06 | Prompt 来源/顺序/去重、工作区指令、时间与会话引用；Inbox/Steering next-turn/next-step、编辑/删除/claim/FIFO | [system_prompt](../src/system_prompt.rs)、[agent](../src/agent.rs)、[agent_loop](../src/agent_loop.rs) | 实现已定位，未取得完整贡献者及所有队列状态组合的等价证据。逐贡献者列入输入输出对照，不能仅验 FIFO 正常路径 | G05/G07；Preset 贡献依赖 H1 → H2 | 同一请求内贡献顺序可追溯且不重复；claim 写入失败、取消和重启恢复 FIFO；编辑/删除与领取竞争不丢/重复消息；真实 Web next-step 与 next-turn 各验 |
| G07 | JSONL/SQLite、checkpoint/崩溃恢复、分支/继承、projection、统计/标题/分页、逻辑语料库/全文搜索/事件追踪 | [session](../src/session.rs)、[JSONL](../src/persistence_jsonl.rs)、[SQLite](../src/persistence_sqlite.rs)、[projection](../src/projection.rs)、[query](../src/session_query.rs) | 两后端与分页资产已定位；逐子项核对，不把文件存在当两后端等价。已交付增量读取不重复实现；Preset 代际继承差集由 D01 补齐 | C1 → H2；D01 补继承 | 相同持久事件在两后端恢复同一 surface/统计/标题；crash orphan、checkpoint、fork/seed、分页及 workspace 搜索边界；完整导出与有界驻留不互相牺牲 |
| G08 | token/容量、手动/自动 compaction、overflow 恢复、tool-result pruning，失败/取消不破坏历史 | [compaction](../src/compaction.rs)、[恢复计划第 10—11 节](CONTEXT_RECOVERY_DEVELOPMENT_PLAN.md) | 已有分段恢复、Goal CAS、持久层分页与历史 smoke；剩余是当前候选回归、原 Goal 个案证据、真实模型窗口覆盖，不能标成未实现 | G05/G07 → H2 | 压力/overflow/手动三路径；超大历史及不可拆保护组；失败、取消、CAS 冲突可恢复；实际缩减后才重试且工具只执行一次；原始日志保留、Browser compact/export/继续对话。原个案单列阻塞，不由合成用例代结案 |
| G09 | fs/Shell/子进程/PTY/Jobs、前后台、进程树清理，Spill 有界预览及完整输出回取 | [filesystem](../src/filesystem.rs)、[subprocess](../src/subprocess.rs)、[builtin_tools](../src/builtin_tools.rs)、[code_runtime](../src/code_runtime.rs)、[jobs](../src/jobs.rs) | 本地执行与输出落盘已定位；通用 Spill 契约与 subprocess spill 不混报，逐生产者核对完整回取。先冻结 E2B 所需 execution-world 边界，不为工具各加远程特例 | G10 与 G09 同单元 → H2；远端实现 E1 延后 | 真实命令/PTY、stdin/resize/signal、前台转后台及退出；超时/取消/owner 释放无子孙残留且不伤无关进程；完整输出可回取，预览截断不丢原件；路径/权限/Windows-POSIX 各自验 |
| G10 | Sandbox 与人机权限：审批先于副作用，迟到回复/取消安全 | [sandbox](../src/sandbox.rs)、[permissions](../src/permissions.rs)、[approval](../src/approval.rs) | 已有实现及分平台场景；仍需当前候选逐平台证据。Native/Legacy 可信执行不等于 WASM containment；不以宽松 fallback 掩盖 sandbox 失败 | C1/C2；作为 H1—H4 每单元准入条件 | 拒绝时无文件/进程/网络副作用；过期批准不授权新 run；取消后回复无效；平台 sandbox 不可建立时 fail closed；说明实际隔离强度 |
| G11 | Skills、MCP、LSP、Web 的发现/注册/撤销、边界、重连/取消与结果形状 | [skills](../src/skills.rs)、[mcp](../src/mcp.rs)、[lsp](../src/lsp.rs)、[web](../src/web.rs) | 明确差集：Rust LSP 只有 definition/hover/references，冻结版另有 goToImplementation；Web 缺口见 D06.3。Skills/MCP 已定位，需对照而非另建 registry | G09/G10 → H2；LSP/Web 补齐 H3 | 四个冻结 LSP 操作逐个真实 server 调用、坐标/返回与取消；MCP 分页/重连替换/旧 handler 失效；Skills shadow/路径/失效；三 search provider 和 fetch 分开验 |
| G12 | 子代理、Workflow/Ralph、Goal、Plan、Todo、Schedule | [subagent](../src/subagent.rs)、[workflow](../src/workflow.rs)、[goal](../src/goal.rs)、[planning](../src/planning.rs)、[schedule](../src/schedule.rs)、[host](../src/host.rs) | 原生父子/Goal 等已定位；Workflow 直线子集不足、外部四路径缺桥接，见 D02/D06.2。Goal 原截图根因未关闭，但不否认已有 CAS 修复 | G05—G10 → H2；D02/D06.2 → H3 | 父子归属/跟进/打断/恢复；Plan 审批退出、Goal 推进/CAS、Todo 折叠；Schedule 会话内跟进/取消、时区与 DST；Workflow 与四外部 provider 各自验 |
| G13 | 完整 Hooks 配置/匹配/命令/结构化决策，重复调用 Guard、通用工具 deadline、不变量注册与业务检查 | [approval](../src/approval.rs)、[invariants](../src/invariants.rs)、[jobs](../src/jobs.rs)、[bridge](../src/bridge.rs)、[tools](../src/tools.rs) | 局部 deny-only ApprovalHook 已定位，但不是完整 Hooks；其他局部 deadline 也不能代替通用工具 deadline。补冻结契约映射，未定位的完整接线不得写成通过 | G05/G09/G10 → H3 | 用真实 hook 命令验证匹配、允许/拒绝/结构化反馈及失败；重复工具提示可见；挂起工具到期、取消和迟到完成唯一结算；不变量注册/撤销及真实业务违例有诊断 |
| G14 | settings/credentials/workspace/attachments/非会话 storage、本地反馈和身份；动态生效、原子写、恢复与秘密保护 | [settings](../src/settings.rs)、[credentials](../src/credentials.rs)、[storage](../src/storage.rs)、[attachments](../src/attachments.rs)、[workspace](../src/workspace.rs) | 存储资产已定位；凭据四层来源与部分 provider settings 生效有差集，见 D06.1/D06.3；本地身份与排除外发见 D06.5 | C1/G07/G10 → H2；外发边界在 H1 先约束 | 冲突/写失败无成功通知或半写；重启恢复；配置保存后下一次操作实际生效；附件整批原子/路径验证；凭据优先级与诊断脱敏；反馈不上传 |
| G15 | Headless、SDK、ACP 服务端、Host API 各自 request/result/event/error/cancel/close；CLI/profile | [headless](../src/headless.rs)、[sdk](../src/sdk.rs)、[api](../src/api.rs)、[cli](../src/cli.rs)、[host](../src/host.rs) | 明确差集：sdk 的 initialize/session-new 合并分派与 prompt durable receipt 不等于 ACP 初始化/多会话/等待 idle。CLI 精确参数/退出、profile HMR 与 bundle HMR 仍需逐项对照 | G05/G07/G10、配置依赖 H1 → H3 | 四入口单独真实客户端；ACP 多会话隔离、文本/resource_link、permission、cancel/EOF 与消息更新；SDK 按自身结果语义；CLI 参数/退出码/配置覆盖。ACP child 客户端另归 D06.2 |
| G16 | 复用上游 Web 与客户端插件；boot graph、加载/卸载、样式、配置/HMR及真实交互 | [frontend](../src/frontend.rs)、[plugin_manager](../src/plugin_manager.rs)、[Web patch](../web/patches/deepseek-harness.patch)、[69 项清单](WEB_E2E_PORT_CHECKLIST.md) | 原有源 Web/boot graph 已定位；D03 动态双端与 D08 产品差异仍须各自证明，静态路由或旧 Chromium 通过不算当前兼容全量通过 | 所测能力所属 H1/H2/H3 → H4；可先分场景验证 | 真实 Rust Host、源 Web、HTTP/WebSocket 与持久层；69 项冻结清单逐项普通模式，动态卸载/HMR 无旧资源；pageerror/意外 console 问题不屏蔽；D08 与第一方额外场景单列 |

## 5. 已批准 D 项的具体交付单元

### D01：Preset 保真，而非退回第二套运行权威

- **目标与资产**：保留 Preset 配置/组合/创作/会话行为；复用 [agent_mode.rs](../src/agent_mode.rs)、[composition.rs](../src/composition.rs)及 Core Loader。
- **明确差集**：ModePluginManifest 只有 id/runtime/source/config，不能表达 group/isolate/inject/intercept/disabled；坏项列表、默认/删除、整目录复制、代际共享/继承和原子切换亦有差异。code→ptc、cordis→composition 的 ID 改写不等于保真迁移。
- **实施动作**：补完整组合描述与代际/选择事实；说明旧 YAML 直读或显式转换的用户路径。不能等价转换时明确报错并说明可信 JS 路径，不静默丢字段；不维护两套 Agent 权威。
- **依赖／批次**：C1 的 Loader、D04/D05 契约 → H1；持久恢复与子代理接线在 H2。
- **验收**：minimal/standard/code/cordis 四份真实配置逐项核对 schema/Prompt/参数；兄弟 realm 不串扰；带 JS/skills/资产的复制可用；损坏项不破坏健康项；默认/锁定/删除、切换失败保持旧事实；父配置变更后已有 child 继承父代际，新会话用新配置；自定义旧会话恢复给出明确迁移结果。

### D02：完整 Workflow 与独立 Ralph

- **资产与缺口**：[workflow.rs](../src/workflow.rs)为直线 parser，args 不参与执行、无完整脚本/parallel/pipeline；log、结果 envelope 与失败语义有差集。[subagent.rs](../src/subagent.rs)已有原生 Ralph 循环，不重写为另一个循环框架。
- **实施动作**：复用已有 JS 执行能力的可行部分，补可终止的脚本执行路径及 Rust-owned child/权限接口；不得靠增加若干语法分支伪称完整 JS。执行器选择在本单元做最小接线验证，未证实复用前不声明 Legacy/PTC 已支持。Ralph 补 schema/freshness gate、轮数配置、handoff/结果上限及失败携 lastReport。
- **依赖／批次**：H2 的 native child、G10；使用 JS 的部分依赖 H1/D05 → H3。
- **验收**：真实 args、循环、分支、结果驱动 prompt、parallel、无全局 stage 屏障的 pipeline；逐 child schema 与 LLM route；普通失败 null 与致命基础设施错误分开；并发/总数/items 上限；await 后死循环、worker 死亡、迟到 child 与取消收敛；顶层/嵌套日志与 append 失败；JSON envelope/文本投影；Ralph fresh rounds、complete/blocked/budget-limited 与失败交接。
- **不新增**：Workflow/Ralph 的进程重启续跑。历史读取与录制重放不是执行 checkpoint。

### D03：模型动态 Host/Client 插件

- **资产与缺口**：Composition 能运行 Native/WASM/可选 Legacy，但现有 inspect/define/validate/run/stop 不等价于上游七工具；Host 动态 API 的空 inventory、null manifest 与 `CORDIS_UNAVAILABLE` 不算实现。
- **实施动作**：保留 Plugin／不可变 Package／Run 的身份、版本与 current/next 事实；补 inspect 目录、双端源码/语法预检、依赖 waiting、浏览器审批与 exact Run 加载/私有 RPC、更新/显式回退、stop/undefine。复用现有 Loader、Host 与 Browser 生命周期，不增加 Agent/Session 权威。
- **依赖／批次**：H1 的服务/JS/配置边界、H2 的 session/tool/权限、G16 Browser carrier → H3，跨页真实交互在 H4 验收。
- **验收**：语法预检不执行；provider/consumer waiting→active→撤销→恢复；工具/Prompt 进入后续模型请求；未批准不执行；无页面 pending 不报成功；两页审批、刷新重连/迟到 ack、旧 Run 调用、更新失败指针与运行状态一致；stop/undefine 清理监听/UI/样式/RPC/待审批。重启后临时定义消失，旧引用明确不可用。
- **边界**：Host VM 与 Client closure 不是恶意代码沙箱；更新失败保留版本指针不代表旧副作用仍存活。

### D04：原生数据/受限表达式，加显式信任的 Legacy JS 配置

- **资产与差集**：Core 有受限表达式/事务，Node Loader 有表达式求值；但原生早求值、产品 ConfigScope 未接全、disabled bool 与特殊 bundle 转换不能覆盖冻结生命周期。JSON `__jsExpr` 能进入 Legacy 求值器，拒绝 YAML `!!js` 不是完整信任边界。
- **实施动作**：复用现有 Legacy Loader，不新建 JS 配置引擎；统一 YAML/JSON 等价编码、来源与执行信任校验；接好依赖就绪求值/provider 替换重求值、patch/HMR/rollback 和写回规则。原生配置不强制依赖 Node。
- **依赖／批次**：C1、D05 的显式服务与信任契约 → H1。
- **验收**：范围草案清点的 26 处真实表达式逐项转换或走可信 JS；env 缺失/空串/null、Windows/POSIX、baseUrl 路径；普通 config/group/disabled 求值时点分开；bundle→profile→home→CLI overlays；候选和回滚失败；dump 不执行；运行时缺失/不可信输入明确拒绝；不能将配置文件原子 rename 当插件外部副作用可回滚。

### D05：新旧 JS/npm 插件的可选运行路径

- **资产与差集**：[Node Bridge](../../tessivum-core/crates/tessivum-node-bridge/src/supervisor.rs)、[compat-host](../../tessivum-core/node/compat-host/src/host.ts)、[DomainBridge](../src/bridge.rs)已存在；固定 shim/服务集合不能支撑任意上游插件，更不等于任意 ctx 对象透明互通。
- **实施动作**：按消费者需求列出服务键、版本、注册/撤销、JSON 请求/结果、流、错误、取消与 owner；优先扩展现有 bridge，不新建第三条 JS 运行路径。LLM 增量 callback、外部 subagent run 与 Web provider 接口分别由其功能单元补齐，不预建无调用方代理。
- **依赖／批次**：C1，G10/D06.5 边界 → H1；功能桥接随 H3 对应调用方一起交付。
- **验收**：真实社区插件覆盖工具/服务/事件/Node API/Browser half；一个用户新编写 JS 插件从公开契约完成注册、调用、取消与释放；缺服务/版本或 runtime 显式报错；崩溃重启旧句柄失效；每项唯一执行归属。
- **运行时清单**：基础 Native、Legacy JS、PTC/Code Runtime 分列 Node/Bun/Python 实际依赖，特别不能漏记 PTC 的 Bun。普通 JS/npm 与 D07 的 TS/JS WASM 分开，不以任意 npm 兼容作承诺。

### D06.1：完整冻结 LLM 能力

**范围**：独立 DeepSeek、冻结 pi-ai 内置 provider、三种显式协议、自定义网关/模型、模型发现/配置/凭据/replay；不新增上游未接通的 OAuth 登录。

| 交付子项 | 现有差集／动作 | 验收条件（全部待执行） |
|---|---|---|
| 内置 provider 路由/鉴权 | 36 名称 catalog 已有，但省略 api 默认 Responses、通用 key/无 key 不能覆盖原生协议。优先评估复用冻结 pi-ai `0.82.1` provider，不为每家重造 SDK；每条 route 固定 Native 或 Legacy 唯一归属 | 逐 provider/模型实际 api；API key、AWS 凭据链、Google ADC/原生 API、Azure、Copilot、Mistral 等特殊路径分别验证。没有凭据时真实鉴权留“未验收”，不删除 provider |
| 模型与配置 | 单默认模型不等于完整 catalog；modelOverrides、headers、reasoning/thinking、cache/transport/timeout 等缺口补齐；离线 catalog 优先，非 catalog `/models` 按协议限制 | 模型目录与能力/容量、覆盖优先级、保存后请求生效；发现不多读 secret；不支持的发现显式拒绝 |
| 凭据 | live process env > YAML file 不等于冻结的启动环境快照 > 受管文件 > project .env > home .env | 四层来源、命名引用缺失不 ambient 回退；草稿 key/endpoint 变化、取消/错误脱敏；密钥不进入配置描述与录制 |
| 流/retry/replay | Responses 已有 replay；Chat/Anthropic 推理签名另补。当前 NodeLlmAdapter 收齐 chunks 数组后返回，不是跨进程增量流 | 首块在完整响应结束前可见，finish 后无块；取消/崩溃释放；同/跨 provider 历史还原、usage 和 retry 分层；Replay fixture 不冒充真实供应商 |
| 独立 DeepSeek | deepseek-official 映射通用 Chat，部署推理/timeout/retry 设置及后续 reasoning 历史需补；不与 pi-ai deepseek 混为一条 | 独立请求配置及工具后续推理还原；官方/自定义 endpoint 均不发送 D06.5 排除的两个关联头 |
| 显式协议和自定义网关 | 复用已有 Chat Completions、Responses、Anthropic Messages，不因补原生 provider 破坏已支持路径 | text/image/tool/usage、协议错误/取消/retry、自定义模型/endpoint 及历史 replay 各验；网关成功不冒充供应商原生鉴权通过 |

完整 36 项保留名单（不按是否有凭据删减）：amazon-bedrock、ant-ling、anthropic、azure-openai-responses、cerebras、cloudflare-ai-gateway、cloudflare-workers-ai、deepseek、fireworks、github-copilot、google、google-vertex、groq、huggingface、kimi-coding、minimax、minimax-cn、mistral、moonshotai、moonshotai-cn、nvidia、openai、opencode、opencode-go、openrouter、qwen-token-plan、qwen-token-plan-cn、together、vercel-ai-gateway、xai、xiaomi、xiaomi-token-plan-ams、xiaomi-token-plan-cn、xiaomi-token-plan-sgp、zai、zai-coding-cn。另保留显式 openai-codex profile 的冻结处理，不追加登录流程；radius 和 image-generation 不自动纳入。

**依赖／批次**：H2 的 LLM/请求/持久化、H1 的配置/秘密/JS 边界 → H3。验收台账按 provider × api/鉴权 × 模型能力拆行，不用“36 个名称存在”作为通过条件。依据：范围草案 Q03 及其中固定归档完整性证据。

### D06.2：四条外部子代理分别交付

当前 [SubagentProvider](../src/subagent.rs)返回本地 AgentHandle，Host 只注册 native；现有 subagents bridge 不提供外部 provider 注册。先补 run/生命周期契约，不以 native spawn/fork 或内部模型 route 替代。

| 路径 | 实施目标及验收条件（待执行） |
|---|---|
| ACP child | stdio initialize→session/new→prompt；配置 allow/reject policy、streamed text/StopReason；cancel→EOF→terminate→整树退出。与 G15 的 ACP 服务端方向相反，不能合并验收 |
| Claude Code | 官方 Claude Agent SDK 驱动宿主 CLI；父 cwd、显式 env、persistSession:false、禁用 AskUserQuestion；只接受非空 success text；iterator/CLI 失败、取消/重复 dispose 清理。不是 Anthropic provider |
| Codex | app-server stdio、ephemeral thread、单 turn、正确 thread/turn 的 final answer；无人值守 approval fail-closed、contextWindowExceeded、interrupt/提前退出。不是父级 approval forwarding，也不是 OpenAI provider |
| dsh-sdk child | 完整独立 Harness child，initialize/session/turn、child provider/model/maxTokens/composition；按 turn-end 结算；取消本地 aborted 后 bounded shutdown。child session id 不升级成父级会话权限 |

共同门槛：有效 workspace/cwd，无隐式回退到服务器 cwd；过滤 ambient credential-shaped 与 DSH_* 环境再叠加显式 env；启动前拒绝冻结版不支持的 outputSchema/depthLimit/toolFilter/persona；四者均为一次性外部 run，不承诺 continuable；Jobs 后台运行与续聊分开。真实外部 CLI/SDK 成功证据与协议夹具分别记录。

**依赖／批次**：H2 的父子/Jobs/进程/权限；复用 JS adapter 时依赖 H1 → H3，四条可独立交付且分别标状态。

### D06.3：三个 search provider 与 HTTP fetch

| 子项 | 现有缺口与实施动作 | 验收条件（待执行） |
|---|---|---|
| DeepSeek search | 已有 provider，但 settings 与实际构造环境快照未贯通；接当前请求一致配置/credential resolver | 下一请求生效且不混用新旧 endpoint/key；命名引用缺失不回退；citations/source 去重、request recording 无 secret |
| Exa | 未定位 provider 注册/实现；补冻结参数与凭据来源，不扩张全部 Exa API | keyword/neural/auto、numResults/highlights；无 highlight 丢弃；显式空 key 不回退 launch env |
| Perplexity | 未定位 provider；WebSearchResult 只有 sources/truncated，需要保留 generated content 与来源分别呈现 | search_results 优先、URL-only citations fallback、content/recency/max tokens；显式 key/launch env 与取消 |
| HTTP fetch/tool 呈现 | 已有 URL/同源 redirect/timeout/body 上限；缺 URL 长度、完整 MIME/charset/Content-Length 行为及 tool 层 HTML→Markdown | 非 HTTP(S)/含 credentials 拒绝，非 2xx 保留为数据，redirect/bytes/chars/timeout/cancel；静态转换/表格/正常脚本移除与转换失败回退；Spill 与工具截断分开 |

共同：零/多 provider、指定不存在/不可用 provider 分开报错；注册撤销与在途取消、每请求 source cap、工具启用/timeout/presentation metadata/replay 分别验证。冻结 fetch 没有 SSRF/private-IP 阻断；不复刻为安全保证，敏感内网部署没有有效出站策略就不能启用。默认 DeepSeek search 开启、fetch 关闭不等于删范围。

**依赖／批次**：H2 Web registry/G10/G14；若选 Legacy 则随功能补 D05 provider bridge → H3。不引入浏览器登录态、JS 执行或 DOM 浏览。

### D06.4：E2B 保留但延后

- **现状与动作**：本地 filesystem/subprocess 与 RemoteAccess 已有，但无 E2B owner/adapters。H2 先冻结 provider-neutral filesystem/subprocess/terminal/spill 的请求、错误、取消、owner/revocation；到 E1 再接同一个共享 sandbox owner，不为 bash/terminal/LSP 各写一套 E2B。
- **验收**：固定 `e2b@2.29.1` 或经明确变更核对的实现依赖；真实 sandbox 中 fs/commands/PTY 同 cwd/runtime root；setup rollback、symlink/mode/canonical/binary/UTF-8、远程输出/spill、进程组/resize/signal；setup/dispose/timeout 与重复删除竞态；API key 仅留宿主连接。
- **边界**：真实凭据/远程创建另行授权。无 key 可做协议夹具，live 保留未验收；不承诺 workspace sync、网络出口策略、发现或 cwd containment。E1 不阻塞基础迁移，但最终范围仍保留。

### D06.5：仅本地日志、反馈与身份，明确排除外发

- **资产与差集**：[telemetry.rs](../src/telemetry.rs)及 Host capture/backend 接口仍存在；默认 None 不证明所有路径满足边界。本地 feedback UUID 不等于旧 home 身份文件迁移。
- **动作**：核对并收敛产品可达的 telemetry/backend/配置/Legacy 默认组合，使旧配置/环境不能隐式恢复 FULL/FEEDBACK_ONLY/OTLP 或两个 DeepSeek 关联头；不支持配置显式说明/拒绝。保留本地日志、/feedback、赞踩/备注、身份隔离/重置与数据恢复，不顺带清空用户历史。
- **依赖／批次**：H1 先固定准入/出站边界，H2 补本地持久化，H3 每个 adapter 接入时复验。
- **验收**：隔离临时 home 与本地请求捕获；反馈不会上传且不声称已共享/送达；Native/Legacy、官方/自定义网关均无 `x-deepseek-harness-user-id`、`x-deepseek-harness-session-id`；正常模型消息/鉴权/流式保留。此门槛约束产品承接路径，不虚称任意受信任用户 JS 都受到网络沙箱。

### D07：四语言 WASM，分别完成

| 语言 | 已知资产／缺口 | 依赖／批次 | 通过条件 |
|---|---|---|---|
| Rust | PDK、wasm-minimal、runtime-smoke 已有；minimal 不覆盖 Host Functions 全面场景 | C1 → C2 | 真实编译 Guest 通过共同矩阵，不只加载/卸载 |
| TypeScript/JavaScript | 示例顶层 ok 与 Host deny_unknown_fields 冲突；ABI 文档“忽略未知字段”也不符；现有 scripts 仅 typecheck | C2 → W2 | 先统一 Host/PDK/文档严格 envelope 契约，修示例并建立真实编译/加载路径；不是只做类型检查 |
| Go | 未定位 Go Guest 工程 | W2 → W3-Go | 固定可行工具链/PDK和真实产物，通过共同矩阵；不要求另建通用 SDK 框架 |
| Python | 现有 benchmark Python 不是 Guest；未定位 Guest 工程 | W2 → W3-Python，可与 Go 并行 | 固定实际编译路线/限制并用真实 Guest 验证；不承诺任意 pip/C 扩展可用 |

共同矩阵：`cordis.plugin/v1` manifest、request/response、五个生命周期导出；Host Functions 授权/拒绝/撤销；错误/非法 envelope/trap；timeout/cancel/memory/output 上限；更新/卸载和旧 handle 失效。每种语言独立产物摘要与结果；保持默认 WASI/网络禁用，不为编译通过放宽沙箱。W2/W3 是建议批次名，Rust→TS/JS→Go/Python 顺序是已批准要求。

### D08：源 Web 复用与批准差异逐项验证

- **保留差异**：中立无 Key 首屏；无隐式模型注册/选择；默认只来自显式保存、恢复不漂移；未选/文本/未知能力模型仍能编辑图片草稿，拒绝发送后保留；`session.models.current=null` 与 inputModalities；远程首屏可编辑但远程终端拒绝。
- **动作**：沿现有源 Web/patch，不重写第二套 UI；每个受影响上游场景标明原契约或批准差异。未批准 patch 不因已存在就获得追认。
- **依赖／批次**：H2/H3 对应 Host 行为 → H4；远程部分复用 X02 安全轨道。
- **验收**：无 Key 新 home→草稿→拒绝仍保留→明确配置/选择模型→同一草稿图文发送；默认/恢复与错误清除；真实权限 admission；69 项基线保留对应入口，另跑 image-input、history-recovery 和第一方扩展。设备撤销、loopback terminal、WS 清理不由普通 UI smoke 代替。

## 6. 独立产品轨道 X01—X05

| 范围 | 现有资产与缺口性质 | 实施动作／依赖／批次 | 验收条件（待执行） |
|---|---|---|---|
| X01 | [Market 计划](PHASE7_FIRST_PARTY_MARKET_PLAN.md)与社区精确版本验证已有历史闭环；不是市场缺实现 | X-Market：在 G03/D05/G16 改动后复验安装、更新、卸载/回滚、Host-owned restart 及 verified/unverified/revoked；不复制社区 registry | 精确版本与完整性、未验证状态披露、失败不提交、卸载清资源；一个已记录真实社区版本重新验证。目录收录不等于安全或兼容 |
| X02 | [Remote 计划](PHASE8_REMOTE_ACCESS_COMPATIBILITY_PLAN.md)已有 pairing/device/revocation/tunnel 与最小配对页 | X-Remote：随 Host/Web/JS 路由变更复验权限边界；默认关闭，不作为 E2B 或远程终端 | HTTP/SSE/WS exact authority/Origin、配对与撤销、活动连接关闭、迟到授权失效、tunnel shutdown；远程 legacy HTTP/WS/PTY 拒绝；真实公网 tunnel 需独立环境与授权 |
| X03 | [Windows 计划](PHASE10_WINDOWS_NATIVE_RELEASE_PLAN.md)、[后续修复记录](WINDOWS_CHECKPOINT_20260916.md)已有实现/旧候选安装器、ZIP、CI 证据；当前发行门槛未关闭 | X-Release：复用现有 macOS/Linux/Windows x86-64 目标和脚本，按同候选源码→包内→安装/升级/回滚/卸载分别收口；不先重写安装器 | 每平台真实命令、sandbox/进程树、Web/PTC/Legacy/WASM；当前归档完整性、路径/ACL/PATH 类型与用户数据保护；版本/校验和/provenance 一致。Windows ARM64 不承诺；Quick Tunnel 的历史实测与当前平台支持声明另核对，不沿旧“有/无”笼统表述自动扩范围 |
| X04 | [固定 Linux 30 样本报告](PHASE9_BENCHMARK_REPORT.md)可复用，含 loader_update/Legacy readiness 回归 | X-Perf：只维持已有可复现 workload 与回归记录；涉及路径变化时同条件重测，功能等价是前置，不新增未批准阈值 | 保留全部样本、失败/回归、runtime tree PSS、环境与清理证据；不删兼容能力来取速度；不宣称生产模型延迟、数万 Agent 或固定单 Agent 内存 |
| X05 | 范围已明确排除冻结基线外新功能 | 不排实现批次；发现新协议/新上游功能/自有增强时逐项申请范围变更 | 变更说明能够追溯 G/D/X 依据；不得借兼容/性能重写增加未批准功能。不为此建立新 backlog 系统 |

X 轨道独立，不自动回滚已有能力，也不成为所有基础迁移的串行前置；但改到其共用依赖时必须保留相应回归。例如 D05 的插件样本验收不依赖 Market 已全部发行，Market 的实际安装回归则依赖 D05 的运行契约。

## 7. 建议批次与依赖

下面是建议顺序，不是用户已批准的开工排期；不预定版本号或工期。两阶段是项目职责与依赖关系，不要求为等待后续语言/远端凭据而停止全部 Harness 工作。

| 批次 | 建议交付物 | 硬依赖／退出门槛 |
|---|---|---|
| B0 | 固定产品/Core 候选、冻结基线、范围到证据映射；将历史通过/当前待验分开；标记旧计划与新批准差异的冲突 | 不改实现。得到可用于后续命令/产物的唯一候选身份、场景入口及明确的未验收项；不要求先重复全部历史 suite |
| C1 | G01/G02 上游可观察行为对照；G03 Loader/配置依赖生命周期与可移植 oracle | B0；Rust 自测与 TS 差分分别记账；补足实际差集，不接受 unsupported 充通过 |
| C2 | G04 Native/真实 Rust WASM 的完整生命周期、安全与 ABI | C1 所需服务/释放契约；真实 Rust Guest 通过共同矩阵。可与 H1/H2 无共享修改的部分并行 |
| H1 | D05 可选 JS 明确契约，D04 配置/信任与 D01 Preset 表达/代际；D06.5 出站边界 | C1；配置不提前求值、失败不提交、Native 不强制 Node、新 JS 插件最小闭环。先固定交互契约再并行具体消费者 |
| H2 | G05—G10、G12 本地编排、G14 的已有链路差分补齐；G11 Skills/MCP；execution-world seam 冻结 | C1；按单元依赖 H1 的配置/Preset，不因 H1 全批未结就阻止独立数据层工作。实际 Headless→工具→持久化→取消/重启闭环 |
| H3 | D02、D03、D06.1/D06.2/D06.3；G11 LSP、G13、G15 精确外部契约 | 各自所需 H1/H2，不强制这些独立能力互相等待；每条 provider/run/协议单独验收和支持标记 |
| H4 | G16/D08 与受影响 Host 契约的源 Web 综合验收 | 被测路径所属 H1/H2/H3；分场景可提前跑，全部基线的完成声明须依赖全部对应项。不把页面能开当全量通过 |
| W2 | D07 真实 TS/JS WASM 与 ABI 示例/文档一致 | C2；通过共同矩阵后才能标该语言已验证，不阻塞无该语言依赖的 H 批次 |
| W3-Go / W3-Python | 两种真实 Guest 分别交付 | W2；二者可并行，分别通过共同矩阵；延期不删除范围 |
| E1 | D06.4 共享 E2B owner + fs/process/PTY adapters | H2 execution-world seam、G10、真实 E2B 授权/环境；明确延后，不作为基础迁移门槛 |
| X-Market / X-Remote / X-Release / X-Perf | 第 6 节各轨道复验与已批准产品交付 | 按实际共享依赖进入，不设虚假的全局串行屏障；每轨独立证据，发行仍另行授权 |

**C1 当前执行记录（2026-10-10，纠正上一轮结论）**：真实 Native provider/consumer 回归证明，required provider 缺失时 `Pending` 是应保留的依赖等待，不是 Loader 事务失败。已移除上一轮“无进展即报错”的逻辑及断言该错误契约的 mock 回归；真实 activation error 的清理/旧树恢复不变。新增 `fixtures/conformance/loader-dependencies.json`：冻结上游真实 Loader 插件与 Rust `NativePluginRuntime` 分别执行等待、provider 到达、撤销、恢复及卸载，五个配置收敛边界的 consumer 可见结果一致；Rust 使用树替换，上游使用 entry 增删，不声称内部状态转换完全等同。oracle 改用模块相对路径，支持 `CORDIS_VENDOR_ROOT`，迁移到临时目录后实跑通过。G01/G02 全部差分、G03 其余 Loader 场景、G04 真实 Guest 仍未结案。

**C1 事件目录补齐（2026-10-10）**：已复现旧 TS oracle 忽略 `input.cases`、未知事件 ID 仍 PASS 的缺陷。现将 `events.json` 切换为 `event/event-catalog`，11 个声明用例全部执行真实上游 Cordis 与 Rust EventBus，并逐例比较调用次序、返回值、错误聚合或两次派发快照；旧 `event/emit` 不保留别名。两端 CLI 的 11 条实际观测一致；逐条篡改预期均在对应索引返回 MISMATCH，未知/空/畸形 case 均拒绝。定向 Rust conformance/events：8 passed；Bun 回归：1 passed、37 assertions；TypeScript 检查通过。未修改生产事件 API。此项关闭现有 11 项事件目录的差分缺口，不代表 lifecycle/service 的所有用例或其余 Loader 场景已完成。

### 跨运行时共同约束

1. Rust 持有 Host/Agent/Session/权限/持久事实；Core 保持领域无关。服务/插件有 scope owner，撤销后旧 generation 不再执行。
2. 一条 provider/工具/配置只拥有一条执行归属；Native 与 Legacy 可以分工，不同时维护两份事实。JSON/版本化服务契约明确，不能隐式透传泛型 Context 或 JS 对象。
3. 流、取消、错误和资源释放必须贯穿 bridge；返回一整包 chunks 不冒充增量流，pending 不冒充 active，拒绝功能不冒充迁移完成。
4. 配置、权限、动态执行的信任判定先于副作用；继承/恢复读取已提交事实，失败不伪造成功。执行资源的不可回滚外部副作用明确披露。
5. 修改一个公共接口时，同一单元迁移所有调用方/协议/文档及相关现有测试；跨仓 Core 变更在消费方锁定同一候选验证后才记完成。可复用的模块不推倒重写。

## 8. 验收执行入口与记录

以下是已有入口，不是本轮已运行结果。先确认候选/工作目录/依赖已准备，再执行；缺凭据或真实平台时保留待验状态。

| 领域 | 现有入口 | 使用限制 |
|---|---|---|
| Core | [oracle/package.json](../../tessivum-core/oracle/package.json) 的 `oracle`、[conformance](../../tessivum-core/crates/tessivum-core/tests/conformance.rs)、[Core fixtures](../../tessivum-core/fixtures/conformance) | 在 `tessivum-core/oracle` 用 `bun run oracle ../fixtures/conformance/<实际文件>.json`；Core 根用 `cargo run -p tessivum-core --bin conformance -- fixtures/conformance/<实际文件>.json`。`loader-dependencies.json` 已有双端轨迹对照；`loader.json` 的 TS 结果仍是 unsupported，不能充作完整 oracle gate |
| Agent/数据/压缩 | [agent_loop](../tests/agent_loop.rs)、[persistence_jsonl](../tests/persistence_jsonl.rs)、[sqlite_query](../tests/sqlite_query.rs)、[session_pagination](../tests/session_pagination.rs)、[compaction](../tests/compaction.rs)、[goal_approval](../tests/goal_approval.rs) | 先沿现有回归覆盖差集，再运行真实临时 home 的 Headless/重启/手动压缩。每后端分别记录；合成 Goal 用例不关闭原截图个案 |
| 执行/安全 | [fs_process](../tests/fs_process.rs)、[tools](../tests/tools.rs)、[jobs](../tests/jobs.rs)、[Windows sandbox](../tests/windows_sandbox.rs)、[Windows process](../tests/windows_process.rs) | 真实进程/文件/PTY/审批，不把 mock exit 当整树退出；Windows 必须真实 Windows，不以 macOS 编译代替 |
| provider/桥接 | [compatible_api](../tests/compatible_api.rs)、[openai_responses](../tests/openai_responses.rs)、[bridge](../tests/bridge.rs)、[legacy](../tests/legacy.rs)、[mcp_web](../tests/mcp_web.rs)、[skills_lsp](../tests/skills_lsp.rs)、[subagent_workflow](../tests/subagent_workflow.rs) | fixture 证明 wire/错误状态；真实模型鉴权、外部 CLI、Web 供应商、Guest 和 E2B 各需真实路径。缺实现时先补生产路径，不用测试替身交付 |
| SDK/ACP/CLI | [sdk](../tests/sdk.rs)、[headless_cli](../tests/headless_cli.rs)、[cli](../tests/cli.rs)、[api](../tests/api.rs)、[上游 ACP 契约](../../upstream/deepseek-harness/packages/acp/acp/README.zh.md) | ACP 服务端新增/补齐独立行为场景，不沿用 SDK slash 别名宣称兼容；每入口的 cancel/EOF/错误/返回时机单独核对 |
| Web | [69 项清单](WEB_E2E_PORT_CHECKLIST.md)、[web/tests](../web/tests)、[兼容检查脚本](../scripts/check_compat_baseline.py) | 真实源 Web/Rust Host/Browser；正常断言模式而非录制。静态 inventory 闭合不等于交互通过；批准差异与上游不变契约分开 |
| 发行/产品 | [Windows 报告](WINDOWS_CHECKPOINT_20260916.md)、[插件台账检查](../scripts/check_plugin_verification.py)、[发行事实检查](../scripts/check_release_facts.py)、[性能报告](PHASE9_BENCHMARK_REPORT.md) | 文档脚本/旧报告只是入口，不替代同候选真实包安装、升级/回滚/卸载、tunnel/market 或同条件 benchmark |

每个范围子项的结案记录至少包含：目标契约与批准差异、候选身份/平台、实际入口/命令、运行输入与产物、可观察结果、失败/取消/释放证据、未通过或未验收项、配置/数据/插件迁移影响。不能用一个总测试数替代矩阵。

永久回归只留能捕获真实行为风险的场景；常规新功能 smoke 可用临时脚本。优先复用现有 fixture、真实插件和上游场景，不为这份清单新增测试框架或空脚手架。

## 9. 本轮交付与尚需条件

本轮纠正上一轮 Loader Pending 契约误判，补齐一条真实 Native/上游 Loader 依赖生命周期对照；没有修改 Harness 产品层、Legacy JS、WASM 多语言、用户数据或发布配置。Core 定向回归：`cargo test -p tessivum-core --test native --test loader_transactions --test services --test service_races --test service_realms --test config --test conformance -- --test-threads=1`，35 passed。lifecycle/services/events/loader-dependencies 四份 fixture 均实跑 TS 与 Rust，轨迹逐项相同；篡改 Loader expectedTrace 后双方均返回 MISMATCH。`bunx tsc --noEmit` 通过；oracle 复制到临时目录并指定 `CORDIS_VENDOR_ROOT` 后通过，临时材料已移除。现有 `loader.json` 仍返回 TS UNSUPPORTED_SCENARIO，不计差分通过。

后续 C1 仍需独立完成完整 service/lifecycle 对照、其余 Loader/配置语义差集和消费方同候选验证；现有 11 项事件目录已具有逐例双端观测。真实供应商鉴权、外部 CLI、Guest、E2B、公网 tunnel 与包安装不因本次修正自动获得验收或授权。原 Goal 个案仍需要准确版本及脱敏日志。

本清单不覆盖或撤销历史批准。与 [旧开发计划](DEVELOPMENT_PLAN.md)、[兼容基线](COMPATIBILITY_BASELINE.md)、[插件兼容文档](PLUGIN_COMPATIBILITY.md)中的旧 Mode/服务注入/原样 UI 等口径冲突时，按已确认范围逐项标明：本轮保留旧记录，实施相应单元时同步现行契约和调用方，不能在未核对前机械恢复旧架构。

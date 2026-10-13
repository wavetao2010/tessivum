# Tessivum 两阶段迁移范围决策草案

> 状态：总原则、G01—G16 保留范围、D01—D03/D05 方案 A、D04“B 加可选 A”、D06.1 完整冻结 LLM 接入范围、D06.2 四条外部子代理路径、D06.3 完整 Web provider 范围、D06.4 保留但延后、D06.5 仅本地/排除模型关联头、D07 四语言 WASM 分批交付、D08 保留逐项 Web 产品差异及 X01—X05 产品扩展边界已于 2026-10-09/10-10 获用户确认。不是实施计划或完成报告。
> 本文保留 2026-10-09/10-10 范围确认时的记录与授权边界；“本次仅确认／未授权实施”描述当时，不撤销后续逐项执行授权，也不自动授权数据迁移、真实外部调用或发布。
> 本文不覆盖既有批准记录，不修改现有开发计划的勾选状态。与历史决策冲突时，先核对记录并单独确认，不自动撤销或追认。
> 2026-10-13：从 `tessivum-context-recovery/docs/` 原样恢复后纳入产品主仓库，文件名沿用以保持既有链接。后续范围维护以本文件为准；工作副本原件保留作历史凭据。当前候选与实施证据见[实施清单 §2](SCOPE_IMPLEMENTATION_CHECKLIST.md#2-固定事实与证据优先级)，不得以旧范围调查的候选身份代替当前源码。

## 1. 草案目的与基线

保留已明确的两阶段方向：

1. **独立 Rust Core**：通用 Context、服务、生命周期、事件、Loader 与插件运行时；不引入 Agent、Session、Tool 等产品领域类型。
2. **Harness 迁移**：依赖独立 Core，实现 Agent/Session/LLM/Tools、产品组合、协议入口与现有浏览器应用的接入。

已确认的总原则：**行为兼容优先，内部实现允许 Rust 化，外部差异逐项批准。**

- 用户功能、持久数据、外部协议、插件接口分别核对，不能互相代替验收。
- 不要求逐行翻译 TypeScript、复刻 JS Proxy/原型链或复制目录结构。
- 现有 Rust 实现是可复用资产，不是决定目标范围的依据；有代码、有路由、有测试文件不等于兼容完成。
- 延后交付不等于删除范围；技术上能够通过 Legacy 承接，也不等于已经原生迁移。
- 不把批准的差异描述为“原样兼容”，不把明确拒绝的功能统计成“已实现”。

### 冻结对象

| 对象 | 本草案采用的基线 |
|---|---|
| Harness | `0.1.0-rc.5` / `47f943859bef60e4160492346772ded9b24f765a` |
| Cordis 行为 | 上述 Harness 的 vendored Cordis 与本地补丁优先；独立 Cordis 上游用于辅助比较 |
| Rust 对应实现 | 上一轮功能对照定位到的 `tessivum-core`、`tessivum-context-recovery`；本文不宣布其当前运行验收状态 |
| 后续上游版本 | 不自动纳入；升级基线必须独立确认新增、删除和改变的契约 |

本草案是范围决策，不重跑行为验收。上一轮静态观察只用于定位风险；本轮复读了既有兼容基线和两阶段边界，不将旧测试结果提升为当前版本证明。

## 2. 建议集中确认的保留项

以下 G01—G16 已于 2026-10-09 经用户确认纳入保留范围。表中的“保留”指对用户和调用方的可观察能力，不要求复制上游内部结构。这不表示对应实现已完成或验收通过；具体替代方案和可选子项仍按第 3 节逐项决定，任何能力缩减须明确批准。

| 编号 | 建议保留的能力 | 范围边界与最低验收目标 |
|---|---|---|
| G01 | Core Context、服务依赖、隔离、拦截 | required/optional 依赖、晚到服务、provider 替换/撤销、隔离域与旧 handle 失效；Core 不反向依赖 Harness |
| G02 | Core 生命周期与事件 | 启动失败回滚、逆序及重入清理、卸载达到静默；事件顺序、短路、错误传播与监听器释放 |
| G03 | Loader、配置组合、插件更新 | 分层 patch、依赖驱动激活、候选验证、失败回滚、持久化与监听关闭；表达式范围见 D04，插件运行时边界见 D05 |
| G04 | Native 与 Extism/WASM 扩展能力 | 独立插件加载、调用、更新、取消、卸载；WASM 的 ABI、资源限制与 capability 授权；语言承诺见 D07 |
| G05 | Agent、LLM、工具调用主干 | turn/step、流式内容、请求记录、工具调用/结果配对、错误和取消；各协议的模型、usage、retry/replay 分别验收，provider 范围见 D06 |
| G06 | Prompt、上下文、Inbox 与 Steering | 工作区指令、来源/顺序/去重、时间及会话引用等扩展的逐项清单；next-turn/next-step、编辑/删除、claim、取消与 FIFO 恢复 |
| G07 | Session 数据平面 | JSONL/SQLite、checkpoint、崩溃恢复、分支与继承、projection、统计、标题、分页、逻辑语料库、全文搜索和事件追踪分别核对 |
| G08 | 长上下文管理 | token 计量、模型容量、自动/手动压缩、溢出恢复、tool-result pruning；压缩失败和取消不破坏可恢复历史 |
| G09 | 文件、Shell、子进程、终端、Jobs、Spill | 真实执行、权限与路径边界、前后台状态、PTY、进程树清理；有界预览与完整输出回取分开验收，不把截断当作保留 |
| G10 | Sandbox 与人机权限控制 | 分平台说明实际隔离强度；审批先于副作用，晚到回复与取消安全；Native/Legacy 信任边界不冒充 WASM 沙箱 |
| G11 | Skills、MCP、LSP、Web 能力 | 发现/注册/卸载、路径/网络边界、重连/取消、结果形状；LSP 按冻结版四操作核对，具体 provider 见 D06 |
| G12 | 子代理、Workflow、Goal、Plan、Todo、Schedule | 父子归属、跟进/打断/恢复、计划审批退出、目标推进、Todo 折叠、会话内定时跟进；Workflow 见 D02，外部子代理见 D06 |
| G13 | Hooks、Guard、运行时诊断 | Hooks 配置/匹配/命令/结构化决策、重复调用提醒、通用工具 deadline、不变量注册与业务检查；不以局部审批 veto 替代 Hooks |
| G14 | 产品数据与配置 | settings/credentials、workspace、attachments、非会话 storage、反馈与本地匿名关联身份；动态生效、原子写、恢复及秘密保护；D06.5 已明确排除 OTLP 导出与模型关联身份头，不将本地身份保留解释为允许外发 |
| G15 | Headless、SDK、ACP、Host API | 独立列出每个入口的请求、结果、事件、错误、取消与关闭契约；SDK 和 ACP 不合并验收；CLI 参数/profile 兼容面见 Q04 |
| G16 | 现有 Web 与客户端插件接入 | 继续源码复用，而非重写第二套 UI；boot graph、加载/卸载、样式、配置更新、HMR 与真实交互；产品行为差异见 D08 |

安全、可访问性、数据完整性、错误处理和资源释放属于各项功能的验收底线，不能作为“简化方案”省略。

## 3. 必须单独确认的替代决策

**D01—D03、D05 已确认方案 A；D04 已确认 B 加可选 A；D06.1 已确认保留完整冻结 LLM 接入能力；D06.2 已确认四条外部子代理路径全部保留；D06.3 已确认完整保留冻结版 Web provider 范围；D06.4 已确认保留 E2B POC 但延后交付；D06.5 已确认仅保留本地能力，排除 OTLP 导出与模型关联头；D07 已确认 Rust、TypeScript/JavaScript、Go、Python 四语言 WASM 目标，按 Rust → TS/JS → Go/Python 分批交付；D08 已确认继续复用上游 Web 源码并保留逐项记录、逐项验收的 Tessivum 产品差异。X01—X05 已分别确认：Market 与 Remote Access 保留独立轨道；平台与发行分阶段验收；性能只保留已测证据；冻结基线外新功能默认排除。** 推荐意见不是默认批准；未选择的项目留在范围表中，不据此写实现。

### D01：上游 Agent Preset 与 Native Agent Mode

- **决定（2026-10-09）：用户确认方案 A。** 保留上游 Preset 的配置、组合、创作及会话行为；Mode 可作为内部表示，不以当前 Mode 的表达边界缩减 Preset 能力。原 YAML 直读还是显式转换仍需说明，不能静默丢弃能力；本次确认不授权开始实现。
- **方案 A：保留上游 preset 的配置与组合能力。** 内部可以采用 Rust 表示，但现有 preset 的可表达能力、选择/锁定、默认值与损坏状态必须有对应行为。
- **方案 B：以 Native Agent Mode / `mode.toml` 替代。** 不再承诺原 `agent.cordis.yml` 可直接使用；逐项列出可迁移字段、必须改写的插件和无法表达的能力。
- **收益与代价：** B 有利于统一会话的运行策略与权限权威，但把兼容成本转移给配置和插件作者；不能仅因 B 已有代码就接受它。
- **调查依据：** Q01 首轮差集已支持本项选择，详见第 4 节。统一 Rust 内部模型，不要求保留两套运行权威；现有表达缺口属于待实现范围，不是获批删减。
- **通过条件：** 用真实 preset 样本说明“直接可用、可转换、需改写、不支持”四类结果；默认/锁定、会话恢复、工具/Prompt/插件组合均有验收方案。

### D02：Workflow 完整脚本与受限直线子集

- **决定（2026-10-09）：用户确认方案 A。** 保留完整 Workflow 脚本编排及 Ralph 的可观察契约，允许执行环境变化并复用已有原生实现；保留显式信任、资源上限、取消与清理边界。不新增进程重启续跑承诺，本次仅确认范围，不授权实现或执行工作流。
- **方案 A：保留冻结版 Workflow/Ralph 的可观察能力。** 执行环境可以变化，但需说明运行时依赖、可信代码边界、取消及子代理管理。
- **方案 B：只支持明确的直线语法子集。** 循环、分支、并发及其他不支持的脚本行为，按实际差集列出并拒绝；不能以成功空结果代替执行。
- **收益与代价：** B 的语法和执行边界较小，但可能要求用户重写现有工作流；真实损失须用上游样本确定，不能凭工具同名推定等价。
- **调查依据：** 第 4 节 Q02 首轮调查已确认通用 Workflow 的脚本/并发缺口，以及独立原生 Ralph 的实际覆盖与差异。当前直线子集只记为部分覆盖，原生 Ralph 按具体缺口补齐；不要求重写已具备等价行为的实现。
- **通过条件：** 明确语法/运行能力表、脚本迁移影响；phase、并发与失败语义（若纳入）、子代理清理、取消和历史恢复都有具体场景。

### D03：模型运行时动态插件与受限 Composition

- **决定（2026-10-09）：用户确认方案 A。** 保留模型动态创建、检查、运行、更新/回退、调用及卸载临时 Host/Client 插件的能力。Composition 可作为执行机制，不作为能力缩水的替代；功能存在不扩大默认执行权限。本次仅确认范围，不授权实现或执行动态代码。
- **方案 A：保留上游动态检查、编写、挂载、调用与卸载插件的能力。** 模型生成的代码仍受显式信任、权限和审批边界约束，不能由“支持自修改”推出自动授权。
- **方案 B：仅允许声明式 Composition 和已知 Native/WASM 能力。** 不执行任意 Host/Client JavaScript；完整列出原动态操作中被禁止的部分。
- **收益与代价：** B 缩小执行面，但改变了插件作者及模型可使用的扩展能力；现有 `CORDIS_UNAVAILABLE` 是拒绝边界，不是功能实现证据。
- **调查依据：** Q01/Q02 的 D03 首轮源码差集见第 4 节。可复用可选 JS 运行路径，但跨运行时服务与双端生命周期仍需实现及验证，不能用现有 Composition 或 Legacy 存在来宣称等价。
- **通过条件：** 每个动态操作都有支持或明确拒绝的映射；权限、持久事实、配置失败回滚和卸载均可观察。

### D04：可执行配置 `!!js` 与受限表达式

- **决定（2026-10-09）：用户确认 B 加可选 A。** 原生配置保持数据与受限表达式；显式信任的 JS 配置复用 D05 Legacy 路径，支持旧配置及用户新编写配置，不新增独立配置执行器。无需 JS 的配置不强制依赖 Node；无法等价转换时明确报错，不静默降级。本次仅确认范围，不授权实现或执行配置代码。
- **方案 A：受信任的 Legacy 配置路径保留 JS 表达式能力。** 需要相应运行时，配置视为可执行的可信代码。
- **方案 B：原生配置只接受数据与受限表达式。** 旧配置需要转换；无法转换的表达式明确报错，不改变求值顺序后伪称兼容。
- **收益与代价：** B 降低原生 Host 的执行面，但不自动覆盖上游依赖就绪后求值、provider 替换后重求值等行为；A 增加运行时和信任成本。
- **保留边界：** 依赖就绪后求值、provider 替换后重求值、配置更新/HMR、失败回滚和明确写回规则仍须保留并验收；配置来源及 YAML/JSON 等价表达式编码均受同一执行与信任边界约束，不因批准可选 JS 路径就默认信任所有输入或扩大权限。
- **通过条件：** 真实 profile 的可转换/不可转换清单；patch 顺序、依赖解析、disabled 求值、失败回滚与写回规则明确。

### D05：npm 生态兼容与“纯 Rust”边界

- **决定（2026-10-09）：用户确认方案 A。** 用户同时明确，JS 插件支持不只服务于旧生态兼容，也应支持用户编写新插件。保留可选 JS 插件运行路径，不因某个旧插件完成原生迁移就取消该扩展能力。
- **支持边界：** 新 JS 插件接入现有运行路径及显式宿主服务契约，不因此另建第三套运行时；具体插件作者接口仍待设计和核对。普通 JS/npm 插件与使用 Extism PDK 编译为 WASM 的 JS 插件是两条不同路径，后者不能透明提供 Node API 或任意 npm 兼容。D07 的多语言 WASM 承诺不因本项确认而自动批准。
- **方案 A：保留可选 Legacy 运行时。** npm 插件通过它运行；只承诺已定义、已验证的跨运行时服务，不承诺任意 JS 对象透明互通。
- **方案 B：仅支持 Native/WASM。** npm 插件作者必须移植；这是明确缩减生态兼容范围，而不是删除无用胶水。
- **收益与代价：** A 保留生态接入可能，但增加运行时、发行与安全边界；B 简化部署，却把迁移成本转移给插件作者和用户。
- **已确认方向：** 选择 A；把“Rust 拥有 Host/Agent 核心”与“所有配置都不需要 JS 运行时”分开。基础配置、Legacy 配置、Code Runtime/PTC 配置分别记录 Node/Bun/Python 等实际依赖，不能漏记 PTC 的 Bun 依赖。
- **通过条件：** 真实社区插件样本覆盖工具、服务、事件、Node API、Browser half，并有用户新编写的 JS 插件通过公开契约完成注册、调用与释放；支持矩阵写明所需运行时、服务代理、信任等级及失败方式。移植完成后移除该能力的重复执行路径，不保留两套事实来源，但不据此删除 JS 插件运行能力。

### D06：可选 provider、远端能力与遥测

按下表逐行确认，不能用“常用 provider”概括批准。**本草案不替用户选择最终供应商名单，也不默认排除可选能力。**

| 决策子项 | 选择空间与影响 | 建议 / 待确认内容 |
|---|---|---|
| D06.1 LLM provider 与原生鉴权 | 已确认保留冻结版 Harness 实际可达的完整 LLM 接入能力，不缩减为三种通用协议/key 模式 | 2026-10-09 用户确认；允许 Native/Legacy 分工，保留独立 DeepSeek、内置 provider 协议/鉴权/模型目录、显式协议及自定义网关、发现、配置、凭据与 replay；不额外承诺上游未接通的 OAuth 登录。仅确认范围，未授权实现 |
| D06.2 外部子代理 | 已确认 ACP、Claude Code、Codex、dsh-sdk 四条独立路径全部保留 | 2026-10-09 用户选择“全部保留”；允许分批实现，分别验收真实协议、权限、取消和退出；不用 native spawn/fork 或内部模型 provider 代替。仅确认范围，未授权实现 |
| D06.3 Web provider | 已确认完整保留冻结版 DeepSeek、Exa、Perplexity 三种 search provider 与 HTTP fetch | 2026-10-09 用户选择“完整保留”；允许 Native/Legacy 分工、分批实现和分别验收；默认部署行为仍可保持 DeepSeek search 启用、HTTP fetch 关闭；不扩张为浏览器、登录态、JS 执行或供应商全部 API。仅确认范围，未授权实现 |
| D06.4 E2B | 已确认保留共享远端 fs/process/terminal execution world，但延后交付 | 2026-10-09 用户选择“保留但延后”；先冻结 Rust provider-neutral execution-world seam，再做 E2B owner 与 adapters；E2B 不是当前基础迁移前置条件，也不自动提供 network policy、workspace sync 或 containment。仅确认范围/批次，未授权实现 |
| D06.5 Identity/Telemetry | 已确认仅保留本地日志、反馈与本地身份能力；排除 OTLP 遥测导出及模型关联身份头 | 2026-10-09 用户选择“仅保留本地”和“排除模型关联头”；不迁移 FULL/FEEDBACK_ONLY 上传、exporter 或采集目的地，也不迁移 DeepSeek 的持久 UUID/session ID 关联头（含自定义网关）；不是仅默认关闭或延后。正常模型消息与鉴权发送不受影响。仅确认范围，未授权实现 |

若选择限定支持范围，能力目录、错误响应、用户说明和验收条件必须同步限定，不能继续使用“全部 provider 兼容”等措辞。

### D07：多语言 WASM 的交付承诺

- **决定（2026-10-09）：四种语言都保留为最终 WASM 交付目标，按 Rust → TypeScript/JavaScript → Go/Python 分批交付。** 每种语言只有在真实编译产物、加载、共同 ABI、Host Functions、错误、取消和生命周期场景通过后，才标为“已验证支持”；延期或理论可移植不算完成。
- **方案 B：分层承诺。** 已编译运行并验收的语言标为支持；仅有通用 ABI/Extism PDK 可用的语言标为可移植，不算交付完成。
- **收益与代价：** A 覆盖面完整但增加维护面；B 更准确，但必须明确尚未兑现的语言目标，不能把原目标悄悄删掉。
- **建议：** 采用分层支持标签，交付顺序为 Rust → TypeScript/JavaScript → Go/Python；该建议已被用户确认。Go/Python 是延后交付，不是排除。
- **通过条件：** 每种承诺语言有真实编译产物和同一套 ABI 场景。上一轮定位的 TS 示例/Host envelope 不一致需列入核对，不能将示例存在视为通过。

### D08：Web 产品差异与源码复用

- **方案 A：恢复并保持冻结版用户行为。** 原始首次引导、默认选择等保持一致；品牌变化不改变协议和流程。
- **决定（2026-10-09）：采用方案 B。** 继续复用上游 Web 源码，同时保留逐项批准的 Tessivum 产品差异；无 Key 首屏、无隐式模型选择、图片草稿保留等分别记录和验收，不把整份现有 patch 一次性追认为合理。
- **收益与代价：** B 保留 Tessivum 的产品选择，但需要维护 patch 与差异验收；不能再声称所有行为均与未修改上游相同。
- **决定依据：** 历史计划、兼容基线、Web E2E 清单和源码 patch 已记录这些差异；本次确认其范围归属，不表示实现或验收已完成。新增差异仍需单独提交。
- **通过条件：** 每个受影响的上游场景都有“行为等价”或“批准差异”的明确归属；原场景被替换必须解释对应关系，不以删除测试缩小范围。分别验证原始契约面和产品差异面，不混报结果。

## 4. 先调查、不能让用户凭猜测选择的问题

这些是形成决策证据的只读调查项，不是获准实施的任务。本草案不自动启动下一轮调查；如需可执行实验，应另行明确隔离环境、凭据与是否有副作用。

| 编号 | 待回答的问题 | 需要的证据 | 支持的决策 |
|---|---|---|---|
| Q01 | Preset、Mode、Composition 与动态插件到底相差哪些能力？ | 冻结版真实配置/插件样本；字段、服务依赖、Host/Client half 与模型操作的逐项映射；可转换与不可表达样本 | D01、D03 |
| Q02 | Workflow/Ralph 和插件能力可通过什么运行时承接？ | 冻结版脚本、外部调用、事件及退出语义；现有 Native/WASM/Legacy 接口覆盖表；所需权限和依赖。仅有可行性推断时显式标注 | D02、D03、D05 |
| Q03 | 外部 provider 是缺实现、缺接线，还是可由现有兼容运行时承接？ | 固定源版本的 provider→注册入口→调用方→运行时映射；凭据和协议要求；不能只搜索类名后宣布缺失 | D06 |
| Q04 | CLI、Profile、HMR、SDK 与 ACP 的精确兼容面是什么？ | CLI 参数/退出语义；profile patch 与 client bundle HMR 分开；协议 request/result/notification/error 对照；SDK 与 ACP 分开 | G03、G15、D04、D05 |
| Q05 | Core 与多语言差分证据覆盖到哪里？ | Loader TS Oracle 未覆盖项与 Rust 自测分开；实际 ABI 与各语言示例对照；固定 Core revision 与可复用夹具 | G01—G04、D07 |
| Q06 | 哪些 Web、权限与远程边界已有用户批准？ | 现有决策/发布记录与 source patch 的逐项对应；区分历史证据、当前实现和当前版本复验 | D08、产品扩展 |

### Q01 首轮结果：D01 的真实 Preset 差集

2026-10-09：完成 D01 首轮调查后，用户确认方案 A；本轮只补充决策记录，不改运行代码。D03 首轮差集见下一小节；跨运行时服务逐项映射和动态验收仍未完成，不能标记整个 Q01 完成。

**结论：现有 Native Agent Mode 不是上游 Agent Preset 的等价表示。** 差异不仅是 YAML/TOML：上游组合插件子树、服务 realm、带表达式的配置和随目录携带的资源；当前 Mode 主要表达固定能力清单、Prompt 策略与扁平插件引用。D05 已接受新写 JS 插件，不能再把“统一 Mode”解释成只能选内置工具。

#### 实际样本与四类迁移结果

只读脚本扫描四份冻结样本的字段行，得到下表。插件行计数包含 group 行及 disabled 行；这是文本清点，不是加载、转换或运行验收。

| 样本 | 插件行 / group / isolate / `!!js` 数 | 与当前 Mode 的关系 |
|---|---|---|
| [`minimal`](../../upstream/deepseek-harness/apps/cli/config/agent-presets/minimal/agent.cordis.yml) | 8 / 2 / 2 / 1 | 完整 Prompt、双工具、持久 shell、无压缩的意图可映射；原样本另含 terminals/fs realm、动态 cwd、超时和输出限制，不能直接等同内置 minimal |
| [`standard`](../../upstream/deepseek-harness/apps/cli/config/agent-presets/standard/agent.cordis.yml) | 29 / 3 / 3 / 2 | 有按平台启停 bash/pwsh、plan/compaction/workflow realm、逐插件配置；固定工具集合和布尔开关不足以保真 |
| [`code`](../../upstream/deepseek-harness/apps/cli/config/agent-presets/code/agent.cordis.yml) | 30 / 3 / 3 / 2 | 在完整组合上增加 code presentation；Mode 的 programmatic 表达意图，但不证明底层组合或 Code Runtime 契约等价 |
| [`cordis`](../../upstream/deepseek-harness/apps/cli/config/agent-presets/cordis/agent.cordis.yml) | 30 / 3 / 3 / 3 | 有动态 Cordis 工具及 preset 自带 skills；Composition 名称映射不能替代这些能力，动态操作另归 D03 |

- **直接可用：** 上述四份 `agent.cordis.yml` 均不能直接作为当前 Mode 文档读取；发现入口只认 `mode.toml`。现有内置同名项不等于导入了原文件。
- **可转换：** 展示名/描述、静态 persona 文本及 complete、有对应能力的工具选择、direct/programmatic 意图、skills/planning/compaction 开关。插件 id/source/JSON config 有扁平承载位置，但包解析、服务和实际行为必须另验；没有现成保真转换器的证据。
- **需改写或补桥接：** `!!js` 的平台/cwd/baseUrl 求值、目录相对插件与资产、逐插件参数及依赖现有 Cordis 服务的 JS 插件。可保留 JS 作者语言；是否原源码可运行须核对 Host bridge，不得直接断言所有插件必须重写。
- **当前 Mode 入口无法表达：** 嵌套 group、entry 级 isolate/inject/intercept/disabled 等组合信息。`ModePluginManifest` 仅有 id/runtime/source/config；`CompositionDescriptor::validate()` 构造 Entry 时将这些选项置空或关闭。Core 本身是否支持这些字段是另一问题，不能归因于 Rust 或 TOML。

#### 不应被配置格式讨论掩盖的用户行为

| 行为 | 冻结上游 | 当前 Rust 入口的静态观察 |
|---|---|---|
| 损坏项可管理 | 缺文件、坏 YAML 仍列出 broken 行 | 无 mode.toml 的目录跳过；解析坏文档以 `?` 向上传播，名单可能整体失败；Host 返回的成功行 broken 恒为 None |
| 默认与删除 | 可存尚不存在的默认 id；删除当前用户默认时清除该默认 | 设置校验先 resolve；删除当前默认返回 MODE_IN_USE |
| 复制创作 | 整目录复制插件、skills、元数据和资产 | 重序列化 Mode 并复制声明的 WASM 文件，不是完整目录复制 |
| 插件生命期 | 同一 preset 代际常驻共享；agent 通过 scope 认父 | AgentLoop 创建时重新 resolve Mode；Composition 资源归 session，不能据此承诺相同共享语义 |
| 子代理继承 | 加入父 agent 正在运行的代际，文件改动或删除不改变这次继承 | 子会话头部默认取 parent.header().agent_mode；该处不是读取选择事件或父方运行快照。普通无种子子代理与带历史 fork 必须分别验收，不能泛称已经等价 |
| 空白切换与恢复 | 空白锁、选择事件、失败不改旧组合；运行中的子代理继承与冷读均有路径 | 有 running/非空锁及选择事件；选择入口先解析、再销毁旧 agent、追加事件，不在这里完成新插件激活。不能把 resolve 成功当作原子切换成功 |
| 旧数据迁移 | preset id 指向其实际组装 | Rust 持久化边界将 code→ptc、cordis→composition，自定义旧 id 报 MODE_MIGRATION_REQUIRED；这只是标识迁移，不证明工具、Prompt 或插件等价 |

上游 README 的空白切换段仍写卸载/恢复旧子树，但本版 `index.ts::recompose()` 实际先 `ensureStanding()` 再 rebind；本表按实现判断，不把过时描述当成必须复刻的内部机制。

#### D01 决定与验收边界

**用户已确认 A 的能力目标：保留用户可观察的 Preset 组合与创作能力，Mode 可作为内部表示，但必须补足表达缺口。** 不要求维护两套运行权威，也不要求复刻 JS Proxy；原配置直读还是显式转换应单独说明，不得静默丢失隔离、资源或插件能力。本项批准是范围决定，不代表现有实现已等价或验收通过。

后续验收场景（本轮未执行）：① 四份真实 preset 的模型工具 schema、Prompt、参数和依赖失败对照，覆盖 bash/pwsh 条件及 minimal 的持久 shell/fs 边界；② 同时挂载兄弟 preset 验证同名服务、工具、Prompt 不串扰；③ 复制带 JS 文件、skills 和资产的 preset 后移动副本，验证资源仍可解析；④ 损坏项仍可列出、读取/删除且不破坏健康项，覆盖默认 id 的设置及删除；⑤ 空白切换成功后恢复/fork/普通 subagent 一致，失败则旧工具/Prompt/日志事实保持不变，非空和 running 时拒绝；⑥ 父会话运行后修改/删除配置，子代理仍继承父方代际，新会话使用新配置，冷读不启动 agent；⑦ 自定义旧会话恢复有明确迁移结果而非静默回退。D03 的模型动态装卸和 Browser half 另列场景，不混在 D01 下宣布完成。

源码依据：上游 [Preset 用户契约](../../upstream/deepseek-harness/packages/preset/agent-presets/README.zh.md)、[代际及 recompose 实现](../../upstream/deepseek-harness/packages/preset/agent-presets/src/index.ts)；Rust [Mode schema/发现/复制](../src/agent_mode.rs)、[会话组装](../src/agent_loop.rs)、[Composition Entry 构造](../src/composition.rs)、[Host 默认/删除/选择](../src/host.rs)、[子代理头部继承](../src/subagent.rs)、[持久化标识迁移](../src/protocol.rs)。本轮证据为源码追踪与只读字段统计，未运行 JS 插件、模型请求或端到端兼容测试。

### Q01/Q02 首轮结果：D03 模型动态插件

2026-10-09：只读追踪模型工具、Host/Client runner、UI 与 Rust Composition/API/Bridge，并运行源码工具声明统计；不执行动态 JS、不启动页面或模型请求、不改运行代码。调查后用户确认 D03 方案 A；本节不表示 Q02 的 Workflow/Ralph 调查完成。

**结论：模型现场编写双端插件，与挂载已知插件描述符，是两个不同能力层。** 当前 Composition 有真实 Native/WASM/可选 Legacy 接线，但不能替代动态编写、版本、审批、Client UI 和调用闭环。D01 A 保留 Preset、D05 A 支持用户新 JS 插件；D03 A 则单独确认模型动态扩展能力的范围，不构成具体代码执行授权。

#### 源码基线与操作映射

上游 `tool-cordis/README.zh.md` 的“五工具、单包 id”说明落后于同目录源码。实际 `src/index.ts::apply` 注册七个工具：`cordis_inspect_list/query/self`、`cordis_define/run/stop/undefine`；模型是稳定 Plugin、不可变 Package 和每次激活的 Run，分别带 id。只读脚本提取到上述 7 项，对侧 Composition 为 `inspect/define/validate/run/stop` 5 项。数量用于发现文档漂移，不是覆盖率。

| 能力 | 上游实际源码契约 | 当前 Rust 路径 |
|---|---|---|
| 发现能力 | Inspect Provider 目录及 Host/Client 查询 Service、Event、Builtin、Tool、Slot 等；只读查询不是业务调用 | composition_inspect 查本会话描述符/状态，不等于服务与槽目录；动态 syncInspectManifest 默认返回 null，未保存对应目录 |
| 编写并定义 | code.host/code.client 为返回插件的普通 JS 函数体，至少一半；语法预检不运行；新增 Plugin 或追加不可变 Package | composition_define 接收 id、runtime/package 和 JSON config，无双端源码及 Plugin/Package 版本指针 |
| 运行和审批 | Host-only 直接启动；含 Client 按授权返回 awaiting-approval/starting，页面启动 Host、取 exact Run 源码、加载 Client、回报；成功才提交 current | composition_run 运行已验证 Entry；仅 activation=Active 提交，Pending 当失败清理；没有双端异步审批/提交闭环 |
| 服务与事件 | provide/inject、事件、工具/Prompt 注册；缺依赖 waiting，提供方出现后激活，消失后撤消费者贡献 | 有 Core 与固定 DomainBridge 服务分派，但 descriptor 不承载完整组合字段，Pending 处理也不同；不是任意 ctx 对象透明跨 Rust/JS |
| 浏览器扩展与调用 | Client 注册 Slot UI、样式/主题等，由页面 Loader 管理；harness.handle/host.call 提供 Client→Host 私有 JSON 方法 | FrontendStatic 有 package scan→boot graph→client 资源及可选 HMR，但不是模型新源码注入；动态 getClientCode/invoke 等入口仍拒绝 |
| 更新及回退 | update 先停旧 Run；失败保留 current/next 指针，不自动重启旧版本；显式 run 旧 Package 回退 | Draft→Validated→Active→Validated，无版本、current/next、update/rollback；单次启动清理不是版本回退 |
| 停止及删除 | stop 取消待审批并清 Run，保留定义/授权/版本；undefine 删除整个 Plugin；Host 按 exact Run 广播撤回 | composition_stop 释放 handle/scope 后回到 Validated；无模型侧 undefine 或双端撤回。会话回收不等于逐插件删除 |
| 历史与归属 | 模型管理操作绑定 Session owner；@pluginId 注入身份/状态后按需取源码；定义在内存，重启不恢复 | 有 ToolRunContext.session 隔离，不证明动态卡片、@引用、诊断或页面重连已实现 |

Rust `HostApi` 动态默认实现实际被 `HostHandle` 使用（没有三项 override），`HostRuntime` 仅转发：inventory 返回 `[]`，run_host_half 返回 `CORDIS_UNAVAILABLE`，call 只有 syncInspectManifest 返回 null，其余拒绝。API 路由存在不等于能力存在，空/null 也不是动态同步完成。与之分开，Host boot 确实给 Composition 注入 Native/WASM 和可选 Legacy runtime，不能反称 Rust 不支持 JS 插件。

另一个动词差异：上游 define 的语法预检只构造 Script；Rust composition_validate 会实际 instantiate 后 dispose，不能据其名称承诺任意插件校验都无副作用。

#### 信任与未决边界

- Host node:vm 与 Client closure/façade 都不是恶意代码安全沙箱；Host helper 有逃逸风险，vmTimeoutMs 只限制同步求值，不约束整个异步任务。A 不等于默认允许所有模型生成代码运行。
- Client 审批区分单 Package 与同 Plugin 后续版本授权；Host-only runner 本身不走这道审批。代码执行权限、工具审批、Browser 授权、远程访问权限须分别记录，保留能力不要求保留安全漏洞。
- 工具管理操作按 session owner 限制，但 UI inventory 是全局面板；私有 invoke 校验存活 Run 和 method，不能据此声称所有 Web 请求天然隔离。部署鉴权另验。
- 更新失败保留旧版本指针，不等于旧副作用仍在运行；Host 撤回与页面清理有异步时序，不等于同步证明每页静默。刷新、多页竞争、迟到 ack/retract、渲染失败须验收，不把已有竞态当成目标能力。
- [INFERENCE] 已确认的可选 JS 路径可承接动态 Host 半，Rust 保留核心事实与权限权威；但 JS provide/inject 与 Rust 服务代理、动态注册进入模型工具视图、Client 私有 RPC 仍需逐项设计和验证。WASM 不能透明替代任意 Node/浏览器代码。

#### 决定与验收场景

**用户已确认 D03 方案 A：保留模型按需创建、检查、运行、更新/回退、调用及卸载临时 Host/Client 插件的能力，默认授权不随功能存在而扩大。** Composition 可复用为执行机制之一，不是功能替代，不再建一套 Agent/Session 权威。普通受信任 JS 插件开发继续按 D05 A 保留。本项批准不表示现有实现等价或验收通过。

后续验收（未执行）：① define 语法错误不留新定义、成功不执行代码，精确检查不可变版本；② 真实 provider/consumer 样本先缺依赖 waiting、后激活、停 provider 撤工具、重启恢复；③ 动态工具/Prompt/事件进入后续模型请求，stop 后消失且跨 owner 管理拒绝；④ Client 首次拒绝/单版本批准/未来版本批准及无页面 pending，不得报为成功；⑤ 更新 Host/Client 失败后 current/next、运行状态、诊断/steering 一致，可明确回退；⑥ Slot UI→私有 Host JSON 调用成功，旧 Run 调用和迟到撤回不影响新 Run；⑦ 两页审批、刷新、断连重连、渲染异常按契约收敛；⑧ stop/undefine 清理服务、工具、监听、RPC、UI/样式及待审批，历史卡片不假装仍存活；⑨ 重启后定义消失，旧 @引用明确不可用；⑩ 未批准代码、越权服务/会话与未授权远程请求拒绝。执行与浏览器验证须在明确的隔离环境内另行进行。

证据：上游 [七工具](../../upstream/deepseek-harness/packages/extensions/tool-cordis/src/index.ts)、[模型约定](../../upstream/deepseek-harness/packages/extensions/tool-cordis/src/prompt.ts)、[Host 状态机/RPC](../../upstream/deepseek-harness/packages/extensions/cordis-host-runner/src/index.ts)、[求值边界](../../upstream/deepseek-harness/packages/extensions/cordis-host-runner/src/sandbox.ts)、[Client 编排](../../upstream/deepseek-harness/packages/extensions/cordis-client-runner/src/client/orchestrator.ts)、[Client 生命周期](../../upstream/deepseek-harness/packages/extensions/cordis-client-runner/src/client/runtime.ts)、[UI inventory](../../upstream/deepseek-harness/packages/extensions/ui-cordis/src/client/inventory.ts)、[真实依赖组合样本](../../upstream/deepseek-harness/packages/extensions/cordis-host-runner/tests/composition.spec.ts)；Rust [Composition](../src/composition.rs)、[Host 接线](../src/host.rs)、[API](../src/api.rs)、[Bridge](../src/bridge.rs)、[浏览器 boot graph](../src/frontend.rs)。本节是静态调查与验收设计，不宣称端到端通过。

### Q02 首轮结果：D02 Workflow 与 Ralph

2026-10-09：本轮只读调查上游工具/worker 与 Rust Workflow/Ralph，运行源码声明清点；未执行工作流、模型调用、构建或测试，未改运行代码。调查后用户确认 D02 方案 A。本节补充语法、结果、生命周期及真实样本证据，不宣称跨运行时承接已验证。

**结论：通用 Workflow 的差距是完整编排能力，而不只是多几个语法关键字；Ralph 则已有独立的 Rust 循环，必须单独验收。** 选择 A 不要求 Rust 实现 JavaScript 解释器，也不要求把已存在的原生 Ralph 重写为脚本；要求的是保留用户可观察契约。

#### 通用 Workflow 对照

| 能力 | 上游实际源码 | 当前 Rust 实际入口 |
|---|---|---|
| 脚本语言 | 普通 JS async 函数体；可用循环、分支、函数、表达式、await，返回无损 JSON；不是 TS/JSX，也不注入完整 Node API | parser 仅认 literal phase、const binding=await agent(literal)、return {bindings}；至少一个 agent，必须原样返回每个 binding 恰好一次，连注释也拒绝 |
| 输入与元数据 | args 进入脚本；meta 必需 name/description，可有 whenToUse/phases，未知字段拒绝 | 工具 schema 接收 args，但 NativeWorkflowEngine 不读取；meta 仅 name 必需，description 不参与执行，schema 不接受 whenToUse/phases |
| 串行依赖与计算 | 可把前一个结果加工后作为后一个 prompt，按 args 动态展开任务 | agent prompt 必须单/双引号字面量，不能引用前一步结果或 args，也不能拼接/模板化 |
| 并发和流水线 | parallel 等所有 thunk 完成；pipeline 每条 item 独立跨 stage，无全局阶段屏障；agent 并发槽、总 child 数及每次 items 有部署上限 | 顺序 await 每个 child；无 parallel/pipeline/条件/循环/自定义函数；Host 固定 max_total_agents=64，不是同一并发策略 |
| 子代理选项 | agent 可传 label/phase/schema/provider/model；schema 为受支持的对象根子集，effort/isolation/agentType 等拒绝 | 不解析第二参数，固定 native OneShot，继承父 AgentOptions，无逐调用 schema 或 LLM 路由选项 |
| 路由含义 | 部署 subagentProvider 决定子代理执行后端；agent 的 provider/model 是 LLM 路由，二者不能混称 | Native Workflow 子调用直接指定 native；Legacy/PTC 存在不代表这个入口已换成 JS worker |
| 失败语义 | 普通 child 非完成返回 null；普通 thunk/stage 异常让该 item 为 null；非法参数/schema、上限和启动/基础设施异常穿透 combinator，不得悄悄变 null | 无上述 combinator；run_child 拼接最后 assistant 文本，空文本报错；收尾可将 child Error 升级 WORKFLOW_CHILD_FAILED，不能称与上游失败数据语义等价 |
| 进度 | phase/log 为实时观察事件；phase 可附到 child-start；不应把所有叙述都称作持久事件 | phase 改内存标签并附到 child-start 记录，没有独立 phase 事件；WorkflowContext::log 是空方法且 parser 不接受 log |
| 工具返回 | 成功规范值 {runId, agentsStarted, result}，result 可为 JSON 对象/数组/标量；文本投影按 maxResultChars 有界 | 工具返回格式化文本，details=null；子结果只形成 binding→字符串对象，计数取对象字段数，本入口没有相同的规范 envelope/渲染上限 |

上游真实样本 `workflow-worker-thread/tests/session.spec.ts` 使用 `log(...args.files.length)`、`pipeline(args.files, (prev,item)=>agent('read '+item))`，另有 parallel thunk 和条件 stage。当前 parser 不能原样接收这些样本。它会报 WORKFLOW_FAILED/不支持直线子集之外语法，不应描述成“假装执行成功”；但 args 接收后不消费是另一项明确差异。

`host.rs` 实际构造 NativeWorkflowEngine::from_recording → WorkflowRuntime → register_workflow_tool。WorkflowEngine trait 可扩展，不等于产品已接上其他引擎；经 PTC 调用同一个 workflow 工具也不会绕过此 parser。[INFERENCE] 完整 JS 可以由独立可终止的脚本执行器承接，Rust 保有父子代理与权限权威，但现有 Legacy/PTC 不能未经接线与验收就算替代完成。

#### 取消、资源与恢复不能混成一个“支持”

- 上游每 run 一个 Node worker；cancel 中止共享 child 信号，宽限到期可强制终止 worker。dispose 幂等并启动 child 清理、等待至宽限，再终止线程及扫描幸存 child；源码明确慢 child 清理可能超过等待界限，不能把 README 的“完全停稳”理解为所有外部资源无条件归零。
- Rust 有父级/调用方 cancellation、关闭 admission、清理已接纳 child、等待在途启动、清理迟到 child，然后记录 run-end；取消分支仍等待 engine execution 返回，没有上游 worker 的强制终止机制。对当前受限引擎与未来任意 JS 执行器，必须分别验收，不能直接给执行器套一个 token 就承诺可停止死循环。
- 上游普通 workflow 工具仅对顶层调用记录 run-start/agent-start/agent-end/run-end，嵌套 transport 不记录；首次 append 失败停止该 run 后续记录并告警，不改变执行结果。Rust 也有四类记录和失败后停记，但本工具入口没有相同的顶层/嵌套判定，记录失败也不在这里告警。这些日志是观察事实，不是脚本 checkpoint。
- 上游明确无执行 journaling/resume；Rust 有 session 历史和 recorded 工具结果重放，但 parser 仍先执行，recorded 结果也必须匹配字符串 bindings。两者均不能据此承诺进程重启继续脚本指令、并发任务或 Ralph 轮次。记录读取、录制重放和执行续跑必须分开。
- Worker/vm 不是恶意代码安全沙箱；脚本的 CPU 隔离、取消和环境变量收紧，不等于权限隔离。选择 A 仍需要显式信任、运行时依赖及子代理资源上限。

#### Ralph：已有循环，但尚非完整等价

上游 Ralph 是部署提供的固定循环脚本，经 WorkflowEngine 执行；Rust `subagent.rs::run_ralph` 独立执行 `for round in 1..=max_rounds`，不经过 Workflow parser。已有每轮新 child id、OneShot/resume=false、不 seed 父历史、前一轮报告 handoff，以及 continue/complete/blocked 和 budget-limited 分支；不能将它列为“缺循环”。

| Ralph 契约 | 已有覆盖与差异 |
|---|---|
| Fresh 与报告 | Rust 每轮创建新 child，prompt 携目标及上一报告；五字段 JSON 和状态约束已有校验。上游还检查 provider 支持 structured output 且不继承父上下文，并将 schema 交给 child；Rust 当前解析末尾文本 JSON，不能当成同一结构化输出契约 |
| 轮数及部署 | Rust 固定上限/默认 64；上游插件默认 256 且可配置，随附 standard preset 明确设为 64。差异是部署可配置性，不能把插件默认和随附实际配置混比 |
| 有界 handoff/结果 | 上游限制 maxHandoffChars 和 maxResultChars；Rust Ralph 入口没有相应字符边界，不能宣称只携“有界”报告 |
| 失败信息 | 上游普通 child 失败先形成 round-failed + lastReport，再由工具报错并展示最后成功交接；Rust 失败直接走 SUBAGENT_TOOL_FAILED，没有同等失败轮次/上次报告输出 |
| 返回与完成含义 | 上游返回 {runId, agentsStarted, result}，呈现明确为 worker 自报完成/阻塞，不是独立验收；Rust 返回 JSON 文本及 childSessionId/result details，外层契约不同。不能把任一 worker complete 当独立正确性证明 |
| 取消与历史 | Rust token 传到 activation，取消时 dispose，循环结束/错误后还调用 parent.dispose；不能因此声称已具备 worker 强停与同等错误边界。双方都没有 Ralph 轮次/handoff 的重启续跑 checkpoint；child 历史持久化不等于循环恢复 |

#### 决定与后续验收

**用户已确认 D02 方案 A：保留完整 Workflow 脚本编排和 Ralph 的可观察契约，允许执行环境改变。** 保留按输入扇出、结果驱动分支/循环、并行流水线和逐 child 选项；已有原生 Ralph 可以继续复用并补齐缺口。显式信任、资源上限、取消和清理仍须验收；历史读取及录制重放不扩大为进程重启续跑承诺。本项批准不代表现有实现已等价或验收通过。

验收场景（未执行）：① 上述真实 args/pipeline/parallel 样本及条件循环、结果驱动 prompt 正确执行，非法语法不创建 child；② schema/独立 LLM provider-model 与部署 subagentProvider 分开验证，普通失败 null 和基础设施致命错误区分；③ 验证流水线无跨 stage 屏障、并发/总数/items 上限、排队取消；④ 启动前取消、启动中迟到 child、脚本 await 后自旋、worker 死亡、慢 child dispose 不泄漏 authority，终态不被迟到结果改写；⑤ 顶层/嵌套记录、append 失败、运行/成员配对、日志重载与执行恢复边界；⑥ 成功 JSON envelope 和有界文本投影一致；⑦ Ralph 两轮 fresh child/共享 cwd/无父 seed/只传上一交接，以及 complete/blocked/budget-limited、失败轮次带 lastReport；⑧ Ralph 非法/超大报告、provider freshness/schema gate、轮数配置及中途取消清理。复用上游 `tool-ralph/tests/integration.spec.ts` 等真实场景，不以少量语法/轮数单测代替端到端证据。

证据：[Workflow 工具](../../upstream/deepseek-harness/packages/workflow/tool-workflow/src/index.ts)、[worker 脚本运行](../../upstream/deepseek-harness/packages/workflow/workflow-worker-thread/src/runtime.ts)、[worker 取消与清理](../../upstream/deepseek-harness/packages/workflow/workflow-worker-thread/src/host.ts)、[上游运行恢复边界](../../upstream/deepseek-harness/packages/workflow/workflow/README.md)、[真实脚本样本](../../upstream/deepseek-harness/packages/workflow/workflow-worker-thread/tests/session.spec.ts)、[上游 Ralph](../../upstream/deepseek-harness/packages/workflow/tool-ralph/src/index.ts)、[Ralph 集成场景](../../upstream/deepseek-harness/packages/workflow/tool-ralph/tests/integration.spec.ts)、[Rust Workflow](../src/workflow.rs)、[产品接线](../src/host.rs)、[Rust Ralph](../src/subagent.rs)。本轮证据是源码与只读清点，不是运行通过报告。

### Q02/Q04 首轮结果：D04 可执行配置

2026-10-09：只读调查冻结版 Loader/Include、真实 bundle/preset、原生 Core 与产品 Legacy 接线。运行静态清点：三个随附 bundle 共 18 处非注释 `!!js`，四个随附 agent preset 共 8 处，共 26 处；该数字不是全仓统计。另将本地 `config.rs`、`loader.rs`、Node bridge `supervisor.rs`、compat-host `host.ts` 与产品锁定的 Core `eb1bc9fd320f880f0939689adf9af1b666a4b682` 缓存逐字节比较，四文件均一致。未执行配置表达式、插件、构建或测试；未修改运行代码。调查后用户确认 D04“B 加可选 A”；不代表实现等价或验收通过。

**结论：配置语言、求值时机、更新事务是三个独立契约。** 上游 `!!js` 不只是启动时替换字符串；原生有受限表达式与回滚基础，但目前不能据此声称与上游配置生命周期等价。

#### 真实配置与转换边界

| 冻结版真实样本 | 原生转换判断（不是已完成迁移） |
|---|---|
| base/code/standard/cordis 中的 `disabled: !!js process.platform === 'win32'` 及反向判断 | 可由宿主平台数据表达，但当前 Entry.disabled 仅 bool，不能直接放表达式；Rust OS 值为 windows/macos 等，不能原样沿用 Node 的 win32/darwin |
| base 的 `process.env.DSH_TELEMETRY_MODE \|\| 'DISABLED'`、permission/OTLP 的 `??`、minimal 的 `DSH_CWD ?? process.cwd()` | 需授权环境输入和缺省规则；空串在 `\|\|` 与 `??` 下不同。当前 Env 未提供即报错，Coalesce 不捕获该错误，不能机械翻译成现有 coalesce |
| base/web 的 `dshHomePath(...)`、base/minimal 的 `process.cwd()` | 可由宿主明确提供已解析路径；当前受限表达式无函数调用，不能将字符串拼接当跨平台路径/来源解析的等价实现 |
| web 的 `ctx.webStartup.host/port ?? ...`、trustedHosts；headless 的 `ctx.headlessStartup.task` | 可投影 JSON 服务数据，但必须保留依赖就绪后求值与 provider 更换后重求值。数组投影与数字缺省也须验收，不能仅凭存在 Service/Coalesce 枚举宣布覆盖 |
| base 的 permission→approval 三元表达式、cordis preset 的 `process.getBuiltinModule('node:url').fileURLToPath(new URL('skills/', baseUrl))` | 当前配置语言不能原样表达；可信 Legacy 保留 JS，或明确转成宿主声明式数据并验证路径来源、权限含义，不能静默换默认值 |

原生 `ConfigExpression` API 有 env/platform/architecture/profile/service、concat/equals/not/and/or/coalesce，但配置叶子 `$expr` 只接受字符串引用 `env.NAME`、`platform`、`architecture`、`profile.NAME`、`service.NAME.path`；不接受组合 AST 或通用 JS。API 的 Literal 仅 string/bool/null（普通配置数值仍可作为数据存在），布尔运算不做 JS 隐式转换，service 缺字段报错而非返回 undefined。因此“API 有算子”不等于上述 YAML 已可转换或兼容。

#### 生命周期与写回

| 边界 | 上游冻结版 | 原生/产品现状 |
|---|---|---|
| 普通 config 求值 | YAML 先保留 `{__jsExpr}`；普通 entry 的 fiber 等声明依赖可用后，经 internal/config 插值再校验/执行。Group/Include 自身保持字面数据，嵌套行在各自上下文求值 | Core import_entry 在 resolver/instantiate/activation 前，用固定 ConfigScope 求值 config/intercept；不是读取当前 Context 中服务的懒求值 |
| provider 替换 | 依赖触发 unload/reload，fiber 从 raw `_config` 重求值；pending update 保存 raw，不提前访问缺失服务 | 重新 import 会按同一 scope 求值，但服务生命周期本身不自动更新该快照。Host 创建 product_loader 只设置 context，未接入 with_config_scope；默认 env/profile/services 均为空 |
| disabled | Entry 级门控，首次判断在自身 fiber 创建前；用 Entry context 求值并转 Boolean。不能把它当普通 config 一样等该行服务就绪 | Entry.disabled 是 bool。产品 bundle resolver 仅识别一种精确文本形式的 loader.entries 去重表达式，在应用整份 patch 前折算 bool；其他 `!!js` 拒绝，不是通用或按每条 patch 动态求值 |
| patch 顺序 | 在 clone 上依序 apply；新插入行可被后续 patch 命中，config 整体替换；缺目标/name 不匹配告警跳过 | Core 也有 clone/validate 与 UpdateConfig 整体替换；产品 bundle override 也是整体替换。Host compose_profile 另走 JSON 对象合并，不能混称同一 patch 语义；当前 bundle 未命中 id 静默略过 |
| 更新失败 | Include 串行应用，成功才提交；Entry/Group 失败尝试恢复旧配置，rollback 失败聚合报错 | Core replace 先导入候选，再 dispose 旧实例、激活候选；失败尝试重新导入/激活旧树。恢复的是新实例，不是让已销毁句柄复活；两边都不能撤销任意 JS/插件外部副作用 |
| 原始表达式与写回 | dump 保留表达式、不执行 app 参数 provider；disabled raw node 通常保留。普通 active config 的 internal/update 可能把 resolved/simplified config 写回 options，不能承诺任何更新均逐字保留表达式 | Core 保留原始 EntryTree，persist 是独立调用，临时文件 sync 后 rename；内存更新并不自动持久化，更不是内存与磁盘的联合事务 |

上游现有 `user-patches.spec.ts` 包含 consumer-first、provider 移除后 consumer 消失、provider 恢复后取新值、坏 provider 使求值失败而不发布 consumer、再次恢复的场景。这些只是已阅读的测试契约，本轮未执行，不能写成已通过。

#### Legacy 已有承接点，但不是输入与权限均已闭环

产品通过 product_loader → LegacyNodeRuntime → PluginLoad(entry) 发送配置；compat-host 初始化上游 Cordis Loader，loadPlugin 调用 loader.create，updatePlugin 调用 loader.update。Core 递归配置处理拒绝 `$js`/`!!js`，却保留普通 `__jsExpr` 对象；上游 Loader 正是识别这个对象后执行 JS。因此源码存在 JSON 表达式节点抵达 Node 求值器的路径，不能笼统说“Legacy 没有表达式执行能力”，也不能把原生 YAML 的 `!!js` 文本拒绝当成整个跨运行时路径的代码执行防线。

但这并非现有 YAML profile 原样兼容的端到端证据：原生 parse_entry_tree 直接拒绝包含 `!!js` 的文档，bundle 路径只有前述特殊转换，Node loadPlugin 仅转交选定 metadata；live ctx 是 Node 侧上下文，不是任意 Rust 服务对象。必须明确原始节点归属、路由、信任校验和可见服务，才能承诺 A 已实现。[INFERENCE] 最小承接方案是复用现有 Legacy Loader，不另建配置 JS 引擎；具体跨运行时服务契约仍需实现阶段验证。

上游求值器使用 `new Function` + `with(ctx)` + `eval`，可访问 Node 全局与上下文，配置应按可信可执行代码对待，不是安全表达式沙箱。配置来源、依赖包、用户 patch、JSON 编码节点应在同一执行边界校验；不得因 D05 允许 JS 插件就自动信任所有输入，也不得为补支持而扩大默认权限。

#### 决定与后续验收

**用户已确认 D04：B 加可选 A。** 原生配置保持数据/受限表达式；显式信任的 JS 配置复用 D05 Legacy 路径，既可承接旧配置，也可供用户新编写配置使用。无需 JS 的配置不因此强制引入 Node。不能原生等价转换的表达式明确拒绝或由用户选择可信 JS 路径，不静默降级。完整配置更新/HMR、依赖求值及回滚能力仍在保留范围；本项批准不重开已确认的 D01/D03/D05，不扩大默认执行权限，也不代表现有实现已等价或验收通过。

验收场景（未执行）：① 26 处真实样本逐项映射，验证 Windows/POSIX、env 缺失/空串/null 与路径基准；② consumer 在 provider 前、pending 更新、provider 替换/失败/恢复均无过早求值或旧值泄漏；③ 普通 config、tree-carrier 与 disabled 时点分别验证；④ bundle→profile→home→CLI overlays 顺序、insert 后 patch、config 整体替换及未命中告警；⑤ 求值/import/dispose/activation/rollback 失败与快速连续 HMR，最后有效树、错误及资源状态可观察；⑥ dump 不执行表达式，文件写回按明确规则保留/改变 raw 值，失败不写成功配置；⑦ `!!js` 与 JSON `__jsExpr` 等价编码均受执行路径及信任约束，Node 运行时缺失/不可用时显式报错；⑧ 跨运行时服务投影明确，不以可运行 Node 代码冒充任意 Context 透明互通。

证据：[原生表达式](../../tessivum-core/crates/tessivum-core/src/config.rs)、[原生 Loader](../../tessivum-core/crates/tessivum-core/src/loader.rs)、[Rust Node bridge](../../tessivum-core/crates/tessivum-node-bridge/src/supervisor.rs)、[Node Loader 接线](../../tessivum-core/node/compat-host/src/host.ts)、[产品 Host](../src/host.rs)、[bundle 补丁入口](../src/plugin_manager.rs)、[上游求值器](../../upstream/deepseek-harness/vendor/loader/src/config/utils.ts)、[上游 Loader hook](../../upstream/deepseek-harness/vendor/loader/src/index.ts)、[Fiber 生命周期](../../upstream/deepseek-harness/vendor/cordis/src/fiber.ts)、[Include 补丁](../../upstream/deepseek-harness/vendor/include/src/index.ts)、[依赖及 disabled 场景](../../upstream/deepseek-harness/packages/boot/app-boot/tests/user-patches.spec.ts)、[base 样本](../../upstream/deepseek-harness/packages/bundle/base/cordis.patch.yml)、[web 样本](../../upstream/deepseek-harness/packages/bundle/web-app/cordis.patch.yml)、[headless 样本](../../upstream/deepseek-harness/packages/bundle/headless/cordis.patch.yml)、[preset 样本目录](../../upstream/deepseek-harness/apps/cli/config/agent-presets)。本节只完成 D04 首轮调查，Q04 的其他 CLI/SDK/ACP 契约仍需分别核对。

### Q03 首轮结果：D06.1 LLM provider 与原生鉴权

2026-10-09：本节只调查 D06.1，D06.2—D06.5 仍分别待调查/确认，不合并批准。只读核对冻结版 LLM、产品路由/凭据与 Node 桥接，运行静态源码清点；未读取用户密钥，未请求供应商接口，未执行模型、构建或测试，未修改运行代码。调查后用户确认 D06.1 完整冻结 LLM 接入范围；不代表现有实现等价或验收通过。

**结论：provider 目录、已注册路由、协议实现、鉴权、模型发现及历史 replay 是不同的支持维度。** 当前 Rust 内置 pi-ai 目录有 36 个名称，另有 deepseek-official；这些名称及 endpoint/model 默认值不能充当供应商接入证据。

#### 锁定依赖与精确目录

本地缺少 pi-ai 安装源码后，进一步只读下载 npm 的固定版本发布归档（内存解包，未安装依赖、未运行包代码）。[pnpm lock](../../upstream/deepseek-harness/pnpm-lock.yaml) 将 `@earendil-works/pi-ai` 锁为 0.82.1；[发布元数据](https://registry.npmjs.org/@earendil-works%2Fpi-ai/0.82.1)及[发布归档](https://registry.npmjs.org/@earendil-works/pi-ai/-/pi-ai-0.82.1.tgz)的 SHA-512 与锁文件完全一致：`3WFYRhEp3lQB3444EhPMBcM7zSaEUE3eJgHOR7s4081NLqbw/FsWilIKWXSua0Gv3sRr7m9xMidR3pPDE7jI/A==`。这消除了以当前新版网站目录代替冻结目录的风险。

归档 `dist/models.generated.js` 的 MODELS 有 37 个 provider key；`dist/providers/all.js::getBuiltinProviders` 返回这些 key。除仅 OAuth 的 openai-codex 外，36 项源码均声明 api-key 方法，与 Rust 36 项目录名称集合相同。**名称没有少不代表实现没有缺。** 完整 36 项为：amazon-bedrock、ant-ling、anthropic、azure-openai-responses、cerebras、cloudflare-ai-gateway、cloudflare-workers-ai、deepseek、fireworks、github-copilot、google、google-vertex、groq、huggingface、kimi-coding、minimax、minimax-cn、mistral、moonshotai、moonshotai-cn、nvidia、openai、opencode、opencode-go、openrouter、qwen-token-plan、qwen-token-plan-cn、together、vercel-ai-gateway、xai、xiaomi、xiaomi-token-plan-ams、xiaomi-token-plan-cn、xiaomi-token-plan-sgp、zai、zai-coding-cn。另保留上游对显式 openai-codex profile 的处理，不新增 OAuth 登录承诺。

已直接阅读归档的 amazon-bedrock、google-vertex、google、azure-openai-responses、github-copilot、openai-codex provider 源码：Bedrock 有 bearer token 或 AWS 凭据链，Vertex 有 Cloud API key 或 ADC/project/location，Google 使用原生 Generative AI API 与 GEMINI_API_KEY；Azure 使用专属 Responses API；Copilot 有 token 环境路径和库级 OAuth，但 Harness 未接 OAuth store。静态 API import 清点另确认 Mistral 使用 mistral-conversations，部分 provider 按模型支持多种 API，不能按品牌强制压成一条通用协议。上述是代码能力边界，不是实际鉴权通过证据。

范围不跟随整个依赖包任意扩张：归档还有 radius provider 工厂，但不在 MODELS/getBuiltinProviders 的冻结目录中，不能自动添加为默认 catalog；依赖的 image-generation provider 也不因 LLM 迁移自动纳入。显式自定义 route 仍按冻结 Harness 的模型/API 配置规则处理。

#### 上游可达边界与 Rust 产品路径

| 维度 | 冻结版上游 | 当前 Rust 实际入口 |
|---|---|---|
| 内置 provider 与手动协议 | llm-pi-ai 对不显式指定 api 的内置 route 复用 pi-ai provider，自带原生协议/鉴权；手动声明/显式覆盖才限于 openai-completions、openai-responses、anthropic-messages | Host parse_routes 只接受这三种 api，省略时一律默认 openai-responses；内置目录仅提供 endpoint 和一个默认 model，没有复用供应商 provider 实现 |
| 原生鉴权 | 未配置 apiKeyEnv 时交给 catalog provider 原生解析，例如 AWS/Google 专有环境与凭据机制；不是通用无鉴权 | 空 credential_ref 要显式 auth=none，生成 without_key 快照，实际不带密钥；不是自动获取 AWS/ADC/OAuth。有 key 时走通用 Bearer 或 Anthropic key header |
| 命名凭据缺失 | 配置了 apiKeyEnv 就必须解析该引用；不存在时报 MISSING_CREDENTIAL，不能回退到其他 ambient key | Host 同样对命名引用缺失报错；此项已有基础，不能因原生鉴权缺口而说所有凭据逻辑缺失 |
| OAuth | Harness 不持有 pi-ai OAuth store、不运行登录流程；仅 OAuth 的 catalog provider 默认不展示，已有显式 profile 仍可编辑。显式 key 可增加 harness api-key method，但不等于登录/刷新流程 | 未见产品 OAuth 登录/刷新链；不把库潜在能力直接加进迁移承诺。外部 Codex 子代理属于 D06.2，不与 openai-codex 模型 provider 混同 |
| 模型目录 | 默认取完整已安装 catalog，可整体替换或 modelOverrides 单项覆盖，并继承模型能力/容量 | 每个内置项仅一个默认模型，或使用用户模型列表；不是完整 catalog。RawOpenAiRoute 拒绝 modelOverrides 等未知字段 |
| 模型发现 | 优先本地 catalog，无网络/凭据读取；非 catalog 才按 OpenAI-compatible /models 查询，其他协议明确拒绝或手填 | compat_discover_models 对允许的请求走 HTTP /models，没有上游 catalog 短路；不能把 Bedrock/Vertex 名称拼到 /models 后称原生发现 |
| provider 配置 | 支持 headers、reasoning/thinkingBudgets、cacheRetention、transport、timeout/streamIdleTimeout、compat 等；仍受上游 schema 限制 | RawOpenAiRoute 只保留显示名/api/baseURL/凭据/auth/models/retryPolicy/enabled，其他上述字段被拒绝。协议能发请求不等于配置行为等价 |
| 历史 replay | 保存 API/provider/model、response 身份及 thinking/text/tool signature 等 adapter-private 数据，并与 durable 内容共同恢复历史 | Responses 已有 output replay_state，同 provider/model 时可重用，不能再列为“所有 live adapter 均无 replay”；Chat/Anthropic 的签名和推理历史需另补。RecordedLlmAdapter 不是供应商 replay 证明 |

产品实际链路为 settings → parse_routes → DynamicRouteResolver → CompatibleApiAdapter。三协议只是线协议分派，不具备 AWS SigV4/region、Google Vertex project/location/ADC、Google 原生生成协议、Azure 特有配置及 Copilot 专用 token 流程的等价证明。一个显式配置为支持协议的中转网关可以是独立 route，但不能冒充供应商原生接入。

#### DeepSeek 与凭据来源也有独立差异

- deepseek（pi-ai catalog）和 deepseek-official（上游原生适配器）是两条路径。Rust parse_deepseek_route 将后者映射到通用 Chat Completions；thinking/reasoningEffort/streamIdleTimeout 在配置解析中校验，却未进入 ResponsesRoute，provider retryPolicy 也未作为该 route 的自定义策略保存；请求级 reasoning_effort 的存在不能证明部署默认已生效。
- 通用 Chat 请求跳过历史 Reasoning block，而冻结版的推理/工具后续请求有 provider 专属还原规则；“收到 reasoning_content 并显示文本”不等于“下一轮正确回传推理状态”。
- 冻结版 credentials-local 区分启动环境快照 > 受管文件 > project .env > home .env。当前 Host 使用 Credentials::new，解析 live process env > YAML file，没有同样的四层来源语义；需要与配置/CLI 来源契约一起验收。
- 凭据配置应只保存引用，密钥在操作边界解析；显式缺失引用不得回退到别的账号。发现草稿 key、endpoint 变化时是否复用存储 key、错误脱敏及取消均须分别验收，不能用成功的无鉴权 mock 证明真实鉴权。

#### Legacy 不是已经接好的通用兜底

Rust bridge 有 llm@1 register/generate 和 NodeLlmAdapter，但当前 NodeLlmAdapter 等 callback 返回完整 chunks 数组后才产生 Rust stream，不是增量跨进程流。compat 的 `@deepseek-ai/dsh-llm` shim 仅导出 createUserMessage，不能因此声称上游 llm-pi-ai 可直接无改动挂载。D05 的 JS 扩展批准保留了承接方向，不代表 LLM 注册、模型目录、流式取消、凭据及 replay 桥接已全部完成。

[INFERENCE] 可优先复用冻结 pi-ai provider 及其鉴权实现，由 Rust 保有 Host/Agent 与权限权威；不必为每家供应商重写一套 SDK。但应补齐明确的跨运行时流/错误/取消/元数据契约，且同一路由只能有一个执行归属，不能同时保留 Native 与 Legacy 两套事实来源。

#### 决定与验收边界

**用户已确认 D06.1：保留冻结版 Harness 实际可达的完整 LLM 接入能力，允许 Native/Legacy 分工。** 包含独立 DeepSeek 路径、内置 catalog provider 的实际协议/原生鉴权/模型目录、三种显式协议、自定义 endpoint/model、模型发现、凭据来源、配置生效及 provider replay。不缩减为三协议/key 模式；保留上游自身的限制，不额外承诺自动 OAuth 登录、任意协议覆盖、无限模型或较新版本的新功能。本次仅确认范围，不授权实现、使用凭据或请求供应商接口；D06.2—D06.5 不随本项自动批准。

D06.5 后续批准的明确差异：不迁移 DeepSeek 的 `x-deepseek-harness-user-id` 与 `x-deepseek-harness-session-id` 关联头，包括自定义网关路径；不影响本节其余模型协议、鉴权与 replay 范围。Native/Legacy 复用都必须遵守该范围，不能因复用上游 adapter 恢复被排除的关联发送。

验收场景（未执行）：① 固定 catalog 与模型事实，未配置/已配置/可用/失败各状态独立；② 内置 provider 不被错误默认成 Responses，显式协议覆盖按冻结限制生效；③ API key、AWS/Google 等供应商原生鉴权分别用真实协议与可用凭据验收，命名引用缺失绝不 ambient 回退；④ catalog 发现离线返回，非 catalog /models 与不支持协议分开；⑤ headers、模型覆盖、推理参数、缓存/transport/超时/retry 配置确实进入请求；⑥ text/image/tool/usage/错误/取消、推理签名及同/跨 provider 历史还原；⑦ DeepSeek 部署默认与工具后续推理历史；⑧ Native/Legacy 流式交付与中断、进程崩溃清理、秘密不落入诊断；⑨ 启动环境/文件/project-env/user-env 优先级及运行中凭据更新。没有供应商凭据时可先验证协议夹具，但保留真实鉴权“未验收”状态，不能据此删除该 provider 或宣布通过。

证据：[上游 provider 构造](../../upstream/deepseek-harness/packages/llm/llm-pi-ai/src/provider.ts)、[目录与凭据解析](../../upstream/deepseek-harness/packages/llm/llm-pi-ai/src/index.ts)、[模型 catalog](../../upstream/deepseek-harness/packages/llm/llm-pi-ai/src/catalog.ts)、[配置](../../upstream/deepseek-harness/packages/llm/llm-pi-ai/src/config.ts)、[发现](../../upstream/deepseek-harness/packages/llm/llm-pi-ai/src/discovery.ts)、[replay](../../upstream/deepseek-harness/packages/llm/llm-pi-ai/src/replay.ts)、[DeepSeek 配置](../../upstream/deepseek-harness/packages/llm/llm-deepseek/src/index.ts)、[上游凭据](../../upstream/deepseek-harness/packages/credentials/credentials-local/src/index.ts)、[Rust Host](../src/host.rs)、[协议分派](../src/compatible_api.rs)、[Responses replay](../src/openai_responses.rs)、[发现 API](../src/api.rs)、[Rust 凭据](../src/credentials.rs)、[Node 回调桥](../src/bridge.rs)、[兼容 LLM shim](../compat/host-modules/@deepseek-ai/dsh-llm/index.js)。本节为源码调查与范围建议，不是供应商接入通过报告。

### 已有观察如何处理

上一轮已经定位到的 ACP/SDK 语义差异、LSP 四操作与三操作差集、Workflow 直线子集、动态 Cordis 拒绝边界，都是需要清楚处置的项目，不自动获得“替代方案已批准”状态。

不得混入较新上游的 file content、DeepSeek request-files 或 image-offload 等后续功能来夸大 rc.5 缺口。Responses adapter 已有 replay state，不能继续把它列成“所有 live adapter 都没有 replay”。通用 Spill 与已有 subprocess 输出落盘也必须区分。

### Q04 首轮结果：D06.2 外部子代理

2026-10-09：本节只调查 ACP、Claude Code、Codex、dsh-sdk 四条外部子代理路径；未启动外部进程、未读取用户凭据、未安装 SDK、未修改运行代码。四者均是一次性、进程外 child，不共享父 Cordis Context，不继承父会话历史；上游统一将 `outputSchema`、`depthLimit`、`toolFilter`、`persona` 声明为不支持，并以能力门控在 provider 启动前拒绝，而不是接受后静默忽略。[上游统一能力契约](../../upstream/deepseek-harness/packages/subagent/subagent/src/types.ts)、[进程外公共结算](../../upstream/deepseek-harness/packages/subagent/subagent/src/out-of-process.ts)

#### 四条路径的真实边界

| 路径 | 实际协议与运行时 | 输入/配置 | 权限、结果与生命周期 |
|---|---|---|---|
| ACP | 启动配置指定的 child ACP server；stdio NDJSON，`initialize` 后 `session/new` 与 `session/prompt` | 父 workspace `cwd` 或已验证的绝对配置 `cwd`；文本 block；配置 `allow`/`reject` 权限策略；显式 child env | `reject` 默认拒绝所有 permission；`allow` 只选首个 `allow_once`/`allow_always`。折叠 streamed text，映射 `end_turn`/`max_tokens`/`refusal`/`cancelled`；取消先发 ACP cancel，再以 stdin EOF→terminate→等待整棵树退出清理 |
| Claude Code | 官方 Claude Agent SDK `query()`，SDK hook 启动宿主 `claude` CLI；本包不实现 Claude 协议 | 父 `cwd`；文本 block 原样拼接成 prompt；显式 env；`persistSession:false`；固定禁用 `AskUserQuestion`；模型、工具、认证由宿主 Claude 配置决定 | 不提供 approval forwarding；用户询问类交互被禁用。只接受非空 success result 文本；SDK/进程/iterator 错误统一 `error`；取消/`dispose` 幂等关闭 query 并终止整棵进程树 |
| Codex | 启动宿主 `codex app-server --stdio`；JSON-RPC `initialize`、`thread/start(ephemeral)`、单次 `turn/start` | 父 `cwd`；非空文本 block 逐项输入；不选择 model/provider，不恢复 thread；模型、sandbox、认证由宿主 Codex 配置决定 | command/file approval 只做无人值守拒绝策略（有 cancel 优先 cancel，否则 decline）；permissions 空集、user input 空 answers、MCP elicitation decline，不是向父级转发审批。只取同一 turn 的 final answer；`contextWindowExceeded` 映射 `max-tokens`，其余失败为 `error`；取消尽力 `turn/interrupt` 后关闭 wire、终止并等待整树 |
| dsh-sdk | `@deepseek-ai/dsh-sdk-client` 驱动完整 child Harness；stdio JSON-RPC，先 `initialize`，再创建 child session 与一次 turn | 父 `cwd` 或已验证配置 `cwd`；child `provider`、`model`、可选 `maxTokens`、args/env 均显式配置；child 自己决定 Cordis composition、工具和凭据 | 不支持父级 prompt cancel，取消先本地结算为 `aborted`，再执行 bounded shutdown 与进程清理；按 child `turn/end` 映射 completed/max-tokens/aborted，缺失或其他 reason 为 error；session id 只在 child 进程内有效 |

四者的共同前置条件是可进入的 workspace；没有父 `cwd` 且未配置 child `cwd` 时必须失败，不能回退到 Harness 启动目录。四者都从父环境移除 credential-shaped 与 `DSH_*` ambient 变量，再叠加配置中显式 env；ACP/Claude/Codex 的 stderr 继承宿主日志，不进入结果，也没有 bounded stderr capture。上游运行时依赖分别是 ACP SDK 0.25.1、Claude Agent SDK 0.3.220、Claude SDK 0.93.0，以及 dsh-sdk client；Codex 生产路径不依赖 `@openai/codex`，它仅出现在测试依赖中。[ACP provider](../../upstream/deepseek-harness/packages/subagent/subagent-acp/src/index.ts)、[ACP run](../../upstream/deepseek-harness/packages/subagent/subagent-acp/src/run.ts)、[Claude provider/run](../../upstream/deepseek-harness/packages/subagent/subagent-claude-code/src/index.ts)、[Codex provider/wire](../../upstream/deepseek-harness/packages/subagent/subagent-codex/src/index.ts)、[dsh-sdk provider/run](../../upstream/deepseek-harness/packages/subagent/subagent-dsh-sdk/src/index.ts)

#### 当前 Rust 差集与建议边界

当前 Rust 产品只注册 `native` 的 `NativeSubagentProvider`；该 trait 的 `start` 返回本地 `AgentHandle`，不是上游带有可选 `localAgent` 的进程外 run 契约。`subagents@1` bridge 只暴露 list/history/prompt/interrupt/delete，没有 provider 注册回调；兼容 `dsh-subagent` shim 只导出 descriptor 操作，不能据此宣称四个上游插件可直接挂载。[Rust provider trait](../src/subagent.rs)、[Rust native 注册](../src/host.rs)、[Rust subagent bridge](../src/bridge.rs)、[兼容 shim](../compat/host-modules/@deepseek-ai/dsh-subagent/index.js)。现有 SDK 服务端 slash-method 别名也不等于 ACP child 客户端或完整协议等价；这是两个方向的能力。

这不是把四条路径替换成 native `spawn`/`fork`：

- **ACP** 是可配置的通用 ACP child，需保留会话初始化、permission policy、流式内容折叠和进程树退出证明。
- **Claude Code** 是外部产品 CLI 适配器，不是 Anthropic/Claude 模型 provider；不能用内部 LLM route 代替。
- **Codex** 是外部 Codex app-server 子代理，不是 OpenAI 模型 provider；不能用 pi-ai/openai route 代替。
- **dsh-sdk** 是独立 Harness child，必须保留 child 自己的 provider/model/composition 边界；不能把它压扁成父进程内 agent。

**用户已确认 D06.2：ACP、Claude Code、Codex、dsh-sdk 全部保留，允许分批实现，四条分别验收；不能用 native spawn/fork 或内部模型 provider 替代。** 本次只确认范围，不授权实现、安装外部 CLI/SDK、登录或使用凭据，也不把源码证据算作运行通过。[INFERENCE] 在已批准 D05 JS 路径下可复用冻结上游适配器，由 Rust 持有生命周期与权限边界，但须补齐真实 provider/run 桥接；本节不预定具体实现顺序。

#### 验收边界（未执行）

1. 每条 provider 的配置缺失、不可进入 cwd、错误 executable/command、显式 credential env 与 ambient credential scrub 分别验证；不得静默使用服务器启动目录或父级未声明密钥。
2. 每条 provider 都拒绝四项不支持的 start capability；均没有 `prepareContinuable`，不承诺恢复或后续对话。上游通用工具仍可将一次性 run 放入 job（依赖 jobs 配置），这不同于 continuable；标准 Preset 的 Codex/Claude 工具默认禁用且关闭后台参数。父级 run id 与 child session/thread id 的命名空间必须分开。[工具能力与 job 门控](../../upstream/deepseek-harness/packages/subagent/tool-subagent/src/index.ts)、[标准 Preset](../../upstream/deepseek-harness/apps/cli/config/agent-presets/standard/agent.cordis.yml)
3. ACP 分别验收 allow/reject、无可允许选项、session 初始化失败、partial streamed text、各 StopReason 与 EOF/terminate/整树退出；不能把 permission callback 当作人工审批队列。
   - ACP 源码只转发文本并丢弃非文本 block，不应描述成图像支持；初始化不声明 fs/terminal 客户端能力，也未实现认证登录流程。不要将这些遗漏扩写为上游已有功能。
4. Claude 分别验收空/非 text/纯空白 prompt、无效或空 SDK result、iterator/CLI 失败、`AskUserQuestion`、`persistSession:false` 与取消清理；不能把宿主 Claude 配置能力写成 Harness provider 能力。
5. Codex 分别验收握手、ephemeral thread、错 thread/turn、final answer 选择、审批请求 fail-closed、context-window error、interrupt 与子进程提前退出；不能把拒绝响应写成父级 approval forwarding。
6. dsh-sdk 分别验收 child initialize、provider/model/maxTokens 配置、session event 折叠、无 turn/异常 turn-end、shutdown 超时与重复 dispose；child 配置和凭据不得越过进程边界成为父级隐式能力。

证据：[统一 provider 接口与能力](../../upstream/deepseek-harness/packages/subagent/subagent/src/types.ts)、[统一外部进程结算](../../upstream/deepseek-harness/packages/subagent/subagent/src/out-of-process.ts)、[ACP](../../upstream/deepseek-harness/packages/subagent/subagent-acp/src/index.ts)、[Claude Code](../../upstream/deepseek-harness/packages/subagent/subagent-claude-code/src/run.ts)、[Codex wire](../../upstream/deepseek-harness/packages/subagent/subagent-codex/src/wire.ts)、[dsh-sdk run](../../upstream/deepseek-harness/packages/subagent/subagent-dsh-sdk/src/run.ts)、[环境过滤](../../upstream/deepseek-harness/packages/subprocess/subprocess/src/index.ts)。本节四条路径范围已获确认，运行仍未验收；D06.3—D06.5 不随本项批准。

### Q05 首轮结果：D06.3 Web provider

2026-10-09：本节只调查冻结版 Harness 的 provider-neutral Web seam、DeepSeek/Exa/Perplexity search 及 HTTP fetch；未调用任何供应商接口、未读取用户密钥、未安装依赖、未修改运行代码。调查后用户确认完整保留三种 search provider 与 HTTP fetch；这只确认范围，不表示现有 Rust 或上游静态证据已经验收通过。结论先行：基础 `web_search`/`web_fetch` 是独立的能力层；search provider 和 fetch provider 不能互相替代，搜索结果也不能推导出浏览器、JavaScript 执行、登录态或任意远端访问能力。

#### 上游实际能力

| 能力 | 冻结版上游事实 | 边界 |
|---|---|---|
| Web seam | `ctx.web` 分开注册 search/fetch provider；执行时按显式 provider 选择，否则要求恰好一个可用 provider；工具启用与 provider 可用性分离，工具可以显示但在执行时返回结构化错误 | 不是“装了工具就有联网”；多个可用 provider 不会隐式猜选；provider-specific 配置不能写进公共 seam |
| DeepSeek search | Anthropic-compatible Messages `POST /messages`，固定 `web_search_20250305` server tool；请求 model、max tokens、max uses；从 text citations 和 `web_search_tool_result` 提取 URL/title/snippet/page age；支持运行时 credential resolver 与 secret-free request recording | 需要 DeepSeek key；不是通用网页抓取；当前冻结路径不提供 Exa/Perplexity 的参数或协议；请求取消、HTTP 错误、响应结构错误分别结算 |
| Exa search | `POST /search`，Bearer key；`auto`/`keyword`/`neural` search type、numResults、highlightsPerUrl；只保留带非空 highlight 的结果，返回 URL/title/snippet/published date | 没有生成式 answer；没有 highlight 的 Exa result 被丢弃；API key、合法 base URL 和正整数参数缺失时不可用 |
| Perplexity search | OpenAI-compatible `POST /chat/completions`，Bearer key；model、max tokens、可选 day/week/month/year recency；优先结构化 `search_results`，缺失时退化为 URL-only citations，也可返回 generated content | 不是 OpenAI provider 的等价实现；URL-only citation 没有 title/snippet；key、合法 URL、max tokens 缺失时不可用 |
| HTTP fetch | 匿名 `GET`，只接受绝对 HTTP(S) URL、无 URL credentials；手动跟随同源 redirect 并限制跳数；限制 URL/response bytes/body chars，按 HTML/text 与 charset 解码，保留非 2xx 为数据 | 明确**没有** private-network/SSRF 地址阻断；没有 cookies、ambient credentials、浏览器 session、JS 执行或 DOM 浏览；不能把此 provider 暴露到可触达敏感内网的部署 |

工具层另外负责参数非空校验、search source 上限、fetch 输出上限、超时、取消、Markdown/HTML 展示及 replay metadata。`web_fetch` 将 HTML 通过 Turndown/GFM 转为模型文本并移除 script/style/noscript；这只是静态内容转换，不是浏览器渲染。Search 结果的 `content`（Perplexity 可提供）与 source citations 是不同字段，不能在只保留 sources 的实现中宣称完整等价。[Web seam types/runtime](../../upstream/deepseek-harness/packages/web/web/src/types.ts)、[Web tool config](../../upstream/deepseek-harness/packages/web/tool-web/src/index.ts)、[search tool](../../upstream/deepseek-harness/packages/web/tool-web/src/search.ts)、[fetch tool](../../upstream/deepseek-harness/packages/web/tool-web/src/fetch.ts)

HTML 转换超深或失败时，上游会回退原始 HTML；移除 script/style/noscript 是正常转换路径的行为，不是对任意原始内容的通用净化保证。通用 spill policy 与 Web provider 分层，不能用 Web 自带截断宣称已实现所有 spill 契约。`available()` 只表示本地配置条件满足，不证明凭据有效、供应商可达或已经付费启用。

凭据来源并不统一：DeepSeek 每次操作读取 settings 快照并解析当前凭据；显式非空 `apiKey` 优先，有 credentials service 时使用指定 `apiKeyEnv` 引用，没有该服务才读同名 launch environment。Exa/Perplexity 在插件加载时取 `config.apiKey ?? launchEnvironmentOf(ctx).get(...)`，分别对应 `EXA_API_KEY`/`PERPLEXITY_API_KEY`，显式空字符串不会回退，且不使用 DeepSeek 的 managed credential store。保留这些冻结行为，不应把三个 provider 全部概括为相同的热凭据机制。[DeepSeek 注册与快照](../../upstream/deepseek-harness/packages/web/web-search-deepseek/src/index.ts)、[Exa 注册](../../upstream/deepseek-harness/packages/web/web-search-exa/src/index.ts)、[Perplexity 注册](../../upstream/deepseek-harness/packages/web/web-search-perplexity/src/index.ts)

#### 当前 Rust 差集

当前 Rust 已有 provider-neutral `harness.web@1` registry：运行时从 `DSH_WEB_SEARCH_PROVIDER`/`DSH_WEB_FETCH_PROVIDER` 读取显式 provider，搜索默认上限为 8；Host 注册 DeepSeek search 与 `http` fetch。[Rust Web seam](../src/web.rs)、[Host 注册](../src/host.rs)

当前 Rust 已实现的能力是：DeepSeek Anthropic-compatible search；请求参数、凭据缺失、取消、响应错误和 citation/source 映射；HTTP fetch 的 HTTP(S)/无 credentials 校验、同源 redirect、15 秒 timeout、1 MiB body、262144 字符上限及 HTML/text body 区分。但 Rust Web result 只保留 `sources`/`truncated`，没有上游 Perplexity `content` 字段；Rust fetch 返回原始 HTML/text body，不是上游 tool-web 的 Markdown/DOM 转换结果。Rust 注释和实现同样明确不阻断 private-network 地址；这是待部署层解决的安全边界，不是已完成的 SSRF 防护。

已有路径也未完全等价：① Host 注册的 `web-search-deepseek` settings 只接受 apiKeyEnv/baseURL/maxUses，但实际 provider 构造读取 `DEEPSEEK_SEARCH_*` 环境并固定字段，没有接入该 settings 快照；UI 保存成功不表示下一次搜索生效。② Rust fetch 没有上游 URL 长度限制、MIME 白名单/charset 解码及 Content-Length 超限先拒绝行为；它把非 HTML 响应按 UTF-8 lossy text 处理。③ Rust 工具直接把 HTML 拼入输出；确有全输出截断和 URL/status/truncated metadata，不是完全缺失工具层。④ 上游 source limit 是每次请求可选参数，Rust 额外强制全局默认上限 8；不能只比较模型工具默认值。以上是静态调用路径差集，不是本轮运行失败报告。[Rust 模型侧 Web 工具](../src/builtin_tools/model_tools.rs)

未发现 Rust 的 Exa 或 Perplexity provider 注册、请求实现、provider-specific credential/config 或对应 tool output 语义。因此“已有 DeepSeek search + HTTP fetch”不能推导为 Exa/Perplexity 已迁移，也不能推导为完整 Web/浏览器能力。

[INFERENCE] 可由 Native 或已批准的 Legacy JS 路径承接，但当前 bridge 没有 Web search/fetch provider 注册/调用代理，host-modules 中也没有 `dsh-web` shim；现有 `webListener@1` 是入站 Web 路由，不是 `ctx.web` 出站搜索服务。不能将“允许 JS 插件”写成“这几个插件已经可直接运行”；每项执行归属应唯一，保留 scope 生命周期与取消。[Node domain bridge](../src/bridge.rs)

#### 建议与验收边界

**建议 D06.3 完整保留冻结版的三种 search provider（DeepSeek、Exa、Perplexity）与匿名 HTTP fetch，以及共同的注册/选择、配置/凭据、结果呈现、错误/取消契约；允许 Native/Legacy 分工与分批验收。** 基础 search/fetch 已在范围内，本次需要确认的是完整 provider 清单，不是重新削减基础范围。没有可用凭据时将真实供应商验收标记“未验收”，不自动删除供应商；若用户明确选择仅 DeepSeek，则把 Exa/Perplexity 延后或排除分别记录。保留的是冻结适配器已有参数，不额外扩展 Exa/Perplexity 全部 API、浏览器登录、JS 执行或远程执行环境。

验收场景（未执行）：① 无 provider、显式 provider 缺失、provider 不可用和多个 provider 歧义分别返回稳定错误；② search 空 query、source cap、取消、HTTP/JSON 错误与 citation 去重；③ DeepSeek key 的 credentials resolver、命名引用缺失不 ambient 回退、request recording 不含 secret；④ Exa keyword/neural/highlight 丢弃规则；⑤ Perplexity search_results 优先、citations fallback、answer/content 与 sources 分离；⑥ fetch 非 HTTP(S)、URL credentials、非法 charset/content type、非 2xx、同源/跨源 redirect、redirect cap、byte/character cap、timeout/cancel；⑦ 对内网/private IP 的风险在实际部署网络策略中单独验收；⑧ HTML 转 Markdown、脚本移除、表格与截断只在 tool 层验收，不能用原始 HTTP body 通过替代。

补充验收（未执行）：DeepSeek settings 更新在下一次请求生效且单次请求不混用新旧 endpoint/key；Exa/Perplexity 显式空 key 与 launch env 优先级；HTTP charset、MIME、Content-Length 声明超限与流式超限的不同结果；provider 注销/替换及在途取消；tool enablement/timeout/output cap、可回放 presentation metadata 与通用 spill 分层。安全基线承认冻结实现无 SSRF 地址阻断，不将该缺口列作应复刻的安全保证；敏感内网部署必须另有有效出站策略，否则不得启用。

证据：[Web seam](../../upstream/deepseek-harness/packages/web/web/src/index.ts)、[DeepSeek provider](../../upstream/deepseek-harness/packages/web/web-search-deepseek/src/provider.ts)、[Exa provider](../../upstream/deepseek-harness/packages/web/web-search-exa/src/provider.ts)、[Perplexity provider](../../upstream/deepseek-harness/packages/web/web-search-perplexity/src/provider.ts)、[HTTP provider](../../upstream/deepseek-harness/packages/web/web-fetch-http/src/provider.ts)、[HTTP policy](../../upstream/deepseek-harness/packages/web/web-fetch-http/src/policy.ts)、[默认部署组合](../../upstream/deepseek-harness/packages/bundle/base/cordis.patch.yml)。本节 D06.3 完整 provider 范围已获确认，运行仍未验收；默认启用策略与完整范围分开记录；D06.4-D06.5 不随本项批准。

### Q06 首轮结果：D06.4 E2B 远端执行环境

2026-10-09：本节只调查冻结版 Harness 的 E2B provider-composition POC；未安装 `e2b`、未读取 `E2B_API_KEY`、未创建远程沙箱、未执行远程命令、未修改运行代码。结论：E2B 不是“远程访问 Web UI”，而是把一个**共享的远程 Linux 执行世界**提供给 filesystem、subprocess 和 terminal adapter；它不迁移 Harness/Cordis 进程、LLM、Agent/Session、持久化、Skills 或高级协议。

#### 上游实际边界

| 层 | 冻结版 POC 行为 | 不包含 |
|---|---|---|
| `dsh-e2b` | 创建一个 E2B sandbox，准备绝对 POSIX `cwd` 与私有 `.dsh-e2b` runtime root；adapter 共用同一 SDK handle；timeout 或 dispose 删除 sandbox | 不提供 workspace 同步、部署平台、沙箱发现、网络出口策略；`cwd` 是解析约定，不是路径 containment |
| `fs-e2b` | 将 provider-neutral filesystem seam 映射到 E2B Filesystem API；远程 read/write/edit/list、版本信息、路径解析、取消和错误映射都在同一 sandbox | 不等于 Host 本地 filesystem；二进制、UTF-8、symlink、大小、canonical path 和远程错误必须分别验收 |
| `subprocess-e2b` | 通过 E2B Commands/PTY 实现远程 executable lookup、进程组、stdio、输出/状态路径和 terminal；进程状态存放在 runtime root | 不使用本机 `spawn`；远程 process/PTY 的退出、轮询、kill、spill 和 service dispose 不能由本地进程测试替代 |

共享 owner 的生命周期是关键契约：构造阶段开始 sandbox 创建；创建 `cwd` 与 runtime root，确认 runtime root 是真实目录而非 symlink，并以受限 mode 初始化；释放先阻止新的 handle 获取、等待 setup，再 kill sandbox。已过期或被其他 owner 删除的 `SandboxNotFoundError` 视为已停稳。E2B API key 只用于宿主 SDK 连接，不进入 sandbox；每个内部 login shell 使用新的随机 `HOME`，避免读取可变用户 profile。固定依赖为 `e2b@2.29.1`。[E2B family 说明](../../upstream/deepseek-harness/packages/e2b/README.md)、[shared owner](../../upstream/deepseek-harness/packages/e2b/e2b/src/index.ts)、[filesystem adapter](../../upstream/deepseek-harness/packages/e2b/fs-e2b/src/index.ts)、[subprocess adapter](../../upstream/deepseek-harness/packages/e2b/subprocess-e2b/src/index.ts)

上游默认 bundle **没有启用完整 E2B 组合**；它把 E2B 标为 POC，默认 product path 仍使用本地 execution world。E2B fixture/e2e 需要 `E2B_API_KEY`，并在真实沙箱中验证 filesystem、subprocess、PTY、secret scrubbing 和 teardown；静态源码、mock SDK 或本地命令成功都不能证明远端路径可用。[默认 bundle](../../upstream/deepseek-harness/packages/bundle/base/cordis.patch.yml)、[E2B live composition](../../upstream/deepseek-harness/packages/e2b/e2b/tests/composition.e2e.ts)

#### 当前 Rust 差集

当前 Rust Host 注册本地 `Filesystem`、本地 `SubprocessRuntime` 与 OS sandbox policy；`RemoteAccess` 是配对/隧道/远程 UI 访问控制，不是远程 filesystem/process execution world。未发现 E2B SDK 依赖、sandbox owner、远程 FS adapter、远程 subprocess/PTY adapter、E2B provider 注册或 `E2B_API_KEY` 处理。[Rust filesystem](../src/filesystem.rs)、[Rust subprocess](../src/subprocess.rs)、[Rust sandbox](../src/sandbox.rs)、[Rust remote access](../src/remote_access.rs)、[Host 注册](../src/host.rs)

因此：① Remote Access 不能作为 E2B 替代；② 本地 sandbox 不能声称远程 Linux 隔离；③ 为了“支持 E2B”只增加 API key/config 没有意义；④ 需要先定义 Rust execution-world seam，再实现一个共享 sandbox owner 与两个 adapter，不能给 bash、terminal、LSP、spill 各写一套 E2B 特例。E2B POC 还明确不提供 network policy，因此纳入范围不应隐含“远程网络安全”或“公网/内网访问保证”。

#### 建议与验收边界

建议 D06.4 **保留 E2B POC 范围但延后交付**，不自动排除；用户已确认此边界。它是可选远端执行世界，不是当前基础迁移的前置条件。实施前先冻结 provider-neutral filesystem/subprocess/terminal/spill 契约和 Rust owner 生命周期，再将 E2B 作为组合适配器接入。没有 E2B 凭据时只能做 mock/协议验收，不能宣布真实远端支持。

验收场景（未执行）：① 缺失/显式错误 API key、绝对 POSIX cwd、timeout 与 sandbox 创建/初始化 rollback；② 同一 owner 下 fs、commands、PTY 看到同一 cwd/runtime root；③ runtime root symlink/file、mode、canonical path、binary/UTF-8、权限和大小错误；④ executable lookup 的绝对路径、PATH、相对路径拒绝与 shell quoting；⑤ stdout/stderr/stdin、输出上限、远程 spill、进程组退出、interrupt/terminate、PTY resize/signal；⑥ caller cancellation 与 owner dispose 竞态、setup 期间 dispose、timeout 删除、重复删除和 `SandboxNotFoundError`；⑦ `E2B_API_KEY` 只留在宿主连接，不能由 command、PTY、spill 或诊断读出；⑧ sandbox 网络策略、workspace 同步/发现和 `cwd` containment 单独记录，不从 E2B SDK 默认行为推导安全承诺；⑨ 无 E2B 凭据时隔离 mock 与 live acceptance，后者必须使用固定版本真实 E2B。

证据：[E2B POC 边界](../../upstream/deepseek-harness/packages/e2b/README.md)、[owner lifecycle](../../upstream/deepseek-harness/packages/e2b/e2b/src/index.ts)、[fs adapter](../../upstream/deepseek-harness/packages/e2b/fs-e2b/src/index.ts)、[subprocess adapter](../../upstream/deepseek-harness/packages/e2b/subprocess-e2b/src/index.ts)、[E2B tests](../../upstream/deepseek-harness/packages/e2b/e2b/tests/e2b.spec.ts)。本节 D06.4 已获“保留但延后”确认，运行仍未验收；D06.5 不随本项批准。

### Q07 首轮结果：D06.5 Identity/Telemetry

2026-10-09：本节为静态源码调查与范围讨论；未启用上报、未读取用户身份文件或凭据、未连接 collector、未修改运行代码。调查后用户选择“仅保留本地”和“排除模型关联头”，最终范围见下文；D06.1 的 LLM 能力保留不包含此次明确排除的关联发送。

#### 上游能力与数据边界

| 能力 | 冻结版实际行为 | 决策影响 |
|---|---|---|
| 本地反馈 | `/feedback` 写入 `feedback/record`，不启动模型工作；确认文本披露 sharing 状态，不承诺落盘或送达。消息赞踩/备注另存 storage-domain sidecar，不是同一个 feedback 事件 | 保留本地反馈不要求开启上报；消息赞踩不能自动视为上传会话的授权 |
| 关联身份 | harness home 下 `.anonymous-user-id` 持久化随机 UUID；不是登录账户，也不从硬件/网络信息生成；同一 home 的进程共用，删除后下一次启动重新生成 | 随机标识仍可关联多次请求，不等于内容匿名化；与 Rust 现有反馈文档内的 UUID 存储不相同 |
| `DISABLED` | 默认模式；不构造 coordinator、OTel provider、processor、exporter，反馈留在本地 | 保留 exporter 能力不等于默认启用 |
| `FULL` | 投影后的会话事件与运维事件交给 OTel SDK，使用 OTLP/HTTP logs | 不是只有计数/耗时；正文可能包含提示词、用户消息、工具参数/输出、文件内容和本地路径 |
| `FEEDBACK_ONLY` | 已提交的 `feedback/record` 触发截至该事件的尚未交接会话日志片段回放；后续事件等待下一次反馈；fork/resume seed 边界另受 coordinator 游标约束 | 不是“只上传反馈文字”；独立伪造总线事件不能授权上传，直接 emit 也不能绕过门控 |
| DeepSeek 请求身份 | 独立模型 adapter 将同一个 UUID 放入 `x-deepseek-harness-user-id`；有 sessionId 时还发送 `x-deepseek-harness-session-id`，发送至解析后的 baseURL，包括自定义网关 | 遥测关闭不关闭这条身份发送路径；必须单独决定是否继承 |

证据：[本地命令反馈](../../upstream/deepseek-harness/packages/feedback/command-feedback/src/index.ts)、[消息反馈](../../upstream/deepseek-harness/packages/feedback/message-feedback/src/index.ts)、[身份生成](../../upstream/deepseek-harness/packages/identity/anonymous-user-id/src/index.ts)、[身份消费边界](../../upstream/deepseek-harness/packages/identity/anonymous-user-id/README.md)、[OTel 模式与授权](../../upstream/deepseek-harness/packages/session/session-telemetry-otel/src/index.ts)、[回放与脱敏](../../upstream/deepseek-harness/packages/session/session-telemetry/src/coordinator.ts)、[DeepSeek 请求头](../../upstream/deepseek-harness/packages/llm/llm-deepseek/src/adapter.ts)。

上游脱敏 waterfall **不自带脱敏规则**，没有规则时交接原始捕获内容；规则抛错会扣留对应记录。`emit` 表示交接而非送达，SDK 负责批处理/重试，不能承诺崩溃不丢失。插件在上传模式要求显式 http(s) endpoint；默认 bundle 则预填 DeepSeek collector URL，但 mode 默认关闭。CLI 将任何非空 `DSH_TELEMETRY_DISABLED`（包括 `0`/`false`）作为禁用该遥测插件的最高优先级 patch；它不禁止 DeepSeek adapter 的身份头。[默认 bundle](../../upstream/deepseek-harness/packages/bundle/base/cordis.patch.yml)、[CLI 关闭开关](../../upstream/deepseek-harness/apps/cli/src/profile-boot.ts)

#### 当前 Rust 差集

Rust 已有 `TelemetryCoordinator`、可注入 `TelemetryBackend`、redactor、有界队列，以及 Host 事件捕获/关闭接线，HostConfig 默认 `telemetry: None`。源码中 `sharing()` 只读取 backend 状态；coordinator 的 capture/enqueue/drain 没有按 Disabled/FeedbackOnly 执行上游授权门控或反馈时 canonical-log 回放，因此不能用枚举存在或确认文本相同证明三种模式等价。检查 `src`、`compat` 与 Cargo manifest 未定位到生产 OTLP backend、OTel 依赖或 DeepSeek 身份头实现；这不是对任意外部注入 backend 的保证。[Rust coordinator](../src/telemetry.rs)、[Host 配置、反馈、事件接线](../src/host.rs)、[依赖](../Cargo.toml)

Rust `/feedback` 已写入会话事件，并从本地 MessageFeedbackDocument 持久化/读取 UUID；该路径不等于上游 home 身份文件迁移或跨 provider 共享已完成。本节只登记差集，不据此修改现有逻辑，也不声明当前默认 Host 正在外发数据。

#### 用户决定与验收边界

用户已确认：① 保留本地日志、`/feedback`、消息赞踩/备注及本地身份用途；② **明确排除 OTLP 导出**，不迁移 FULL/FEEDBACK_ONLY 上报、exporter 或上游采集目的地；③ **明确排除模型关联头**，不迁移 DeepSeek 的 `x-deepseek-harness-user-id` 与 `x-deepseek-harness-session-id`，包括自定义网关。不是“保留但默认关闭”，也不是“延后交付”。该项是批准的上游行为差异，不撤销 D06.1 其余模型协议与鉴权能力；正常模型请求所需消息和凭据发送不属于本项排除范围。

此前建议保留可选 exporter 与显式启用关联头，用户未采纳，以本次选择为准。未来如要重新引入导出或模型关联发送，需另行确认范围、接收端及隐私边界；本次不为此预留交付承诺。现有可注入 telemetry 接口与 Legacy 插件如何遵守本地边界，需要在实施清单中核对，不能凭默认 None 宣称全路径已经满足；本次不删除或修改任何运行代码。

验收边界（未执行）：本地日志/反馈正常持久化与恢复；反馈不触发上传，确认文本不声称已共享或已送达；身份本地隔离/重置与迁移不破坏反馈数据；默认配置、旧配置/环境变量及 Native/Legacy 路径不得隐式恢复被排除的遥测导出；正式请求捕获验证 DeepSeek 官方地址和自定义网关均无上述两个关联头，同时正常模型消息、鉴权与流式调用保留。对不支持的上报配置应明确说明或拒绝，不能静默声称 FULL/FEEDBACK_ONLY 可用。验证需隔离临时 home 与本地捕获端，不使用真实用户日志、凭据或未经授权的远端 endpoint；本次静态调查不等于上述验收通过。

### Q08 首轮结果：D07 多语言 WASM 交付承诺

2026-10-09：仅核对本地源码、示例、ABI 文档与 CI 配置；未安装语言工具链、未编译 Guest、未执行 WASM 或修改运行代码。用户选择“四语言保留，分批交付”；这确认最终范围和顺序，不代表任何语言已验收或授权立即实现。

| 语言路径 | 当前可定位证据 | 不能据此宣称 |
|---|---|---|
| Rust WASM | `tessivum-pdk` 定义 envelope、五个生命周期导出及 Host Functions 包装；`wasm-minimal` 是 cdylib Guest；Windows CI 配置真实 wasm32 编译及 runtime-smoke 加载/卸载 | CI 文件不是本轮执行结果；minimal Guest 不调用 Host Functions，runtime-smoke 不覆盖每个导出/错误/取消场景，不能把这些文件视为完整语言验收 |
| TypeScript/JavaScript WASM | 有 TS 示例和 `@extism/js-pdk` 开发依赖，但 package scripts 只有 typecheck；示例响应携带顶层 `ok` 字段 | Host 的 `ResponseEnvelope` 使用 `deny_unknown_fields` 且没有 `ok`；按当前源码，该响应会被拒绝为 `PROTOCOL_INVALID`。不是可直接交付的 ABI 示例，也未定位到对应编译/真实加载 CI |
| Go WASM | 本次 Core Guest 文件清点未找到 Go 源文件或 go.mod | 不能把 Extism 多语言路线视为本项目已支持 Go；需要确定工具链、产物和共同 ABI 场景 |
| Python WASM | Core 中定位到的 Python 文件是 benchmark 驱动，不是 Guest；未找到 Python Guest 工程 | 不承诺普通 Python 源码、任意 pip 包或 C 扩展直接进入 WASM；工具链与运行时限制需要实际验证 |

证据：[Rust PDK](../../tessivum-core/crates/tessivum-pdk/src/lib.rs)、[Rust Guest](../../tessivum-core/examples/wasm-minimal/src/lib.rs)、[CI](../../tessivum-core/.github/workflows/ci.yml)、[runtime-smoke](../../tessivum-core/examples/runtime-smoke/src/main.rs)、[TS Guest](../../tessivum-core/examples/wasm-typescript/src/index.ts)、[TS package](../../tessivum-core/examples/wasm-typescript/package.json)、[Host envelope 与引擎](../../tessivum-core/crates/tessivum-extism/src/lib.rs)、[ABI 文档](../../tessivum-core/docs/PROTOCOLS.md)。ABI 文档中“忽略未知字段”的描述也与当前 Host 严格反序列化不一致，后续应与 TS 示例一起修正；本轮只登记，不自动修复。

**不混淆三条路径：** Rust 原生插件不等于 Rust WASM Guest；D05 已批准的新旧 JS/npm 插件走 Legacy 运行时，不因 D07 选择而删除；TS/JS WASM 是另一个编译产物路径，不透明提供 Node API、任意 npm 或 Browser API。Host 当前禁用 WASI 与默认网络访问，只提供显式注册/授权的 Host Functions；不能为某种语言编译通过就自动放宽这些安全边界。

用户已确认 D07：Rust、TypeScript/JavaScript、Go、Python 四种语言全部保留为 WASM 交付目标；按 Rust → TypeScript/JavaScript → Go/Python 分批交付。每种语言以真实产物通过共同 ABI、Host Functions、错误、取消、资源限制和生命周期场景后才标为已验证支持；Go/Python 延后而非排除。D05 的 Legacy JS/npm 插件路径与 TS/JS WASM 路径继续分开，不承诺任意 Node/pip API 透明进入 WASM。

共同验收边界（未执行）：固定工具链/PDK 版本和可复现产物；相同 `cordis.plugin/v1` manifest 与 request/response；五个生命周期导出；Host Functions 授权成功/拒绝与资源撤销；Guest 错误/非法 envelope/陷阱；超时、取消、内存与输出上限；更新/卸载清理。每种承诺语言使用真实产物验证，不以 Rust/WAT 或 mock Guest 的结果代替；语言标准库与依赖兼容面分别记录。选择范围不授权安装依赖、改代码或放宽沙箱。

### Q09 首轮结果：D08 Web 产品差异与源码复用

2026-10-09：本节核对现有 Tessivum 产品计划、兼容基线、Web E2E 清单与源码 patch；未修改 UI、未运行 Browser、未重放用户数据。调查确认当前产品采用上游 Web 源码加 Tessivum 差异，而不是逐行恢复上游首次使用行为。

| 差异 | 当前记录 | 范围归属 |
|---|---|---|
| 中立首屏 | 无 Key 可进入工作台，不主动弹 DeepSeek 配置；可编辑文本/图片草稿 | D08 产品差异；不改变 LLM 鉴权要求 |
| 显式模型选择 | 不隐式注册/选择 DeepSeek；新会话只继承用户明确保存的默认；恢复会话保留已保存选择 | D08 产品差异；发送前 Host 仍必须验证可路由模型 |
| 图片草稿 | 无模型、纯文本模型或未知能力时仍可选择/粘贴/拖放；不支持发送时保留草稿 | D08 产品差异；不自动增加 OCR、视觉中继或跨服务转发 |
| 模型能力 wire | `session.models.current` 可为 `null`；模型可声明 `inputModalities`；客户端和 Rust schema 同步 | D08 产品差异；未选模型不是虚构 provider/model |
| 远程首屏/终端 | 远程首屏可编辑草稿；远程终端继续拒绝，legacy 终端路由限本机 | Remote Access 产品/安全边界；不把终端拒绝混成 Web 原样兼容 |

历史记录明确称上述行为是 Tessivum-owned 或 Alpha.26 有意差异；Web 清单保留受影响的上游场景入口，使用 Tessivum 断言替换行为，不通过删除测试缩小基线。现有检查脚本也要求上游 E2E inventory 与这些替换入口保持对应关系，并将 Market、Remote Access、image-input、history-recovery 作为额外第一方场景。证据：[产品计划](./DEVELOPMENT_PLAN.md)、[兼容基线](./COMPATIBILITY_BASELINE.md)、[Web E2E 清单](./WEB_E2E_PORT_CHECKLIST.md)、[Web patch](../web/patches/deepseek-harness.patch)、[兼容检查](../scripts/check_compat_baseline.py)。

采用方案 B 后，兼容声明必须分成两层：① 上游协议、wire schema、Web boot graph、加载/卸载、持久状态和未变更 UI 场景的兼容；② 上述 Tessivum 产品差异的独立行为契约。品牌变化、UI 文案或布局变化不能掩盖协议变化；产品差异也不能被描述为上游原样行为。新增差异不得沿用本次批准，必须单独记录影响、替代关系和验收。

验收边界（未执行）：全新目录无 Key 进入工作台且可编辑草稿；发送前未选模型稳定拒绝并保留草稿；配置并明确选择模型后同一草稿成功发送；新会话默认只来自显式保存，恢复会话不漂移；文本/图片/未知能力提示与 Host admission 一致；远程首屏、设备撤销、loopback 终端拒绝和 WebSocket 清理分别验证；原始未改动场景继续使用真实上游 Web、HTTP/WebSocket wire、Chromium、pageerror/console 监测；每个意外差异进入清单，不删除断言规避。

用户已确认 D08 方案 B：保留逐项记录的 Tessivum Web 产品差异；本项确认范围，不授权恢复、重写或重新验收 UI。

### Q10 首轮结果：X01—X05 产品扩展边界

2026-10-10：本节核对现有 Phase 7—10 计划、开发计划、性能报告、Web/Remote 边界和发行记录；未启动 Market mutation、Remote Access、安装器或性能基准，未修改产品代码。既有“已完成”是历史实施状态，不自动等于本次两阶段迁移范围继续承诺；本节只为 X01—X05 选择提供证据。

| 项目 | 已有事实 | 当前不能推导 |
|---|---|---|
| X01 Market | Tessivum-owned Market、Profile mutation、精确版本状态、verified/unverified/revoked、Host-owned restart、安装/更新/卸载/失败回滚及一个社区插件验证闭环已有独立计划和报告 | 社区目录收录不等于插件兼容；市场安装不等于任意 npm 包安全；不应复制完整社区 Registry，也不应把 Market 自动并入 Rust Core 基础契约 |
| X02 Remote Access | Rust-owned pairing/device/revocation、HTTP/SSE/WebSocket authority、Cloudflare Quick Tunnel 和最小配对页已有 Phase 8 记录；默认关闭、loopback 与 exact authority/Origin/Tunnel 边界保留 | 远程 Browser 可用不等于远程 filesystem/process/PTY；远程 legacy HTTP/WebSocket 路由与侧栏终端继续拒绝；不能把 X02 当作 D06.4 E2B |
| X03 OS/发行 | macOS/Linux 发行与归档有历史证据；Windows x86-64 runtime/ACL/PowerShell 仍在 Phase 10 收尾，安装器与正式发行门槛未完成；Windows ARM64 与远程 Windows Quick Tunnel 明确不支持 | 源码编译、CI 绿灯或单平台安装不能推出完整跨平台支持；不能承诺 Windows 安装、签名、升级、卸载或 ARM64，除非各自有真实包内证据 |
| X04 性能 | 固定 Linux VM 上已有 30 样本 Core/产品报告，包含优势、Legacy readiness 回归、Core `loader_update` 回归和完整清理结果；报告明确不代表生产 LLM 延迟、模型质量、裸机 Linux 或数万 Agent 饱和能力 | 历史 benchmark 不能变成未来 SLO；“低内存/数万 Agent”愿景没有当前证据；新增阈值必须先固定 workload、平台、统计和回归门槛 |
| X05 新功能 | 范围草案已将新模型协议、后续上游功能、自有 UI/模式增强列为冻结基线外的独立目标 | D01—D08 的批准不包含新增功能；不能以兼容迁移、性能优化或产品差异名义顺带扩张范围 |

用户已确认 X01—X05：X01 保留第一方 Market 独立轨道；X02 保留 Rust-owned Remote Access 独立安全轨道；X03 保留现有平台目标并按真实证据分阶段验收，Windows x86-64 继续收尾、Windows ARM64 不承诺；X04 只保留可复现历史 benchmark 与回归基线，不新增未经测量的性能承诺；X05 默认排除冻结基线外的新模型协议、后续上游功能和自有 UI/模式增强。上述选择不阻塞基础 Rust Core/Harness 迁移，也不授权本轮实施。

证据：[Market 计划](./PHASE7_FIRST_PARTY_MARKET_PLAN.md)、[Remote Access 计划](./PHASE8_REMOTE_ACCESS_COMPATIBILITY_PLAN.md)、[性能计划](./PHASE9_BENCHMARK_ECOSYSTEM_PLAN.md)、[性能报告](./PHASE9_BENCHMARK_REPORT.md)、[Windows 发行计划](./PHASE10_WINDOWS_NATIVE_RELEASE_PLAN.md)、[总开发计划](./DEVELOPMENT_PLAN.md)、[产品计划](./PHASE3_PRODUCT_PLAN.md)。

## 5. 产品扩展单独列账

以下不是从迁移范围里删除，而是与上游兼容分开决策、实施和验收；不自动回滚现有能力。

| 编号 | 产品/交付事项 | 建议边界 |
|---|---|---|
| X01 | 已确认保留第一方 Market 独立轨道 | 保留精确版本验证、安装/更新/卸载/回滚与安全边界；不把任意 npm 兼容或社区目录收录自动并入 Rust Core 基础契约 |
| X02 | 已确认保留 Remote Access 独立安全轨道 | 默认关闭，保留 Rust-owned 配对、撤销、exact authority 与 Tunnel 边界；远程 Browser 不表示远程 filesystem/process/PTY，且不替代 D06.4 E2B |
| X03 | 已确认保留现有平台目标并分阶段验收 | macOS/Linux 按已有证据声明；Windows x86-64 继续独立收尾并分别验收运行、安装、升级、卸载和发行；Windows ARM64 不承诺 |
| X04 | 已确认只保留已测性能证据 | 固定 Linux benchmark 作为历史/回归基线；不追加生产 LLM 延迟、数万 Agent、单 Agent 内存或新 SLO 承诺 |
| X05 | 已确认默认排除冻结基线外新功能 | 新模型协议、后续上游功能、自有 UI/模式增强逐项另行批准，不借迁移、兼容或性能工作顺带扩张 |

关闭一个扩展不应使被承诺的基础能力失效；确有依赖时，应在批准范围中明示，不能用这句话隐藏当前实际依赖。

## 6. 验收口径草案

### 每个范围项独立记录四种状态

| 维度 | 可用状态 | 禁止的推导 |
|---|---|---|
| 范围 | 待确认 / 等价保留 / 接受替代 / 明确排除 | 已有代码不能推导为用户已纳入；延后不能推导为排除 |
| 批次 | 未排期 / 已确认批次 / 延后 | 进入批次不能推导为已授权实施 |
| 实现 | 未核对 / 已定位 / 部分覆盖 / 缺失 | 文件存在、路由登记或测试数量不能推导为实现完整 |
| 验收 | 未验收 / 历史证据 / 指定版本通过 / 指定版本失败 | 某旧版本/局部场景通过不能推导为当前全量通过 |

### 必须满足的共同要求

1. **数据路径**：真实持久层、事件和恢复，不用内存模拟替代声明支持的 JSONL/SQLite 行为。
2. **协议路径**：固定版本客户端或协议夹具检查请求、结果、通知、错误、取消；SDK smoke 不算 ACP 证明。
3. **执行路径**：真实工具/进程/插件运行；模型可使用确定性 replay 验证事件语义，但 replay 不替代供应商鉴权与真实协议接入验证。
4. **生命周期**：失败、超时、取消、重载、关闭和资源释放与正常路径同等重要。
5. **安全路径**：权限拒绝必须阻止副作用；Native、Legacy、WASM 与各 OS sandbox 的不同保证写清楚。
6. **浏览器路径**：真实 Rust Host、上游来源 Web、网络与持久状态；页面能打开不能替代完整交互。
7. **替代方案**：同时记录保留能力、丢失能力、配置/数据/插件迁移影响、运行时依赖和验收场景；收益仍为推断时不能写成实测结论。
8. **支持声明**：未具备等价实现的能力返回明确错误并标为不支持；拒绝行为正确也不能把对应上游功能记为迁移完成。

“完成”只针对用户批准的范围和指定版本证据。未决项不计入已完成，也不通过移出分母制造完成率。历史功能清单的勾选与本次范围批准分开管理。

## 7. 建议审阅顺序与批准格式

### 第一轮：方向与基础范围

已完成：2026-10-09 用户确认第 1 节总原则及 G01—G16 建议保留范围没有问题。此确认不包含 D01—D08 的方案选择，也不授权开始开发。

### 第二轮：替代与可选能力

对 D01—D08 逐项选择；信息不足的项目标“先完成 Qxx”，不要求猜一个方案。D06 必须按子项记录。历史已批准的产品差异通过 Q06 核对，不因沉默视为重新批准。

已确认记录：2026-10-09 用户接受 D05 方案 A，并明确 JS 插件路径也用于新插件开发，不仅兼容旧插件；随后在 D01 差集调查后接受 D01 方案 A，保留 Preset 能力，允许 Mode 作为内部表示；在 D03 差集调查后接受 D03 方案 A，保留临时 Host/Client 动态插件能力及显式信任、权限和审批边界；在 D02 差集调查后接受 D02 方案 A，保留完整 Workflow 编排及 Ralph 契约，允许执行环境改变；在 D04 差集调查后接受 B 加可选 A，原生配置保持数据/受限表达式，可信 JS 配置复用 Legacy 路径，保留配置生命周期与权限边界；在 D06.1 差集调查后确认保留冻结版 Harness 实际可达的完整 LLM 接入能力，允许 Native/Legacy 分工，不额外承诺自动 OAuth 登录；在 D06.2 选择“全部保留”，确认 ACP、Claude Code、Codex、dsh-sdk 四条外部子代理路径，允许分批实现且分别验收；在 D06.3 选择“完整保留”，确认 DeepSeek、Exa、Perplexity 三种 search provider 与 HTTP fetch，允许分批实现且分别验收，默认启用策略不等于范围排除；在 D06.4 选择“保留但延后”，确认 E2B 共享远端 fs/process/terminal execution world 留在范围内，但先于实现冻结 execution-world seam，且不把 E2B 当作基础迁移前置条件；在 D06.5 选择“仅保留本地”和“排除模型关联头”，保留本地日志/反馈/身份用途，明确排除 OTLP 导出与 DeepSeek 的持久 UUID/session ID 关联头；在 D07 选择“四语言保留，分批交付”，确认 Rust、TypeScript/JavaScript、Go、Python 均为最终 WASM 目标，交付顺序为 Rust → TypeScript/JavaScript → Go/Python，逐语言真实验收后才标为已验证支持；在 D08 选择方案 B，确认继续复用上游 Web 源码并保留逐项记录、逐项验收的 Tessivum 产品差异；在 X01—X05 分别选择保留 Market 独立轨道、保留 Remote Access 独立轨道、平台与发行分阶段验收、只保留已测性能证据、默认排除冻结基线外新功能。开发授权状态不变。

### 第三轮：冻结范围后再排实施

范围冻结后，才生成实施清单：每项包含目标契约、实际缺口、验收场景、依赖和交付批次。实现与正式发布仍需要各自明确授权。
本轮整理结果：[已批准范围实施清单](SCOPE_IMPLEMENTATION_CHECKLIST.md)。它只整理目标、缺口、依赖和验收入口，不授予实施或发布授权。

可用以下简短格式反馈，不必一次决定全部项目：

```text
总原则：已确认（如需修改，请明确指出）
基础范围：G01—G16 已确认（如需修改，请明确指出）
D01：A 已确认，保留 Preset 能力，Mode 可作为内部表示
D02：A 已确认，保留完整 Workflow 编排及 Ralph 契约，允许执行环境变化
D03：A 已确认，保留模型动态 Host/Client 插件能力及权限边界
D04：B 加可选 A 已确认，原生受限配置与显式信任的 Legacy JS 配置并存，保留配置生命周期契约
D05：A 已确认，包含用户新编写 JS 插件的支持目标
D06.1：已确认完整冻结 LLM 接入范围，允许 Native/Legacy 分工，不新增 OAuth 登录承诺
D06.2：已确认四条外部子代理路径全部保留，允许分批实现、分别验收
D06.3：已确认完整保留 DeepSeek、Exa、Perplexity search 与 HTTP fetch，允许分批实现、分别验收
D06.4：已确认保留 E2B POC 但延后交付，先冻结 execution-world seam
D06.5：已确认仅保留本地日志/反馈/身份；排除 OTLP 导出及模型关联头，不是默认关闭或延后
D07：已确认 Rust、TypeScript/JavaScript、Go、Python 四语言 WASM 目标，按 Rust → TS/JS → Go/Python 分批交付；逐语言真实验收后才标支持
D08：B 已确认，继续复用上游 Web 源码，保留逐项记录与验收的 Tessivum 产品差异
X01：已确认保留第一方 Market 独立轨道
X02：已确认保留 Remote Access 独立安全轨道，默认关闭且不替代 E2B
X03：已确认保留现有平台目标并分阶段验收，Windows ARM64 不承诺
X04：已确认只保留已测 benchmark 与回归基线，不新增未测性能承诺
X05：已确认默认排除冻结基线外新功能，逐项另行批准
下一步授权：仅继续调查 / 仅整理实施清单；X01—X05 范围已分别确认，实施仍需单独授权
```

上述格式只是审阅方式，不是预填决定；不作答的行仍待确认。“认可草案”若未明确处理未决选项，不视为全部推荐自动生效，更不视为开发或发布授权。

## 8. 证据入口

以下用于追溯基线和上一轮静态观察，不代表本文重新验收：

- [上游功能分组](../../upstream/deepseek-harness/packages/README.zh.md)
- [两阶段目标与仓库边界](DEVELOPMENT_PLAN.md)
- [既有兼容基线](COMPATIBILITY_BASELINE.md)
- [Web 场景及已有差异记录](WEB_E2E_PORT_CHECKLIST.md)
- [Harness vendored Cordis 修改记录](../../upstream/deepseek-harness/vendor/README.md)
- [Core 兼容政策](../../tessivum-core/docs/COMPATIBILITY.md)
- [Core 差分报告及 Loader 限制](../../tessivum-core/docs/CONFORMANCE_REPORT.md)
- [Core 安全与异步释放边界](../../tessivum-core/docs/SECURITY.md)
- [ACP 上游契约](../../upstream/deepseek-harness/packages/acp/acp/README.zh.md)与 [Rust SDK 分派](../src/sdk.rs)
- [LSP 上游操作](../../upstream/deepseek-harness/packages/lsp/README.md)与 [Rust LSP](../src/lsp.rs)
- [Rust Workflow 语法边界](../src/workflow.rs)
- [TypeScript Guest 示例](../../tessivum-core/examples/wasm-typescript/src/index.ts)与 [Extism Host envelope](../../tessivum-core/crates/tessivum-extism/src/lib.rs)

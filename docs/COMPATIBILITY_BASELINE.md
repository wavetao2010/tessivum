# DeepSeek Harness 兼容基线

本文冻结 Tessivum 的第一目标兼容面。它不是“行为近似”清单；除明确列为 Tessivum 扩展的部分外，源 Web 前端必须能把 Tessivum 当作同版本 DeepSeek Harness Host 使用。

## 1. 上游基线

- 仓库：`deepseek-ai/deepseek-harness`
- 版本：`0.1.0-rc.5`
- 本地基线提交：`47f943859bef60e4160492346772ded9b24f765a`
- 冻结日期：2026-08-20
- Web 入口：`apps/web/src/main.ts`
- Web 组合：`packages/bundle/web-app/cordis.patch.yml`
- 浏览器验收：`apps/web/tests/*.e2e.ts`（完整清单见 [`WEB_E2E_PORT_CHECKLIST.md`](WEB_E2E_PORT_CHECKLIST.md)）

升级上游版本时必须先单独更新本文、RPC/事件协议测试和 Web E2E 清单；不得在功能提交中顺带漂移版本。

## 2. 范围与完成定义

兼容完成同时满足：

1. 使用上游源码入口和上游 Client 插件组合构建 Web；不维护第二套 UI 实现。
2. 浏览器直接加载 Tessivum 生成的 boot graph，完成 workspace、session、模型、工具、设置、goal、subagent、feedback 等真实流程。
3. Host RPC、WebSocket 下行帧、Node 插件桥协议、错误码和取消语义与本基线一致。
4. 69 个上游 Web E2E 文件已逐项移植并通过普通模式；快照录制模式不能代替断言模式。
5. 每个浏览器场景均要求 `pageerror=[]`、受监控的 `console.warn/error=[]`，且 fixture/golden inventory 闭合。

不要求复刻 Node Host 内部实现、Cordis 的 TypeScript 数据结构或上游测试脚手架；要求的是相同的外部契约和用户行为。

## 3. 源 Web 与 boot graph

### 3.1 唯一前端

- 生产入口必须来自上游 `apps/web/src/main.ts` 及其源码依赖。
- `tessivum/web` 只允许保存 Vite/Bun 适配、别名和构建配置；不得复制或改写产品 UI。
- 不再以预编译 `@tessivum/web-app` bundle 作为兼容标准；该 bundle 只能作为迁移期间的临时对照物。
- 所有 `@deepseek-ai/*` 包固定到 `0.1.0-rc.5` 对应源码，不得混用别的 rc 版本。

### 3.2 `window.__DSH_BOOT__`

Host 必须在入口脚本执行前、作为 `<head>` 内第一个 script 注入唯一 graph：

```ts
interface WebBootEntry {
  id: string
  url: string
  rev: string
  inject?: string[]
  immediately?: boolean
}

interface WebBootGraph {
  rev: string
  entries: WebBootEntry[]
}
```

冻结规则：

- `id` 就是完整 npm package name；不能改写 `/`、`@` 或作用域。
- `url` 精确为 `/plugins/${id}/client.js?rev=${rev}`；路由必须支持 scoped package 的多段路径，例如 `/plugins/@deepseek-ai/dsh-client-runtime/client.js`。
- `rev` 是 bundle 内容的 12 位 SHA-1 前缀；graph `rev` 是序列化 entries 的 12 位 SHA-1 前缀。它们是内容一致性锚点，不是时间戳。
- 每行仅 `id/url/rev` 必填；`inject` 是 package-name 依赖边，`immediately` 是第一阶段 prefetch 标记。entries 数组的发布顺序不承载激活语义，激活顺序由 fiber 依赖等待决定。
- bundle 必须执行 `window.__ModuleLoader__.load({ id, factory })`，handoff id 与 graph row 完全相同；factory 通过同步 `require(packageName)` 解析依赖。
- Host 从当前 Loader entries 的 package `dsh.client` 声明动态组成 graph；新增上游 client 包不能要求修改 Rust 硬编码清单。声明畸形、bundle 缺失或依赖不可满足必须 loud fail。
- 页面缺少或拿到畸形 graph、未知 bundle id、rev 不匹配或不安全路径时必须失败，不能启动半套 UI 或回退到任意文件。
- 样式在 bundle 执行时归属到 `style[data-plugin=<id>]`，模块销毁/热更新时一并清理；Host 不手工拼接上游 CSS。


当前实现状态（2026-08-20）：上游 `apps/web/src/main.ts` / `AppWebEntry` 直接从固定源码构建；构建管道从 Web profile、`dsh.client.inject` 闭包和 Tessivum 的 in-page directory picker 组合出 38 个 package-name rows，调用上游 `build:lib:client`，逐包校验 `window.__ModuleLoader__` handoff id，复制原 manifest 与 bundle，并验证 source/staged SHA-1 和 Rust graph revision。Rust Host 默认只扫描这些 source-built artifacts；`web/package.json` 与 `bun.lock` 不再包含 registry `dsh-*` artifacts。此前 registry bundle 的 Chromium 启动证据不能替代这批新产物；必须由下方 69-file gate 重新观察。

## 4. Browser ↔ Host wire

### 4.1 四象限消息

```ts
type RpcResult<T> =
  | { ok: true; value: T }
  | { ok: false; error: { code: string; message: string; details: object } }

type ClientRequest  = { type: 'client-request';  rpcId: string; method: string; payload: unknown }
type ServerResponse = { type: 'server-response'; rpcId: string; result: RpcResult<unknown> }
type ServerRequest  = { type: 'server-request';  rpcId: string; method: string; payload: unknown }
type ClientResponse = { type: 'client-response'; rpcId: string; result: RpcResult<unknown> }
type RpcReceipt = { accepted: true } | { accepted: false; reason: 'not-pending' | 'bad-response' }
```

- Client 为 `client-request` 生成 UUID；HTTP `server-response` 必须回显同一 `rpcId`。
- Host 为每个 `server-request` 生成 `rpcId`；需要回答的 approval/question 在重放时复用原 id。
- `POST /api/respond` 只接受 `client-response`；迟到或重复回答返回 `not-pending`，畸形回答返回 `bad-response`。
- 业务失败走 HTTP 2xx 内的 `RpcResult.ok=false`；HTTP 状态只描述 carrier 失败。

### 4.2 上行 HTTP

- `POST /api/<method>`：JSON `ClientRequest`，返回 JSON `ServerResponse`。
- `POST /api/respond`：JSON `ClientResponse`，返回 JSON `RpcReceipt`。
- 默认最大请求体：`160 * 1024 * 1024` bytes。
- Client 断开必须取消正在运行的 RPC；取消不能留下运行中的 turn、pending response 或泄漏任务。
- privileged 方法沿用上游 trusted-host 判定；不能因兼容而放宽信任边界。

### 4.3 下行 WebSocket

源 Web 使用两个只下行 WebSocket，不使用 SSE：

- `/api/events.mux`
- `/api/events.host`

每条文本消息是一个完整 `ServerRequest`：`method === payload.type`。二进制帧或畸形 JSON 被 Client 丢弃并报告；Client 向下行 socket 发送消息时 Host 以 code `1008`、reason `downlink only` 关闭。

Mux payload 闭集：

- `session/event`
- `session/subscribed`
- `approval/requested`
- `approval/resolved`
- `question/requested`
- `question/resolved`
- `session/queue`
- `session/jobs`
- `session/projection`
- `stream/error`

Host payload 闭集：

- `host/session-added`
- `host/session-removed`
- `host/session-status`
- `host/agent-error`
- `host/workspace-changed`
- `host/workspace-removed`
- `host/workspace-order-changed`
- `host/archived-sessions-changed`
- `host/remote-event`
- `stream/error`

重连语义：重新打开两条 socket，并重新获取 history/list 基线；v1 的 `since` 不提供增量恢复保证。

## 5. Core RPC 面

`POST /api/<method>` 必须支持以下 52 个方法名及上游 `rpc-map.ts` 的请求、结果和错误 schema：

```text
agentPreset.copy
agentPreset.list
agentPreset.openDocument
agentPreset.read
agentPreset.remove
agentPreset.select
credentials.describe
credentials.set
credentials.unset
goal.clear
goal.complete
goal.create
goal.edit
goal.pause
goal.resume
host.createDirectory
host.describe
host.listDirectory
host.openPath
host.pickDirectory
llm.discoverModels
llm.models
llm.providers
session.attachment
session.cancel
session.create
session.fork
session.history
session.list
session.models
session.prompt
session.rename
session.search
session.selectModel
session.updateQueue
settings.describe
settings.mutate
settings.openDocument
settings.replace
settings.update
skill.list
subagent.history
subagent.interrupt
subagent.list
subagent.prompt
workspace.archiveSession
workspace.create
workspace.delete
workspace.insertBefore
workspace.insertSessionBefore
workspace.list
workspace.rename
```

“同名但字段不同”不算兼容。实现时必须从冻结上游生成的 codec/类型建立 contract tests，特别覆盖联合类型、可选字段、错误 details 和未知字段拒绝策略。

## 6. Typert Remote contributions

Gateway 还必须挂载以下生成式 Remote endpoints。wire endpoint 使用 `namespace/method`，请求 payload 为 `{ args: { ... } }`，Host 与 Client 均执行冻结 schema 校验。

```text
commands/list
commands/execute
goals/edit
goals/pause
goals/resume
goals/complete
goals/clear
goals/create
dynamicCordisRunner/undefineFromPanel
dynamicCordisRunner/runHostHalf
dynamicCordisRunner/getClientCode
dynamicCordisRunner/resolveRequestRun
dynamicCordisRunner/settleUserRun
dynamicCordisRunner/stopFromPanel
dynamicCordisRunner/syncInspectManifest
dynamicCordisRunner/resolveInspectQuery
dynamicCordisRunner/inventory
dynamicCordisRunner/reportRenderFailure
dynamicCordisRunner/reportClientGuardFailure
dynamicCordisRunner/invoke
pluginInventory/list
messageFeedback/list
messageFeedback/put
messageFeedback/delete
```

Remote contribution 的挂载生命周期、重复 endpoint/namespace 冲突、撤回后调用、取消和返回值验证必须与上游 Gateway 一致；不能把这些方法手工塞入 Core RPC map 后绕过 contribution 语义。

## 7. 转发 Host 事件

`host/remote-event` 仅允许以下 11 个事件，payload 保持 `{ event, args }`，不重命名、不投影：

```text
agent-preset/selected
commands/change
credentials/updated
cordis/request-run
cordis/request-run-resolved
cordis/dynamic-package
cordis/dynamic-retract
cordis/inspect-query
cordis/inspect-query-resolved
llm/adapters-updated
settings/document-updated
```

增加 allowlist 是兼容基线变更；任意 Cordis 事件透传到浏览器是安全缺陷。

## 8. LLM / Agent Loop / Replay 契约

LLM 与 Agent Loop 是独立兼容层，不得以“能生成文本”替代以下协议。

### 8.1 Provider-neutral 请求与流

- 一次 `GenerateOptions` 请求包含：`provider`、`model`、有序 `messages`，以及可选 `system`、`tools`、`temperature`、`maxTokens`、`stop`、`reasoningEffort`、`sessionId`、`purpose`；取消信号属于进程内控制面，不进入 JSON 持久化。
- `Message` 必须保留稳定 `id/role/content/source`。核心内容块为 `text`、`reasoning`、`image`、`tool-call`、`tool-result`，且 `ContentBlockMap` 可由插件合并扩展；tool call 使用 `{ id, name, arguments }`，tool result 以 `toolCallId` 关联，image 只持有验证后的 attachment ref。`notice` 是 message source form，不是 content block。
- 流块必须兼容 `block-start`、`text-delta`、`reasoning-delta`、`tool-call-delta`、`block-end`、`usage`、`finish`；块索引属于消息内位置，tool delta 必须保留字段 `id`，`finish` 后不得再产生任何块。
- 核心 `finish.reason.kind` 为 `stop | tool-calls | max-tokens | error | aborted`，但 `FinishReasonMap` 可由 adapter 合并扩展，消费者对未知值必须 fail loud 或显式降级；`error/aborted` 带结构化 `LlmFailure { message, code, status?, providerRetryAfterMs?, requestId? }`。
- `TokenUsage` 必填非负 `inputTokens/outputTokens`，可选非负 `cacheReadTokens/cacheWriteTokens/reasoningTokens`；各输入计数互斥。Provider 未给 usage 时整个 usage 可以缺省，不能把估算值伪装成上游事实。
- `LlmAdapter` 每次调用只做一次 provider 尝试；adapter 选择、请求构造和迭代中异常都由 `LlmRuntime` 规范化为唯一终止 `finish`，重试不藏在 adapter 内。

### 8.2 Agent Loop 持久化状态机

- 一个 Agent 只有一个活动 driver；公开 `AgentStatus` 只有 `idle | running`，dispose/factory failure 通过生命周期事件和错误报告，不伪造成第三种 status；每个活动 turn 拥有独立取消源。
- 正常边界按 `turn/start -> step/start -> ... -> step/end -> turn/end` 持久化。每个 provider chunk 必须先追加为 `assistant/chunk { turn, step, chunk }`，再送入内存 assembler。
- 每次成功的 LLM 尝试必须追加且只追加一个 `assistant/message` 完成锚点；其 `sourceEventSeqs` 指向该次所有 chunk，内容、source/replayState 和可选 usage 从流组装。即使内容为空或因 `max-tokens` 结束，也必须写完成锚点；空 assistant message 不进入下一次派生 prompt。
- `tool-calls` 结束时执行消息内全部工具调用并追加 `tool/call`、`tool/result`；若工具未声明终止 turn，则进入同一 turn 的下一 step。无 tool-call 的 `stop` 完成 turn；`max-tokens` 在后续 step 中保持 sticky，不能降级为 completed。
- Loop 正常退出写 `turn/end` 的 `completed | max-tokens | blocked | aborted | error`；持久化后端在 reload 时可用 `interrupted` 闭合 crash orphan，且 `TurnEndReasonMap` 可合并扩展。取消、provider 错误、工具错误或 hook 错误都不能留下半开的 turn/step。
- `agent/request` waterfall 在每次尝试前产生冻结配置，并按精确 `provider/model` 解析 adapter 默认值。`request/header` 仅在 `initial/resume/change` 时追加，`request/context` 仅在 provider/model/contextWindow 改变时追加；实际请求带完整派生消息和稳定 `sessionId`。
- queue/steer/next-step 输入、取消与恢复必须由单一 loop 状态机串行裁决；API handler 不得直接改写 JSONL 伪装成 Agent 执行。

### 8.3 Retry 与取消

- 重试通过 `agent/request-error` waterfall 插件实现；同一 `turn/step` 的失败尝试可重进 `buildRequest`，不得开启假的新 session 或隐藏失败前已经持久化的 chunks。
- 默认 normal policy：`maxRetries=2`；可重试 code 为 `EMPTY_RESPONSE/RATE_LIMIT/SERVER/TIMEOUT/TRANSPORT`；指数退避从 `500ms` 到 `10s`，对称 jitter ratio `0.1`。
- Provider 给出正数 `providerRetryAfterMs` 且不超过 `maxDelayMs` 时优先采用；normal policy 超过上限则不重试，always policy 回退本地 delay。等待必须可被 turn 取消和插件卸载同时中止。
- 每次计划重试先持久化 `llm/retry`，等待完成再写 `llm/retry-started`；计数按 `turn/step/provider/policyKey` 从 session log 恢复，restart 后不能重置预算或重复尝试。
- policy 省略、code 不可重试或预算耗尽时，waterfall 继续交给下游恢复器；都不接管时，原始结构化 `LlmFailure` 终止 turn。

### 8.4 Session JSONL Replay

- Replay 的主要输入就是已持久化 session JSONL，不维护第二份私有录制格式；按 `(turn, step)` 分组 `assistant/chunk` 重建每次流调用，且每组必须以 `finish` 结束。
- 标记了 `llmStreamCall:true` 的 `compaction/summary` 可按其完整 `rawOutput` 重建一次本地总结调用；未标记者不得推断为 LLM 调用。
- chunk 前纯异常、取消或 hang 无法仅从 JSONL 推导，必须由显式 override sidecar 以 replace/patch 表达；重复 patch index、非法 chunk 或未完成组必须 fail loud。
- 父/子 Agent replay 按记录 session 的 `createdAt` 排序，并在 live session 第一次调用时绑定脚本；每个 session 独立推进 cursor。脚本数不足、存在未绑定脚本或退出时未消费完都必须失败。
- Replay adapter 可以提供 provider/model catalog 供发现测试，但不得访问真实 provider；节奏延迟只能模拟增量传输，不能改变正确性。

### 8.5 当前迁移边界

- `tessivum::llm` 已有 provider-neutral stream 与终止归一化，`agent_loop` 已有基础 turn/step/tool 持久化；这不等于本节兼容完成。
- 缺口至少包括：完整 block/chunk/source/usage wire、冻结 prepared-call/header/context 语义、可恢复 retry ledger、上游 queue/steer 状态机以及 JSONL replay 消费器。
- 在这些契约和对应回归场景完成前，禁止宣称 Agent/LLM 兼容完成。

## 9. Node 兼容桥协议 `cordis.node/v1`

Node 兼容层只服务于显式 `legacy-node` 插件；Rust Host 主路径和原生/WASM 插件不得依赖 Node。

Alpha.11 的 Core 实现基线固定为 `tessivum-core v0.1.5` / `e894744e88cbed359179745e31eed00c1f45201b`。

### 9.1 Transport 与帧

- Host 启动一个长期 Node compat-host 进程；stdin/stdout 构成唯一双工协议流，stderr 仅作诊断日志，绝不承载协议。
- 每帧是 `u32` 大端长度 + UTF-8 JSON；默认单帧上限 `1 MiB`，接收端必须先校验长度再分配和解码。
- 每个 JSON frame 精确包含 `protocolVersion:"cordis.node/v1"`、正数 `connectionGeneration`、`kind`、可选正数 `requestId`、`payload`。未知字段、错误版本、旧 generation 或 request/response 缺失 `requestId` 均为协议错误。
- request kind 闭集：`exit`、`plugin.load`、`plugin.update`、`plugin.dispose`、`plugin.snapshot`、`service.call`、`service.provide`、`service.remove`、`event.subscribe`、`event.emit`、`event.callback`、`registration.dispose`、`web.route.register`、`web.route.unregister`、`web.route.request`、`pnpm.run`。
- 控制/结果 kind：`hello`、`ready`、`response`、`error`、`cancel`、`heartbeat`、`log`、`pnpm.output`。`response/error/cancel` 回显原 `requestId`；远端错误是 `{ code, message, details? }`，不得降级为无结构字符串。`pnpm.output` 不带 `requestId`，以 `operationId` 关联有界 stdout/stderr 流。

### 9.2 Handshake、背压与故障

- 每次进程启动分配递增的 `connectionGeneration`；Host 先发 `hello`，只有收到相同 generation 的 `ready` 后连接才可接受业务请求。
- 单连接拥有有界 outgoing queue、handshake timeout、request timeout、shutdown timeout；满队列立即返回 `QueueFull`，不能无限积压或悄悄丢帧。
- pending 表以 `requestId` 唯一关联；timeout、调用方取消、协议错误、EOF、进程退出或主动 shutdown 必须只结算一次，并清空全部 pending。
- 非活动 generation 的迟到 response/callback 必须拒绝；重启后的新连接不得继承旧 pending、subscription 或 callback handle。
- `event.callback` 是 Node 调用 Host callback 的有界 RPC；Host 结果仍走 `response/error`。`event.subscribe`、`service.provide` 返回的 registration handle 必须能以 `registration.dispose` 幂等释放。

### 9.3 插件生命周期与关闭顺序

- Node 插件加载配置必须经过 schema 校验；`plugin.load` 成功后才进入 loaded，`plugin.update` 必须原子替换配置，`plugin.dispose` 完成 effect/registration 清理后才返回。
- 关闭顺序固定为：停止接收新请求 -> 取消/结算 pending -> 请求插件 dispose/进程 exit -> 等待有界 cleanup -> 关闭 stdin/stdout -> 等待或强制终止子进程。
- 非法帧、Host/Node 任一端崩溃、超时和 forced kill 都必须进入可观察错误结果；不得自动改走 native/WASM runtime 或把失败报告成成功卸载。
- 当前实现已具备真实 compat-host：vendored Cordis/Loader、function/object/class 插件、Service/inject、事件/waterfall、异步 disposer、generation cleanup 与真实社区 timer 样本均有 Rust↔Bun 往返测试。Node 协议本身不再是 placeholder；整体迁移仍需保留本节回归并通过依赖 Node 的上游 Web 场景。

### 9.4 依赖边界

- 默认生产部署可完全不安装 Node/Bun；只有配置了 `runtime: legacy-node` 的插件才启动 Bun compat-host 并要求对应 JS package。Rust Host 主路径、native/WASM 插件和已构建的 Browser 静态运行时不依赖它。
- Browser 上游构建可以在发布流水线使用 Node/Bun，但运行时只消费静态产物，不因此把 Node 引入 Rust Host 主路径。
- Node 桥不得暴露泛型 Rust `ContextHandle`；只允许调用显式 service/event/callback 协议面。

## 10. Tessivum 原生扩展

以下能力允许 Tessivum 超越上游，但不能改写前述兼容契约：

- `sessions.search`、分支/合并/回滚 lineage。
- DAG task graph 与 durable event stream。
- parent/child subagent hierarchy。
- native tool、Extism tool、显式 legacy Node tool。
- 原生 browser-control / MCP / LSP / jobs / approval 扩展。
- native/Extism/legacy Node 统一通过受限 `DomainBridge` 调用 `agents@1`、`llm@1`、`systemPrompt@1`、`sessionStore@1`、`toolRuntime@1`；禁止插件拿到泛型 `ContextHandle`。
- Tessivum 自有 browser face 复用上游 package-name boot graph 与 module handoff，不建立第二套插件 id、loader 或 CSS 生命周期。

扩展必须满足：

- 不污染 DeepSeek Harness 兼容层的数据语义、事件名、事件顺序和 Browser RPC 形状。
- 通过能力发现显式暴露；不支持时返回结构化错误，不能 silent no-op。
- 与上游路由冲突时上游兼容路由优先。
- 任一扩展关闭后，原版 Harness 核心路径仍然成立。

## 11. 完成定义

只有以下条件全部满足，才可以宣称“DeepSeek Harness 迁移完成”：

1. 上游 Browser shell 与动态 client plugin graph 无私有 fork 地启动。
2. 全部冻结 Core RPC、Remote command、四象限 envelope 和事件流通过契约测试。
3. Session JSONL、projection、恢复、tool-call、LLM retry/replay 与 cancellation 语义通过场景测试。
4. 上游 profile 中所有已启用 client 包动态发现、加载、卸载和样式清理正常。
5. `legacy-node` 的 `cordis.node/v1` 完成真实 compat-host 往返、超时、取消、重启和有界关闭测试。
6. 默认 Rust Host + Browser UI 路径不需要 Node runtime；native/Extism/legacy Node 边界可独立关闭。
7. Tessivum 扩展关闭后，上游核心行为仍保持兼容。
8. 端到端运行真实 Rust 后端、真实上游 Browser shell、真实 WebSocket/API、真实 session store、真实工具和至少一个真实或 replay LLM adapter；不得用静态响应冒充。

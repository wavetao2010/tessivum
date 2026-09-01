# Tessivum Phase 8 Remote Access 与新版 Legacy Host 兼容开发计划

> 状态：已完成；Alpha.19-A/B 通用兼容层、Alpha.19-C Rust Remote Access、Alpha.19-D 自有最小 Browser/mobile 界面及 Alpha.19-E 发行门槛已关闭
> 计划日期：2026-09-01
> 实现基线：`v0.1.0-alpha.19`
> 发布：`v0.1.0-alpha.19`
> Core 基线：`tessivum-core v0.1.6` / `bafb893f182d64b7b464b6cf827676f7ac368168`
> Browser 兼容基线：DeepSeek Harness `0.1.0-rc.5` / `47f943859bef60e4160492346772ded9b24f765a`
> 分析样本：`@linxin666/dsh-remote-web-ui@0.3.6`

## 1. 文档目的

Phase 8 在不升级冻结 Browser 基线、不把 Rust Host 重新变回 TypeScript Host 的前提下，交付两组相互依赖但所有权不同的能力：

1. 多个 Legacy Node 插件都会复用的 Host 兼容 facade；
2. 由 Rust Host 掌握最终权限的 Remote Access 产品能力。

远程插件的二维码、移动端和设置界面优先复用已有实现；配对、设备会话、撤销、API/Origin/Host 授权和远程 WebSocket 握手不能交给一个任意 Node 回调成为最终裁决者。

关联文档：

- [二阶段开发计划](DEVELOPMENT_PLAN.md)：总体运行时分工与实施顺序；
- [目标运行时架构](ARCHITECTURE.md)：Rust Host、Legacy Node 和 Browser Cordis 的所有权；
- [插件生态兼容方案](PLUGIN_COMPATIBILITY.md)：固定版本矩阵、DomainBridge 和源码兼容声明；
- [DeepSeek Harness 兼容基线](COMPATIBILITY_BASELINE.md)：冻结 Browser/Wire 契约；
- [Phase 7 第一方插件市场计划](PHASE7_FIRST_PARTY_MARKET_PLAN.md)：固定来源、可复现插件产物和 Host-owned restart 的先例。

如实现需要偏离本文，先更新本文及关联架构文档，再修改代码。

## 2. 当前事实与故障证据

### 2.1 安装成功，Host 激活失败

已观察到的实际链路：

```text
pnpm Profile 已安装 @linxin666/dsh-remote-web-ui@0.3.6
  → Core Loader 读取 cordis.patch.yml
  → Legacy Node import 插件 lib/index.js
  → import @deepseek-ai/dsh-host-apiproxy/api/rpc
  → Node ESM 无法解析包
  → Loader dispose 已挂载 Fiber
  → INVALID_HOST_CONFIG
```

发布包的 `lib/index.js` 对 `RpcId` 是真实值导入，并在请求、响应和 Mux 路径实际调用。该包只把 `@deepseek-ai/dsh-host-apiproxy` 声明在 `devDependencies`，没有声明为运行依赖或 peer；安装后的 pnpm dependency graph 因而不包含它。

这不是插件业务逻辑中的运行时异常：插件尚未进入 `apply`，失败发生在模块求值阶段。

### 2.2 版本契约不一致

样本插件声明：

```json
{
  "dsh": {
    "engines": {
      "dsh": ">=0.1.1-rc.1"
    }
  }
}
```

其开发依赖使用 `0.1.1-rc.2` Host 包；Tessivum 的冻结 Browser/Wire 基线仍是 `0.1.0-rc.5`。自定义 `dsh.engines.dsh` 不是 pnpm 的标准 `engines.node`，当前安装路径不会自动拒绝该不匹配。

Phase 8 不通过伪报 Host 版本绕过约束。兼容性必须由精确 capability 报告和真实场景证明。

### 2.3 当前能力盘点

| 能力 | 当前事实 | Phase 8 结论 |
|---|---|---|
| Rust sessions/workspaces/models/settings API | 52 个冻结 Core RPC 已实现 | 复用，不重写领域逻辑 |
| Browser mux/host WebSocket | 两条下行已实现 | 复用，并为 Node facade 提供受限订阅 |
| Browser commands | `commands/list`、`commands/execute` 已实现，`ctx.commands` 已接入 | 复用 Rust command registry |
| Legacy HTTP route | `web.route/v1` 已实现 | 保留现有 Host/Origin/body/deadline 边界 |
| Legacy WebSocket Upgrade | `web.upgrade/v1` 已实现并有真实社区插件验证 | 不重做 |
| `webServer.host` / `webServer.port` | Compat Host 已暴露不可写 Rust listener 快照 | 保持只读和 generation 归属 |
| Node `ctx.apiProxy` | 已由版本化 DomainBridge 服务组合，覆盖冻结 Core RPC 域 | 不把 TS Host 搬回 Rust |
| `api/gate` | 不在冻结基线，当前未实现 | 不实现任意 Node waterfall 授权；远程安全策略归 Rust |
| `webserver/index-inject` | 当前未实现；早期计划明确排除任意 `tapIndex` | 不开放通用 HTML/脚本注入 |
| Remote pairing/device/revocation | 当前没有原生领域 | 新增 Rust-owned Remote Access 域 |

## 3. 阶段目标

Phase 8 完成后必须同时成立：

1. Legacy Node 插件可通过版本化、可序列化、有限方法集访问已有 Rust Host 能力；
2. `ctx.apiProxy` 和 `ctx.commands` 只是 Node 侧适配形状，Rust wire 仍按领域服务版本化；
3. 远程访问默认关闭，默认监听与现有 loopback 安全姿态不变；
4. 启用远程访问时，配对、设备会话、撤销、HTTP 与 WebSocket 授权由 Rust Host fail-closed 执行；
5. 复用远程插件的展示层时保留来源、许可证和固定版本，不复制其 Node Host 权威；
6. 不兼容插件在 Profile mutation 提交前得到结构化诊断，不能再次把正常 `tsv web` 安装成无法启动的状态；
7. 只有真实桌面端与移动端 Browser E2E、安全拒绝矩阵和发行包 smoke 全部通过后，才发布 `alpha.19`。

## 4. 冻结决策与明确不做

### 4.1 冻结决策

1. 保持 DeepSeek Harness `0.1.0-rc.5` Browser/Wire 基线；Phase 8 不是全量 `0.1.1` rebase。
2. Rust Host 继续持有 Session、Workspace、Model、Settings、Command、权限和持久事实。
3. Node compat-host 在现有 DomainBridge 服务之上组装 `ctx.apiProxy`；Rust 不实现一个携带任意 JS 对象的单体 ApiProxy。
4. 新增的跨运行时服务使用独立版本，例如 `workspaces@1`、`models@1`、`commands@1`、`hostEvents@1`、`webListener@1`；方法集按实际调用冻结。
5. `webServer.host` 和 `webServer.port` 是不可写快照；Node 插件不能自行 bind、换端口或扩大监听范围。
6. Remote Access 是 Rust 原生安全域。Legacy/Browser 插件负责展示和可选 transport，不拥有最终 allow/deny。
7. 样本插件只作为精确兼容与 UI 复用输入；未通过固定版本矩阵前，不把其 npm 名称写入“已支持”列表。
8. 复用第三方前端代码时固定 commit、tarball integrity、许可证和本地改动审计，采用 Phase 7 已验证的可复现导入方式。

### 4.2 明确不做

- 不为了一个插件安装完整 `@deepseek-ai/dsh-host-apiproxy` 及其 Host 依赖树；
- 不把缺失 import 静默改写成成功 no-op；
- 不伪造 `dsh` 版本满足插件 engine；
- 不开放任意 Node `api/gate` 回调决定匿名请求是否进入 Rust API；
- Legacy Node `web.route/v1` 和 `web.upgrade/v1` 始终保持 loopback-only，不能成为 Remote Access 的匿名或已配对入口；
- 不开放任意 HTML、inline script 或 `tapIndex`；
- 不让 Node 插件启动第二个对外 HTTP listener；
- 不默认开放 LAN，也不在非 loopback 明文 HTTP 上发放长期 bearer session；
- 不从零重写已有二维码、移动端和设备管理 UI；
- 不把一个固定插件样本扩大解释为“兼容所有 `0.1.1` 插件”。

## 5. 目标架构

```text
Browser / Mobile UI
  ├─ QR、设备列表、设置、会话交互
  └─ 只调用 Remote Access 与现有 Host API
                 │
                 ▼
Rust HTTP / WebSocket Authority
  ├─ Host + Origin + remote session 校验
  ├─ 配对 token、设备 session、撤销与审计
  ├─ 52 Core RPC、commands、mux/host events
  └─ 有界 Legacy route / upgrade registry
                 │
       ┌─────────┴─────────┐
       ▼                   ▼
Native Rust Domain    Legacy Node Compat Host
remoteAccess@1        ctx.apiProxy / ctx.commands
Host API domains      webServer read-only metadata
       ▲                   │
       └──── versioned DomainBridge ────┘
```

两条不变量：

- Node facade 可以改变调用形状，不能改变 Rust 权限结论；
- Browser 插件可以改变呈现，不能成为持久状态或远程认证权威。

## 6. 通用 Legacy Host facade

### 6.1 `ctx.apiProxy` 适配

Compat Host 从版本化领域服务组合插件期望的对象形状。第一批只覆盖样本和既有兼容场景实际调用的方法：

- sessions：list、create、history、search、prompt、models、selectModel、rename、cancel；
- workspaces：list、create、listDirectory；
- Agent Modes：list；
- models：models、providers、discoverModels；
- settings：describe、mutate；
- events：mux subscribe、client response handoff。

规则：

- 每个方法映射到一个现有或新增的 exact `service@version.method`；
- 请求/响应继续使用 Rust schema、大小、并发、取消和 deadline；
- 不把 Rust `Context`、数据库、Session 对象或 channel 直接交给 Node；
- 未实现方法返回稳定 `SERVICE_UNAVAILABLE`，不能返回空成功；
- Node 断开或 generation 退休后，订阅、pending RPC 和 callback 全部 owner-wide cleanup。

### 6.2 `ctx.commands` 适配

`commands@1` 只暴露：

```text
list(sessionId)
execute(sessionId, line, argv, signal)
```

它复用 Rust Host 已有 command registry、Agent ownership、取消和错误映射。Node 不获得命令实现、工具注册表或绕过 Session authority 的入口。

### 6.3 Web listener 元数据

`webListener@1.describe` 返回当前 generation 的只读快照：

```text
host
port
loopback
advertisedOrigins[]
remoteAccessEnabled
```

Compat Host 将 `host`、`port` 投影到 `ctx.webServer`。属性不可写；listener 重启产生新 generation，旧插件不能继续使用旧快照注册路由。

现有 `web.route/v1` 与 `web.upgrade/v1` 保持唯一注册路径。所有路由仍由 Rust Axum 对外承载。

### 6.4 Profile compatibility preflight

插件 mutation 在提交前至少检查：

1. `dsh.engines.dsh` 是否落在已声明 capability matrix；
2. bundle patch、Host entry 和 client entry 是否存在；
3. 发布产物的真实 runtime import 是否能由自身依赖、peer 或 Tessivum 明确 facade 解析；
4. inject 服务是否全部存在于目标 Legacy Context；
5. package/version 是否属于固定兼容矩阵，或是否只能以 unsupported 状态安装。

失败必须回滚 Profile snapshot，并使用结构化错误：

- `PLUGIN_DSH_ENGINE_UNSUPPORTED`；
- `PLUGIN_RUNTIME_DEPENDENCY_MISSING`；
- `PLUGIN_SERVICE_UNAVAILABLE`；
- `PLUGIN_COMPATIBILITY_UNVERIFIED`。

直接手工修改 Profile 仍可能在 boot 时失败，但诊断必须点名 package、version、specifier/service 和当前支持基线。

## 7. Rust-owned Remote Access

### 7.1 默认姿态

- 默认关闭 Remote Access；
- 默认 listener 继续 loopback-only；
- 启用远程模式必须是显式 Host 配置，不由安装插件自动开启；
- Tunnel 只改变传输可达性，不改变 Host 授权；
- 首版不支持未经 TLS 的直接 LAN bearer session。

### 7.2 配对与设备会话

Rust Host 负责：

- 使用 CSPRNG 生成一次性、短 TTL 配对 token；
- token 只存 hash，成功兑换后立即失效；
- 设备 session 使用独立随机凭据、到期时间和最后活动时间；
- 支持设备命名、列表、单设备撤销、全部撤销和过期清理；
- 持久化文件遵守 Host data directory、0600 和原子写入规则；
- 日志、事件、API 和 Browser 状态永不返回原始 token/session secret。

### 7.3 HTTP 与 WebSocket 授权顺序

非 loopback 请求按固定顺序处理：

```text
listener remote mode
  → normalized Host
  → exact Origin / Fetch metadata
  → TLS / trusted tunnel posture
  → remote device session
  → revocation / expiry
  → route-specific authorization
  → body decode and domain handler
```

WebSocket 在 Upgrade 前执行同一 authority；握手后绑定 device/session generation。撤销或 Host shutdown 必须关闭相关连接。

所有拒绝为稳定状态与错误码：

- `REMOTE_ACCESS_DISABLED`；
- `REMOTE_TLS_REQUIRED`；
- `REMOTE_HOST_DENIED`；
- `REMOTE_ORIGIN_DENIED`；
- `REMOTE_AUTH_REQUIRED`；
- `REMOTE_SESSION_EXPIRED`；
- `REMOTE_SESSION_REVOKED`。

### 7.4 Node 与 Browser 的权限

Legacy Node 可读取脱敏设备状态，并通过 exact DomainBridge 方法请求 issue/revoke；Rust 重新校验调用来源和状态。Browser 只获得当前用户界面所需的脱敏投影。

任何 Node callback timeout、异常或断开都按拒绝处理，但 Node callback 不进入最终 API allow/deny 热路径。

## 8. UI 与第三方插件复用

### 8.1 复用范围

优先复用样本插件中与 Host 权威可拆分的部分：

- QR 与配对交互；
- 设备列表、状态和撤销界面；
- 移动端 Session/Workspace 页面；
- 远程设置表单；
- 可选 Tunnel 状态展示。

不得直接复用为产品权威的部分：

- Node 内存中的 token/device session 真相；
- Node `api/gate` 最终授权；
- 将远程请求代理成 loopback 从而绕过 Host authority；
- 任意 pre-boot inline script 注入。

### 8.2 交付形态决策门槛

先做固定源码审计和最小分离实验：

1. 若 Browser/mobile half 能仅通过稳定 HTTP/WebSocket contract 接入 Rust，则保留其 UI，Host half 替换为薄 adapter；
2. 若发布包无法分离，但源码可小范围改造，则维护有 provenance 的 Tessivum adapter package；
3. 只有当前端与 Host 权威强耦合、复用成本高于最小界面时，才实现 Tessivum 自有 UI；不得复制其 Node 后端。

最终包名在实现里程碑 A 结束时冻结。无论采用哪种形态，`@linxin666/dsh-remote-web-ui@0.3.6` 都继续作为差分和兼容诊断 fixture。

实现决策：不导入 `@linxin666/dsh-remote-web-ui@0.3.6` 的 Host 或 Browser 运行时代码。该包与较新的 DSH Host 接口、Node gate 和自身 Tunnel 权威强耦合；Tessivum 改用发行包内固定的 `src/remote_access_page.html`，只通过 Rust HTTP contract 生成 QR、兑换配对、展示设备和执行撤销。样本包继续只作为不兼容预检与安全差分 fixture。

### 8.3 Boot contribution

不提供通用 `webserver/index-inject`。若复用 UI 必须在 Browser Cordis 启动前安装极小 bootstrap：

- 优先消除该需求或改为正常 client bundle；
- 无法消除时，只接受构建期固定、hash 绑定、大小受限的 Tessivum-owned asset；
- 不接受运行时 Node 提供的 HTML 或 inline script；
- contribution 必须进入 source audit、CSP/nonce 策略、发行 inventory 和 Browser E2E。

## 9. 实施里程碑

### 9.1 Alpha.19-A：兼容预检与契约冻结

1. 固定样本插件 tarball、源码 commit、integrity 和许可证；
2. 输出 runtime import、inject、route、event 和 Browser client dependency inventory；
3. 加入 `dsh.engines.dsh` 与 runtime dependency preflight；
4. 冻结新增 DomainBridge service/method、DTO、error code 和 capability negotiation；
5. 证明失败 mutation 回滚后原 Profile 与 `tsv web` 仍可启动。

### 9.2 Alpha.19-B：通用 Node facade（已完成）

1. 实现缺失的 domain services；
2. 在 Compat Host 组装 `ctx.apiProxy` 和 `ctx.commands`；
3. 暴露不可写 `webServer.host` / `port` 快照；
4. 保持 `web.route/v1` / `web.upgrade/v1` 边界；
5. 完成取消、generation cleanup、超时和负载上限测试。

### 9.3 Alpha.19-C：Native Remote Access（已完成）

1. 实现 Rust 配对 token、device session、撤销、过期与持久化；
2. 把 HTTP、SSE 和 WebSocket remote authority 接入统一 middleware；
3. 保持默认 loopback，增加显式 remote 配置和 TLS gate；
4. 提供脱敏 Browser/Legacy 状态与 exact mutation API；
5. 完成重启恢复、撤销断连和 fail-closed 场景。

### 9.4 Alpha.19-D：自有最小产品界面（已完成）

1. 冻结 Rust-owned 单文件 `/remote` 界面，不引入第三方 adapter 或第二套 Cordis 应用；
2. 桌面端生成 QR/fragment link、列出设备并撤销，移动 viewport 完成命名和配对；
3. 配对后进入现有 Session/Workspace Web shell，不复制 Session、模型、设置或工具权威；
4. loading、empty、active、expired、revoked、disconnected 和结构化错误状态均由同一页面呈现；
5. release inventory、双 Browser E2E、安全拒绝矩阵和无后台 Tunnel 进程约束纳入现有门槛。

### 9.5 Alpha.19-E：发行（已完成）

1. 将最终精确包/version 加入插件兼容矩阵；
2. 更新 README、架构、安全说明、release notes 和第三方许可证；
3. 运行全量 Rust、Core、Browser、插件与发行包门槛；
4. 从四平台归档进行 clean install/upgrade/uninstall smoke；
5. 所有门槛通过后最后修改版本并发布 `v0.1.0-alpha.19`。

## 10. 文件与仓库影响地图

| 工作 | `tessivum` | `tessivum-core` | 插件/UI |
|---|---|---|---|
| Profile preflight/rollback | `src/plugin_manager.rs`、CLI/市场 mutation | protocol diagnostics | 固定 fixture |
| Domain services | `src/bridge.rs`、`src/host.rs`、`src/api.rs` | node bridge protocol/supervisor、Compat Host | 无领域真相 |
| Listener metadata/routes | `src/bridge.rs`、Web host startup | Compat Host `webServer` facade | 只读消费 |
| Remote security | 新的 Rust remote-access domain、API middleware、persistence | 仅版本化 DTO/bridge | 脱敏状态与 mutation |
| Browser/mobile | boot graph、bundle audit、Web tests | 无 React 实现 | 复用或薄 adapter |
| Release | workflow、packaging、license inventory | 固定 Core revision | 固定 tarball/source evidence |

若 `tessivum-core` wire 发生变化，必须先独立发布新的 Core 版本，再由产品仓库固定精确 revision；不得让本地 checkout 与 release/CI 使用不同协议。

## 11. 验证矩阵

### 11.1 安装与失败恢复

| 场景 | 必须证明 |
|---|---|
| 原始 `0.3.6` 无适配安装 | 在提交前得到明确 engine/dependency/capability 诊断，Profile 不变 |
| pnpm 部分写入 | snapshot 恢复 package.json、lock、bundle order 和 node_modules 可用状态 |
| 手工写入不兼容包 | boot 点名缺失 specifier/service，不产生成功空服务 |
| 删除/更新 | Fiber、routes、upgrades、subscriptions、device callbacks 无残留 |

### 11.2 通用 facade

| 场景 | 必须证明 |
|---|---|
| Session/Workspace/Model/Settings | Node 调用与现有 Browser RPC 返回同一权威结果 |
| Commands | exact Session、取消、未知命令和运行错误保持 Rust 语义 |
| Mux | 顺序、取消、重连、generation cleanup 和响应 handoff 正确 |
| Web metadata | host/port 正确且不可写，listener 重启使旧快照失效 |
| Unsupported method | 稳定失败，不返回空数组、空对象或假成功 |

### 11.3 Remote security

| 场景 | 必须证明 |
|---|---|
| 默认启动 | 仍为 loopback-only，无 remote route/session/tunnel 副作用 |
| 配对 | token 单次、短 TTL、成功即失效，日志与 Browser 不泄漏 secret |
| 设备 session | 有效访问成功；缺失、篡改、过期、撤销全部拒绝 |
| HTTP authority | Host、Origin、TLS、session 任一失败均在领域处理前拒绝 |
| WebSocket | 握手复用同一 authority；撤销与 shutdown 主动断连 |
| 重启 | 合法持久状态恢复，过期/撤销状态不复活 |
| 并发 | 配对竞争只成功一次，撤销与在途请求结果确定 |

### 11.4 Browser 与移动端

真实 Chromium 至少覆盖：

1. 桌面打开配对面板并生成 QR；
2. 独立移动 viewport 完成配对；
3. 移动端列出 Workspace/Session、读取历史、发送 Prompt、取消；
4. 模型读取/选择和设置变更遵守现有 authority；
5. 桌面设备状态实时更新，撤销后移动端 HTTP 与 WebSocket 同时失效；
6. Host restart 后页面恢复或给出明确重新配对状态；
7. `pageerror=[]`、受监控 `console.warn/error=[]`、未预期 HTTP 4xx/5xx 为空。

### 11.5 发行包

- source audit、依赖闭包、许可证和 provenance 闭合；
- 四平台归档含正确 Compat Host、Host modules、Browser assets 和固定插件/adapter artifact；
- clean data root 首启不启用 Remote Access；
- Alpha.18 → Alpha.19 升级保留 Session、Settings、插件 Profile 和设备撤销状态；
- uninstall 删除程序但保留用户数据，文档明确清理 Remote Access 数据的方法；
- SIGTERM/restart 不留下 Bun、Tunnel、WebSocket 或 pnpm 子进程。

## 12. 强制发布门槛

Phase 8 代码完成后至少执行：

1. `tessivum-core` Rust、Node bridge、protocol 与 compatibility tests；
2. `tessivum` Rust workspace tests、clippy、format 和 compatibility baseline check；
3. Web source audit、38 client package build、typecheck 和 69 场景回归；
4. 固定插件/adapter 的 source tests、bundle build 与 dependency-closure preflight；
5. Remote Access 的真实双 Browser E2E 和安全拒绝矩阵；
6. release archive、Homebrew、升级、回滚、关闭与进程树 smoke。

不得以单元测试替代真实 Profile 安装和 Browser 配对流程，也不得以 loopback mock 证明非 loopback authority。

## 13. 完成定义

Phase 8 只有在以下全部满足后才能标记“已完成”：

- 通用 Node facade 由版本化 DomainBridge 支撑，没有 Rust/JS 对象越界；
- 不兼容插件安装会回滚，用户不会因一次市场安装失去 `tsv web`；
- Remote Access 默认关闭，启用后由 Rust 执行 fail-closed HTTP/WebSocket authority；
- UI 复用或自有最小 UI 的来源、许可证、构建和差异记录完整；
- 精确版本矩阵中的 built-in Remote Access 产品面通过 Host、Browser、移动端、重启、撤销和进程清理 E2E；分析样本保持明确不支持安装；
- 现有三个社区兼容样本与第一方市场无回归；
- README、架构、插件兼容矩阵、安全限制、release notes 和发行 inventory 已更新；
- 四平台发行门槛通过；
- 最后才把产品版本推进到 `v0.1.0-alpha.19`。

## 14. 风险登记

| 风险 | 控制 |
|---|---|
| 为一个插件复制完整新版 Host | 只加实际使用的 versioned domain methods，Node 侧组装 JS facade |
| Node gate 成为安全权威 | Rust middleware 做最终判定，Node 不进入 allow/deny 热路径 |
| 明文 LAN 泄漏 bearer session | 首版非 loopback 强制 TLS/trusted tunnel，不默认开放 LAN HTTP |
| pre-boot 注入扩大 XSS 面 | 禁止通用 index injection；仅构建期固定、hash 绑定 asset |
| 插件升级再次漂移 | 精确版本矩阵、runtime import/inject audit、显式 rebase review |
| Remote API 绕过现有权限 | 复用同一 Host domain handlers，不建立第二套 Session/Workspace 真相 |
| Mux 跨进程背压或泄漏 | bounded queues、deadline、cancel、generation owner cleanup |
| Profile 安装导致产品无法启动 | mutation 前 preflight、事务快照、失败回滚和真实 restart smoke |
| 复用 UI 形成长期大 fork | 先分离稳定 HTTP/Wire contract，仅维护必要 adapter 和 provenance |
| Remote 功能拖累默认用户 | 默认关闭、无后台任务、无额外 listener、无 Tunnel 进程 |

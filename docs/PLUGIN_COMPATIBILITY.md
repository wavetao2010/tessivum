# Tessivum 插件生态兼容方案

> 目标：在 Rust 化 Host/Agent Runtime 的同时保留现有 DeepSeek Harness npm 插件生态，并为新插件建立 Extism/WASM 路径。总体顺序见[二阶段开发计划](DEVELOPMENT_PLAN.md)，运行时细节见[目标运行时架构](ARCHITECTURE.md)，Native Mode 与 `agent.cordis.yml` clean cutover 见 [Phase 5 计划](PHASE5_NATIVE_AGENT_MODES_PLAN.md)，当前 Profile authority 与 mutation 语义见 [Phase 6 计划](PHASE6_DSH_PROFILE_COMPATIBILITY_PLAN.md)，早期 dshmarket HTTP Bridge 架构记录见 [Phase 4 计划](PHASE4_BRAND_DISTRIBUTION_MARKET_PLAN.md)。

## 1. 结论

Extism/WASM 可以承载新的跨语言插件生态，但不能透明运行现有 Cordis npm 插件。现有生态通过 Legacy Node Host 保持兼容：

```text
现有 npm/Cordis 插件  → Legacy Node Host
新跨语言插件          → Extism/WASM
性能关键官方插件      → Native Rust
React/浏览器插件       → TypeScript Browser Cordis
```

“兼容现有生态”不等于“把现有 TypeScript 自动编译成 WASM”。

## 2. 当前插件契约

现有插件通常是 npm ESM/TypeScript 包，导出以下一种形态：

```ts
export const name = 'plugin-name'
export const inject = ['tools']
export const Config = schema
export function apply(ctx, config) {}
```

或者：

```ts
export default class MyService extends Service {
  constructor(ctx) {
    super(ctx, 'myService')
  }
}
```

插件可能依赖：

- 活的 `Context` Proxy；
- Service 对象及其方法；
- `ctx.on`、`ctx.waterfall`、`ctx.effect`；
- 函数、Promise、AbortSignal、Stream；
- Node `fs/path/net/child_process`；
- npm 依赖和 native addon；
- 浏览器 DOM、React、window；
- Loader Entry、HMR 和动态模块加载。

这些都不是通用 WASM ABI。

事实来源：

- [第一个 Cordis 插件](../../upstream/deepseek-harness/docs/cordis-tutorial/01-first-plugin.zh.md)
- [生命周期与 Effect](../../upstream/deepseek-harness/docs/cordis-tutorial/02-lifecycle-and-effects.zh.md)
- [服务与 Inject](../../upstream/deepseek-harness/docs/cordis-tutorial/03-services.zh.md)
- [Extism JavaScript PDK](https://github.com/extism/js-pdk)

Extism JS PDK 使用 QuickJS/WASM，不是 Node：没有 Node API、DOM、真实事件循环、timer、动态 import 或后台任务。因此只适合可打包的纯 JS 逻辑，不能视为 npm Cordis runtime。

## 3. 兼容性的五个层次

| 层次 | 定义 | 目标 |
|---|---|---|
| Agent Mode 文件兼容 | 原 `agent.cordis.yml` 可直接作为 Tessivum Session Runtime | 不支持；必要时提供一次性 importer 或显式 Legacy DSH 模式 |
| Profile 配置兼容 | Profile 中的 npm 包名、版本和 client-half 仍可识别 | 必须 |
| 分发兼容 | 原 npm 包可继续安装和解析 | 必须 |
| 源码兼容 | 插件无需修改即可运行 | 仅对通过固定矩阵的 Legacy Node 插件声明支持 |
| ABI 兼容 | 原 JS 对象/函数直接进入 Rust/WASM | 不支持，也不应伪装支持 |

兼容声明必须点明层次。禁止用“支持 TypeScript”暗示原 npm 插件可直接进入 Extism，也禁止用“删除 `agent.cordis.yml`”暗示 npm/Browser 插件兼容层被删除。

## 4. 插件分类与处理

| 插件类型 | 默认运行时 | 说明 |
|---|---|---|
| Rust 官方核心 | native | Agent、Session、LLM、Tools 等高频主干 |
| 新 `.wasm`/WASM manifest | wasm | Extism 执行，显式权限 |
| 现有 npm Cordis 包 | legacy-node | 原样交给 Node Compatibility Host |
| 有 `dsh.client` 的浏览器包 | browser | Host 发布 bundle，页面 Cordis 挂载 |
| npm 包主动发布 WASM 构件 | wasm | package manifest 明确声明，不按语言猜测 |

Loader 不能通过扫描源码猜 runtime。选择优先级：

1. profile/entry 显式 runtime；
2. 包 manifest 的版本化 Cordis runtime 声明；
3. `.wasm` 构件；
4. 现有 npm/Cordis 包默认 `legacy-node`；
5. 无法判断则明确失败。

### 4.1 Agent Mode 与插件激活

Agent Mode 配置格式和插件 Runtime 是两条独立轴：

```text
Native AgentModeSpec / mode.toml
  → 引用已注册 Native Entry
  → 引用 cordis.plugin/v1 WASM Entry
  → 引用活动 pnpm Profile 中的 Legacy npm Entry

pnpm Profile
  → 决定 Legacy/Browser 包是否已安装

Browser boot graph
  → 独立发布已安装包的 dsh.client half
```

删除 Rust 对 `agent.cordis.yml` 的运行时解析后，现有 npm 包仍按 `legacy-node` 路由到真实 Cordis compat-host；新 Mode 只负责决定某个 Session 是否获得该插件经 DomainBridge 发布的 Agent 能力。Profile 级 Browser 插件不因未被 Agent Mode 引用而消失。

Mode Resolver 必须在 Agent 启动前验证 Runtime、安装状态、manifest、权限和受支持服务。需要原始 `agentCore`、`llm`、`systemPrompt`、`sessionStore`、`toolRuntime` 模块或未桥接 JS 对象身份的插件继续明确失败，不能通过 Mode 引用绕过兼容边界。详细迁移、持久字段和验收矩阵见 [Phase 5 计划](PHASE5_NATIVE_AGENT_MODES_PLAN.md)。

## 5. 目标插件 Manifest

新 WASM 插件使用已冻结的 `cordis.plugin/v1` manifest：

```yaml
schemaVersion: cordis.plugin/v1
id: com.example.search
version: 1.2.0
runtime: wasm
entry: plugin.wasm
abi: cordis.plugin/v1
inject: []
permissions:
  - cordis.service.call
servicePermissions:
  - service: tools@1
    methods: [schemas]
configSchema:
  type: object
  additionalProperties: false
exports:
  - cordis_init
  - cordis_call
  - cordis_event
  - cordis_update
  - cordis_stop
```

规则：

- `id` 在不同 Loader entry 间唯一；同一 entry 的 committed/candidate generation 通过不透明 instance authority 隔离；
- `abi` 不匹配、entry 越界或缺少五个生命周期 export 时在装载前失败；
- `inject` 是激活门控，不是权限授予；
- `permissions` 是通用 Host capability 上限；
- `servicePermissions` 只接受已发布的精确 `service@version` 与 method，不接受通配符、前缀或正则；
- 非空 `servicePermissions` 必须同时声明 `cordis.service.call`；
- config 先由 Host 验证，再传给 Guest；未声明能力默认不可用；
- manifest 不能通过 Guest 运行时返回值自我扩大权限。

现有 npm 包无需补此 manifest 才能通过 Legacy Node Host 运行。

## 6. Legacy Node Host

### 6.1 兼容目标

在不修改插件源码的前提下支持：

- npm 包解析；
- function/object/class 插件；
- `name`、`inject`、`Config`、`apply`；
- Service 注册和依赖门控；
- `ctx.on`/`once`/事件分发；
- `ctx.effect` 与异步 disposer；
- Node API 和普通 npm 依赖；
- Loader 配置和必要的 HMR；
- 浏览器 client bundle 元数据发现。

### 6.2 运行策略

Node Host 内运行 vendored `@deepseek-ai/cordis`，保持 JavaScript Context 和插件子图。Rust 不尝试重建插件内部对象图。

```text
Rust Loader
  → node.plugin.load(package, config, contextDescriptor)
  → Node Cordis ctx.plugin(...)
  → registrations/services/events 经代理发布到 Rust
```

同一组互相传递任意 JS Service 对象的插件应留在同一个 Node 子图。只有具备稳定可序列化契约的服务才跨 Rust/Node 边界。

### 6.3 生命周期映射

| Node 行为 | Rust 所有权 |
|---|---|
| `ctx.provide` | Rust 或 Node 服务注册记录绑定 Node generation |
| `ctx.on` | Rust listener proxy + Node callback id |
| `ctx.effect` | Node Fiber 管理；跨边界资源同时登记 owner id |
| `ctx.plugin(child)` | Node 子 Fiber，归属父 plugin instance |
| `fiber.dispose()` | Rust 发 dispose，等待 Node 全部异步 cleanup |
| Node 进程断开 | Rust 按 generation 批量撤销全部代理资源 |

### 6.4 首批跨运行时服务

按 Harness 实际插件使用优先实现：

1. `logger`；
2. `timer`；
3. `tools`：schema、register、execute；
4. `systemPrompt`：section/register/assemble 所需面；
5. `llm`：adapter 注册和请求流；
6. `sessions`：受控 append/read；
7. `agents`：按 ID 查找、发送和取消的受控面；
8. `settings`/`credentials`：只暴露明确读取接口。

不要暴露通用 `ctx.get(any)` 跨边界代理。每增加一个服务代理，必须有：

- 版本化 schema；
- 调用方向；
- cancellation；
- 错误码；
- payload/并发上限；
- 生命周期所有者；
- 真实插件兼容样本。

### 6.5 不兼容情况

即使使用 Node Host，以下情况仍需处理或明确不支持：

- 插件依赖 Host 私有源码路径；
- 插件假定与 Rust 核心共享 JavaScript 对象身份；
- 插件 monkey-patch Cordis 私有字段；
- native addon 不支持当前 OS/CPU；
- 插件依赖未代理的 Rust 服务对象方法；
- 插件跨 Node/Rust 传递函数、Stream 或类实例；
- 插件把 Context 对象塞进公共服务返回值。

兼容报告必须指出具体 API/依赖，而不是只报“加载失败”。

## 7. Extism/WASM 插件协议

### 7.1 适用场景

适合：

- JSON/二进制输入输出工具；
- 纯计算与转换；
- 通过受控 HTTP/FS/数据库能力执行的业务插件；
- 多语言第三方插件；
- 模型动态生成、需要明确沙箱边界的插件。

不适合：

- Harness 高频内部事件；
- Token 级流式转发；
- 依赖共享 Rust 对象身份的服务；
- React/DOM 插件；
- 无法中断且长期占用实例的后台服务。

### 7.2 PDK 表面

PDK 提供 Cordis 概念，不复制 JavaScript Context 语法：

```text
plugin.init(config)
plugin.call(method, input)
plugin.on_event(event, payload)
plugin.update(config)
plugin.stop()

host.log(...)
host.call_service(...)
host.emit(...)
host.subscribe(...)
host.register_tool(...)
host.dispose(resource_id)
```

Host 持有 timer、subscription 和后台任务。Guest 只保存 Handle，不能自行创建逃离调用生命周期的线程或事件循环。

### 7.3 TypeScript/JavaScript PDK

可以提供 TypeScript PDK，降低社区迁移成本，但必须明确：

- 构建目标是 Extism QuickJS/WASM，不是 Node；
- 纯 JS npm 依赖可能可打包；
- Node built-in、DOM、native addon 不可用；
- `async/await` 不代表存在并发事件循环；
- I/O 必须通过 Host Function；
- 现有 `apply(ctx)` 代码通常需要改写。

## 8. 浏览器插件

浏览器插件不进入 Extism JS PDK。当前产品边界是：

- Rust Host 严格扫描 package `dsh.client` 与 conditional `./client` export，生成 `window.__DSH_BOOT__` 并按哈希提供 bundle；
- 浏览器 TypeScript Cordis 保持 inject/lifecycle、React component/slot、remote 和 dynamic client half；
- `web/package.json` 固定 gateway/remotes、connection/locale/runtime、conversation/layout/settings/sidebar/theme/tool/trajectory/workspace、client-web 与 typert 等 published roster；
- npm 未发布 `dsh-session-log-export` 和 `dsh-client-ui-workflow-run`，因此不伪造条目；`dsh-client-ui-slash` 及少数未发布传递包仅用显式 override 维持 published bundle 解析；
- `web/src/main.ts` 的 module sink/`createRequire` shim 是 fail-loud 兼容层，上游发布 browser-safe client module 后删除。

Rust Host 保持 boot graph、bundle 路由、full-form RPC/SessionEvent、published mux/host WebSocket downlinks 与 durable SSE；Browser Cordis 是明确保留的兼容平面，不是待删除的服务端旧主干。

## 9. 插件迁移等级

### Level 0：原样兼容

插件不改源码，运行在 Legacy Node Host。

适合已经过兼容报告和真实样本验证的 npm 插件；未验证插件必须返回具体诊断，不做“全部兼容”承诺。

### Level 1：打包兼容

插件仍是 npm 包，但 manifest 增加明确 runtime/兼容信息，源码仍运行在 Node。

可声明：

- 所需 Node 版本；
- Host/Client halves；
- 使用的稳定服务版本；
- 是否可被自动分析。

### Level 2：WASM 端口

插件改用 Cordis PDK，发布 `.wasm` 和 manifest。配置 id 可保持，但 runtime 明确改变。

### Level 3：Native Rust

只用于官方核心或经证明需要同进程高频能力的插件。第三方插件不因“更快”被默认要求 Native，因为 Native 等同可信代码执行。

## 10. 迁移评估流程

对每个插件生成报告：

1. 读取 package manifest、Cordis 导出和 `dsh.client`；
2. 列出 inject/provide/event；
3. 检测 Node built-in、native addon、DOM/React；
4. 识别跨运行时服务；
5. 分类为：
   - Legacy 原样运行；
   - Legacy + 新服务代理；
   - 可机械转换的纯工具；
   - 需要人工 WASM 端口；
   - Browser 保留；
   - 不支持；
6. 使用真实场景验证，不仅做静态扫描。

静态工具只给建议，不自动改写并声称完成迁移。

## 11. 从 Cordis npm 插件移植到 WASM

推荐步骤：

1. 保留插件 id、配置 schema 和用户可见行为；
2. 把 `inject` 转成 manifest service dependencies；
3. 把 `ctx.tools.register` 等注册转成 PDK Host Function；
4. 把 Node I/O 转成受控 capability；
5. 把 listener 转成 Guest `on_event` export；
6. 把 disposer 转成 resource id 释放和 `plugin.stop`；
7. 删除 Context/Service 对象身份假设；
8. 为每个跨边界 payload 定义 schema；
9. 增加权限声明和上限；
10. 与 Legacy 版本运行同一行为夹具；
11. 达到等价后再将默认 runtime 从 legacy-node 切到 wasm。

示意：

```ts
// 旧插件
export const inject = ['tools']
export function apply(ctx) {
  ctx.tools.register(definition)
}
```

变为概念上的：

```text
manifest.inject = tools@1
plugin.init:
  resource = host.tools.register(definition)
plugin.stop:
  host.dispose(resource)
```

不是把旧 `ctx` Proxy 搬进 Guest。

## 12. 版本与兼容策略

### 12.1 WASM ABI

- 主版本改变表示二进制/消息不兼容；
- Host 可以同时支持有限数量的 ABI 主版本；
- 插件装载前协商，不能运行到一半才发现；
- 新增可选字段保持旧 Guest 可运行；
- 删除 Host Function 必须跨主版本。

### 12.2 服务协议

每个跨运行时服务独立版本，例如：

```text
tools@1
sessions@1
agents@1
```

Cordis ABI 版本不代替领域服务版本。

### 12.3 Legacy Node

兼容基线固定到明确的 `@deepseek-ai/cordis` 范围和 Node 版本。超出范围的插件给出警告或拒绝，不静默尝试不确定行为。

## 13. 安全与权限

| 类型 | 安全声明 |
|---|---|
| Native | 完全可信，与 Host 同权限 |
| Legacy Node | 可信旧插件；独立进程不是安全沙箱 |
| WASM | 默认无能力，按 manifest 授权 |
| Browser | 客户端不可信，服务端重新校验 |

WASM 权限至少覆盖：

- 文件路径范围和读写模式；
- HTTP host/method/body 上限；
- secret/config key；
- service/method；
- timer/background task 数；
- 内存、调用时间、并发和输出大小。

Legacy 插件不因为经过 Bridge 自动获得 WASM 的安全声明。

Alpha.16 的 Legacy `web.route/v1` 注册只接受 `/dsh-market`、`/sidebar`、`/dream-skin` 三个根命名空间及其路径段后代；其他根、编码绕过和相似前缀必须结构化拒绝。固定版本是已验证的兼容目标，不是运行时安装白名单。

## 14. 测试矩阵

### 14.1 Legacy 样本

至少包含：

- function plugin；
- Service subclass；
- required/optional inject；
- event/waterfall；
- async effect/disposer；
- Node filesystem/network；
- native addon 的可诊断失败；
- Host + browser 双半插件；
- provider 替换后 consumer reload。

当前固定的真实社区样本：

- `dshmarket@1.29.2`：Profile mutation、HTTP prefix route、Browser client bundle 与重启激活；
- `dsh-better-sidebar@0.16.1`：Host/Browser 双半插件、HTTP prefix route、WebSocket upgrade、native-backed settings 写入及重启恢复。
- `dsh-dream-skin@8.30.1`：Host/Browser 双半插件、`/dream-skin/api` 有界状态持久化路由与浏览器主题 bundle；

`tessivum-market@0.1.0-alpha.17` 是另行验证的第一方 Host + Browser 双半插件：保留 `dshmarket@1.38.1` 的固定 MIT 来源与 DSH 社区目录，增加 Tessivum 产品身份、精确版本更新、旧 `dshmarket` 事务迁移、Host-owned 重启和发行物校验。它不扩大上述未修改社区包的固定版本矩阵；实现与证据见 [Phase 7 第一方插件市场与 Host 重启开发计划](PHASE7_FIRST_PARTY_MARKET_PLAN.md)。

### 14.2 WASM 样本

至少包含：

- Rust Guest；
- TypeScript/JavaScript Guest；
- Host Function；
- 工具注册；
- 事件订阅；
- 配置更新；
- 取消/超时；
- 权限拒绝；
- Guest trap/内存超限；
- 卸载后 Handle 失效。

### 14.3 跨运行时

至少包含：

- Native provider → Legacy consumer；
- Legacy provider → Native consumer；
- Native provider → WASM consumer；
- Node 崩溃后的依赖转 Pending；
- WASM trap 后其余插件继续运行；
- Browser plugin 通过 Rust Host wire 正常工作。

## 15. 发布与弃用策略

1. Rust Cordis 首个稳定版同时发布 Native/WASM/Legacy 三条路径。
2. 现有 npm 插件默认继续进入 Legacy Node，不设置强制迁移期限。
3. 官方插件逐个发布迁移状态和替代 runtime。
4. 只有当真实使用数据表明 Legacy 支持范围可收缩时，才提出弃用；弃用必须给出工具、兼容报告和替代版本。
5. Browser Cordis 是否长期保留单独决策，不与 Legacy Node Host 的服务端弃用绑定。

## 16. 兼容方案完成定义

- 冻结的样本 profile 能识别并加载被明确声明支持的 npm 插件；
- Native Agent 能在其 Mode 明确引用且 DomainBridge 支持时看到 Legacy 插件注册的工具/提示词；
- Node 崩溃或插件卸载后无残留；
- 新 WASM 插件 ABI 已冻结；per-plugin manifest permissions 接线前，Host service call 默认拒绝，不能宣称权限生态已完成；
- 浏览器插件继续工作，且 `dsh.client` boot graph 不依赖 Agent Mode 文件；
- 社区插件只有通过固定版本矩阵、受限 Host route、Profile 安装和真实 Browser E2E 后才属于兼容范围；当前范围是 `dshmarket@1.29.2`、`dsh-better-sidebar@0.16.1` 与 `dsh-dream-skin@8.30.1`；
- 删除 `agent.cordis.yml` Runtime Parser 后，Native/WASM/Legacy Node/Browser 四条插件路径仍有独立回归证据；
- 不支持的插件获得具体、可行动的诊断；
- 文档明确区分 Mode 文件、Node 源码兼容与 WASM 沙箱，不做误导性兼容或安全承诺。

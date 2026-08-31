# Tessivum Phase 7 第一方插件市场与 Host 重启开发计划

> 状态：已完成
> 完成日期：2026-08-31
> 实现发布：`v0.1.0-alpha.17`
> Core 基线：`tessivum-core v0.1.6` / `4c3d7b7769e43e2eb228ebf43d46bef6119c4574`
> 上游兼容基线：DeepSeek Harness `0.1.0-rc.5` / `47f943859bef60e4160492346772ded9b24f765a`
> 当前固定社区兼容目标：`dshmarket@1.29.2`、`dsh-better-sidebar@0.16.1`、`dsh-dream-skin@8.30.1`
> 第一方市场实现种子：`dshmarket@1.38.1`；导入前必须固定源码提交、npm tarball integrity 与 MIT 许可证归属

## 1. 文档目的

Phase 4 已完成 `dshmarket@1.29.2` 的真实兼容，Phase 6 已统一 Profile mutation、Bundle 激活顺序和 Loader inventory。本阶段不再等待社区市场上游接受 Tessivum 专属改动，而是交付一个由 Tessivum 维护、仍兼容 DSH 社区目录和插件协议的第一方市场插件。

本计划解决四个相互关联的问题：

1. `dshmarket` 的产品文案和重启命令仍把当前 Host 称为 DeepSeek Harness / `dsh web`；
2. Desktop 兼容路径刻意禁用 `dshmarket` 自带的进程重启，Tessivum 尚未提供 Host-owned 重启服务；
3. 市场用 registry 目标版本与 pnpm 实际落盘版本做校验，但新版本等待期导致“目标 `1.38.1`、实际 `1.37.0`”时只报失败，没有给出显式强制重试入口；
4. Tessivum 可以开发和发布自己的插件，但当前市场没有第一方来源、产品兼容级别和发行所有权。

关联文档：

- [二阶段开发计划](DEVELOPMENT_PLAN.md)：仓库边界、总路线和发布门槛；
- [Phase 4 计划](PHASE4_BRAND_DISTRIBUTION_MARKET_PLAN.md)：品牌、分发、pnpm Profile 和首个 dshmarket 兼容闭环；
- [Phase 6 计划](PHASE6_DSH_PROFILE_COMPATIBILITY_PLAN.md)：`dsh.profile.bundles`、Loader inventory 和统一 mutation 权威；
- [插件生态兼容方案](PLUGIN_COMPATIBILITY.md)：Native/WASM/Legacy Node/Browser 四条插件路径；
- [DeepSeek Harness 兼容基线](COMPATIBILITY_BASELINE.md)：冻结 Browser/wire 契约。

本文定义的第一方市场、发行、迁移、精确版本更新、Host-owned 重启和 Native Host Browser E2E 契约已在 `v0.1.0-alpha.17` 实现；仍未完成的事项必须继续按本文“不做”和兼容边界处理。

## 2. 当前事实与故障证据

### 2.1 当前市场所有权

当前用户安装的是外部 npm 包 `dshmarket`。Tessivum 只提供它需要的兼容 Host 面：

```text
dshmarket Host half
  → desktopProfiles.current
  → desktopPnpm.runPlugin(...)
  → Rust PnpmProfileBoundary
  → pnpm Profile

Browser half
  → /dsh-market/*
  → Rust Axum web.route/v1
  → Bun Compat Host callback
```

这条链路证明 Tessivum 能运行 DSH-compatible Host + Browser 双半插件，但市场的产品文案、自更新策略和进程重启仍归第三方包所有。

### 2.2 品牌残留不是协议名问题

`dshmarket@1.38.1` 的中英文 locale 仍包含：

- `DeepSeek Harness` 产品名；
- `dsh web` 和“关闭当前 dsh 进程”的操作提示；
- 把 `dsh-market vX` 放在主标题附近的展示；
- 使用 DSH Host 作为终端插件、冲突、卸载和重启提示的主语。

必须修改的是用户可见 Host 身份。以下兼容身份继续保留：

- npm 包名和历史来源 `dshmarket` / `dsh-market`；
- `/dsh-market/*` 路由；
- `dsh-market/update-api/v1` schema；
- `dsh.client`、`dsh.bundle.patch`、`@deepseek-ai/*`；
- 第三方插件真实存在的 `dsh-*` 包名。

### 2.3 `1.38.1` 更新失败事件

已观察到：

```text
更新失败: dshmarket — dshmarket 更新目标为 v1.38.1，
但实际安装为 v1.37.0；已自动恢复原版本。
```

相关发布时间：

| 版本 | npm 发布时间（UTC） |
|---|---|
| `1.37.0` | 2026-08-30 03:31:55 |
| `1.38.1` | 2026-08-30 15:42:41 |

故障发生时 `1.37.0` 已超过约一天，`1.38.1` 只发布约十三小时。结果与 pnpm 的新版本等待策略一致：普通 `add dshmarket@latest` 可以成功退出，却把较成熟的 `1.37.0` 落盘。

`dshmarket@1.36.0` 随后正确执行了两项保护：

1. 比较 registry 目标版本与实际安装版本；
2. 在 `RESOLVED_VERSION_MISMATCH` 后恢复更新前构建。

缺陷在恢复入口：self-update UI 只在响应含 `stale: true` 时展示“立即更新”，而版本不匹配响应只带 `failureCode: RESOLVED_VERSION_MISMATCH`。用户得到安全回滚，但没有同页的显式 force retry。

### 2.4 兼容声明边界

`dshmarket@1.29.2` 继续作为冻结社区兼容样本。`tessivum-market@0.1.0-alpha.17` 以 `dshmarket@1.38.1` 的固定 MIT 源码为实现种子，并经过独立的 first-party Host、Browser、mutation、rollback、snapshot 与发行包验证；这不等于宣称未修改的社区 `dshmarket@1.38.1` 已进入兼容矩阵。

## 3. 阶段目标

Phase 7 完成后：

1. Tessivum 拥有一个第一方、可独立测试和发布的市场插件；
2. 市场继续消费 DSH 社区目录并安装 DSH-compatible npm 插件；
3. Tessivum 第一方插件可以在同一 UI 中出现，并明确标注来源与兼容范围；
4. 市场 UI 的当前 Host 身份统一为 Tessivum；
5. 安装目标从浮动 `@latest` 改为已探测的精确版本；
6. 新版本等待、显式强制、实际版本校验和事务回滚形成一个完整状态机；
7. 重启由 Rust Host 拥有，市场不再创建或重放 `dsh` 进程；
8. 用户可以手动立即重启，也可以显式启用“队列完成后自动重启”；
9. 旧 `dshmarket` 与第一方市场完成清洁迁移，不同时注册相同路由；
10. Core Web 在未安装第一方市场时仍不要求 Bun/pnpm。

## 4. 冻结决策与明确不做

### 4.1 冻结决策

1. 第一方市场暂定产品 ID 为 `tessivum-market`；最终 npm/package 名在首次公开发行前冻结。
2. 以 `dshmarket@1.38.1` 的 MIT 源码为实现种子，不从零重写市场、registry、backup、diagnostics 和客户端交互。
3. 保留完整上游许可证、NOTICE/来源和导入提交；Tessivum 品牌不掩盖代码来源。
4. 第一方市场是普通的 Legacy Node + Browser 双半插件，不进入 `tessivum-core`。
5. 第一方市场默认不是 Rust Host 的强制依赖；未安装时 Core Web 和 Native/WASM 继续工作。
6. 第一方市场源码与 Tessivum 同仓维护，发行生成固定、可复现的插件 tarball；不依赖上游合并或浮动 Git branch。
7. 可写 Profile 中只安装已固定的第一方 tarball；tarball 先复制到 `${TESSIVUM_HOME}/artifacts/market/<version>/`，避免 Homebrew Cellar 或解压目录升级后 `file:` 路径失效。
8. 市场不更新自己。第一方市场版本跟随 Tessivum release，升级由 Tessivum 安装/迁移路径提交，不进入市场自己的 mutation 队列。
9. `/dsh-market/*` 和 update API v1 保留为兼容路由；用户可见产品名称改为 Tessivum。
10. 普通安装保留新版本等待期；绕过只能来自当前目标版本上的一次显式用户动作。
11. npm 更新使用检查阶段返回的精确版本，不在 mutation 阶段重新解释 `@latest`。
12. 插件激活继续 restart-required；本阶段不引入 Host/Browser 热挂载第二权威。
13. Host 重启由 Rust 主进程执行；Node 插件只请求，不持有可执行路径、argv 或 shell 权限。
14. 自动重启默认关闭，且只在整个 mutation 队列成功排空后执行一次。

### 4.2 明确不做

- 不全仓替换 `dsh`、`deepseek` 或第三方包名；
- 不伪装第一方市场源码是 Tessivum 原创；
- 不维护永久、无法同步来源的匿名代码拷贝；
- 不在 Rust 中重写完整 dshmarket 路由和 React UI；
- 不让市场自更新、自删除或替换正在运行的自身代码；
- 不默认绕过 `minimumReleaseAge`；
- 不因 registry/mirror 分歧而把较旧实际版本报告为成功；
- 不在自动重启前取消运行中的 Agent；
- 不新增全局 `dsh` shim；
- 不在本阶段建立 Native/WASM 商店包格式、签名服务或远程审核后台；
- 不宣称整个 DSH 社区目录都与 Tessivum 兼容。

## 5. 第一方市场包与发行所有权

### 5.1 源码布局

计划在产品仓库增加：

```text
plugins/market/
  package.json
  src/
  client/
  tests/
  LICENSE.upstream
  UPSTREAM.json
```

`UPSTREAM.json` 至少记录：

```json
{
  "repository": "https://github.com/dsh-market/dsh-market",
  "version": "1.38.1",
  "commit": "<frozen-before-import>",
  "tarballIntegrity": "<frozen-before-import>",
  "license": "MIT"
}
```

`commit` 和 `tarballIntegrity` 未固定前不得提交导入结果。源码更新必须是显式 vendor bump，包含来源 diff、许可证复核、compat snapshot 和 Browser E2E；不建立自动追随 upstream `main` 的任务。

### 5.2 可复现插件产物

Release 构建产生：

```text
share/tessivum/plugins/tessivum-market-<version>.tgz
```

启用或升级时：

1. 校验发行 inventory 和 SHA-256；
2. 原子复制到 `${TESSIVUM_HOME}/artifacts/market/<version>/`；
3. 通过现有 Profile lock 调用 pnpm 安装该稳定路径；
4. 更新 `dependencies` 与 `dsh.profile.bundles`；
5. 保留旧 tarball直到新构建、manifest 和入口验证完成；
6. 成功并越过回滚窗口后再清理不再引用的旧 artifact。

不从当前 Homebrew Cellar、版本化解压目录或源码 checkout 建立长期 `file:` 引用。

### 5.3 运行时归属

```text
Tessivum release
  └── first-party market tarball
        ├── Host half: Bun Compat Host / Cordis Fiber
        └── Browser half: existing client graph

Market mutation
  → desktopPnpm
  → Rust PnpmProfileBoundary
  → one writable Profile
```

市场仍是可卸载插件；移除它不能删除其安装过的其他插件。Core 不因市场缺失而改变 Agent、Session、Native/WASM 或普通 Web 行为。

## 6. 市场身份与目录模型

### 6.1 Host 品牌描述

Compat Host 提供一个可选、只读的 Cordis 服务：

```ts
interface HostLifecycle {
  readonly product: {
    readonly name: 'Tessivum'
    readonly command: 'tessivum web'
  }

  restart(): Promise<{ accepted: true }>
}
```

市场使用该服务生成产品文案。服务不存在时不能猜测 Tessivum，也不能从 `ctx.baseUrl`、profile 名或 argv 推导品牌。

### 6.2 用户可见文案

必须使用 Tessivum 的语境：

- `发现可在 Tessivum 中使用的社区插件`；
- `重启 Tessivum 后生效`；
- `等待 Tessivum 启动超时`；
- 手动兜底命令 `tessivum web`；
- 主标题版本写作 `市场组件 vX`，不把市场版本伪装成 Tessivum 版本。

源码来源在“关于/源码”位置显示，不占用 Host 品牌位置。

### 6.3 两类目录来源

第一版不建立新的远程目录服务。市场在客户端合并：

```text
现有 DSH 社区 registry snapshot/API
              +
随 Tessivum release 固定的第一方 catalog.json
              ↓
       统一查询、分类和展示
```

每张卡片必须标记：

- `Tessivum 官方`：进入固定第一方 catalog 并随发行完成测试；
- `DSH 社区兼容`：来自社区目录，且存在通过的 Tessivum 版本矩阵；
- `DSH 社区`：可尝试安装，但没有 Tessivum 兼容承诺。

“官方”只描述发布所有权，不等于 Native/WASM 沙箱或无风险。Legacy Node 插件继续以用户权限运行。

## 7. 确定性更新与新版本等待状态机

### 7.1 检查阶段冻结目标

更新检查返回：

```json
{
  "packageName": "example-plugin",
  "installedVersion": "1.2.0",
  "targetVersion": "1.3.0",
  "targetSource": "npm",
  "registryOrigin": "https://registry.npmjs.org/",
  "publishedAt": "2026-08-30T15:42:41Z"
}
```

mutation 使用精确目标：

```text
pnpm add example-plugin@1.3.0
```

禁止重新拼接 `@latest`。如果 dist-tag 在下载期间前移，新版本留给下一次检查；一次操作只安装用户确认的目标。

### 7.2 等待期结果

普通操作遇到尚未满足等待期的精确目标时，返回：

```json
{
  "ok": false,
  "failureCode": "RELEASE_TOO_FRESH",
  "retryable": true,
  "targetVersion": "1.3.0",
  "retryAfter": "<absolute timestamp>"
}
```

UI 显示：

- 目标版本和发布时间；
- 等待结束的绝对时间；
- “届时重试”；
- “立即安装”显式动作。

显式强制请求必须同时携带本次检查得到的精确 `targetVersion`。服务端重新确认它仍是用户看到的目标后，只给这一条 pnpm 命令增加：

```text
--config.minimumReleaseAge=0
```

不修改全局 `.npmrc`、profile 配置或后续操作默认值。

### 7.3 Registry 分歧

若目标已经超过等待期，但实际版本仍低于目标，返回：

```text
REGISTRY_DIVERGENCE
```

这与 `RELEASE_TOO_FRESH` 分开处理：

- 不把镜像延迟谎报成安全等待；
- 不自动接受较旧版本；
- 不无限 force retry；
- 保留 registry origin、expected/actual 和 bounded pnpm 日志供诊断；
- 恢复精确的更新前 source identity。

### 7.4 事务与回滚

每次 add/update/remove 继续遵守：

```text
Profile lock
→ capture manifest + lock/source identity
→ pnpm mutation
→ verify actual version/source
→ verify package entry/client bundle
→ composition trial validation
→ reconcile dependencies + bundles
→ publish terminal result
```

任一步失败：

1. 恢复 manifest；
2. 重装更新前精确来源；
3. 验证实际文件、入口和 composition；
4. 只有验证成功才报告“已自动恢复”；
5. 恢复失败时 fail loud，并阻止重启按钮把损坏 profile 带入下一次启动。

## 8. Host-owned 重启

### 8.1 Rust wire

产品 DomainBridge 增加可选服务：

```text
hostLifecycle@1.restart
```

约束：

- 只在 `tessivum web` 注册；
- Headless、SDK 和无 Web listener 的 Host 返回 `SERVICE_UNAVAILABLE`；
- Node 只提交固定 restart 请求，不能传 executable、argv、cwd、env 或 shell command；
- 重复请求幂等，只安排一次重启；
- 有 `AgentStatus::Running` 的 Agent 时返回 `HOST_BUSY`；
- Host 已进入 shutdown 时返回稳定的不可重试结果。

### 8.2 Web 主循环

`run_web` 同时等待 OS signal 和 restart coordinator：

```text
restart accepted
→ stop new HTTP admission
→ drain current Market response
→ shutdown ApiServer
→ shutdown Host/Legacy/WASM resources
→ relaunch exact Tessivum invocation
```

平台行为：

- Unix/macOS/Linux：优雅关闭后 `exec` 当前可执行文件，保留 PID 和 supervisor 所有权；
- Windows：监听端口释放后 spawn 相同 executable/args/cwd/env，父进程退出；
- relaunch 失败必须写入稳定、可定位的错误日志并返回非零退出，不静默消失。

市场继续使用 boot ID 判断新进程已经接管；不把固定 sleep 当作成功证据。

### 8.3 手动与自动模式

市场保留：

- `立即重启`；
- `所有插件操作完成后自动重启` 开关，默认关闭。

自动重启只有在以下条件全部满足时触发一次：

1. 用户已显式启用；
2. mutation 队列为空，且没有 queued/running 操作；
3. 至少一个成功结果明确要求 restart；
4. 没有失败、回滚中、build approval 或兼容风险待处理；
5. Host 确认没有运行中的 Agent；
6. 当前 boot 尚未安排过自动重启。

Host 拒绝时保留 banner 和手动按钮，不在后台循环重试，也不取消 Agent。

## 9. 从 `dshmarket` 清洁迁移

旧市场和第一方市场不能同时活动，因为二者会注册相同 `/dsh-market/*` 路由和 Browser section。

迁移必须是显式、事务化操作：

1. 检测 profile 中的 `dshmarket` / `dsh-market` 包和 bundle；
2. 校验第一方 tarball 与其入口；
3. 快照旧 manifest、lockfile、市场状态和禁用/分组数据；
4. 停止当前 mutation admission；
5. 安装第一方市场并将 bundle 顺序中的旧 ID原位替换为新 ID；
6. 运行 composition trial，确认没有重复 route/entry/section；
7. 移除旧包；
8. 保留兼容状态目录或执行有版本的状态迁移；
9. 提示重启；
10. 重启后验证只存在一个 Host Fiber、一个 Browser section 和一组路由。

若第 5–8 步任一失败，恢复旧市场的精确包版本、bundle 位置和状态；不能留下两个市场或没有市场的半迁移状态。

第一方市场稳定发布前，原 `dshmarket@1.29.2` 继续作为兼容 fixture，不从测试矩阵删除。

## 10. 安全边界

- 所有 `/dsh-market/*` mutation 继续经过 Rust authority、Origin/Host、prefix、body、response 和 deadline 限制；
- restart 只接受当前同源 loopback UI 触发的市场路由请求；Node 服务本身不获得通用进程控制；
- pnpm target 必须来自已验证目录项、精确用户输入或固定第一方 tarball；
- lifecycle/build scripts 默认拒绝，只能通过现有精确 allowlist 显式放行；
- catalog 描述、README、截图和 release notes 都是不可信远程内容，不能变成 HTML、shell 或文件路径；
- Legacy Node 市场及其安装的 Node 插件不是沙箱；UI 必须继续显示第三方代码信任提示；
- backup/restore 不扩大 secret 范围，现有 size limit 和 credential 警告继续生效；
- 第一方 catalog 是发行输入，必须进入 source audit、hash、许可证和 release inventory。

## 11. 实施里程碑

### 11.1 Alpha.17-A：第一方市场包

1. 固定 `dshmarket@1.38.1` 的 tag/commit、tarball integrity 和许可证；
2. 导入到 `plugins/market`，保留来源记录；
3. 切换 package/plugin identity 和用户可见 Tessivum 文案；
4. 删除 self-update/self-uninstall 产品路径；
5. 加入固定第一方 catalog overlay 和来源 badge；
6. 构建可复现 `.tgz` 并进入 release inventory；
7. 保持普通 DSH 社区 registry、diagnostics、backup 和 Profile mutation 行为。

### 11.2 Alpha.17-B：更新正确性

1. 将 npm mutation 目标冻结为检查阶段的精确版本；
2. 分离 `RELEASE_TOO_FRESH` 与 `REGISTRY_DIVERGENCE`；
3. 为等待期结果提供一次显式 force retry；
4. 对 actual < target、actual > target、dist-tag 前移、镜像延迟和版本不可比较建立测试；
5. 收紧 rollback：恢复 source identity 后再次验证入口和 composition；
6. 把“目标 `1.38.1`、实际 `1.37.0`”固化为回归场景。

### 11.3 Alpha.17-C：Host 生命周期

1. 冻结 `hostLifecycle@1.restart` DTO 和错误码；
2. 在 Rust Host 增加 restart coordinator 和运行中 Agent guard；
3. 在 Compat Host 发布 `hostLifecycle` 产品描述与 restart facade；
4. 将市场 `/restart` 委托给 Host；
5. 实现 Unix exec、Windows spawn 和失败日志；
6. 接入立即重启和 opt-in 队列完成后自动重启；
7. 验证 boot ID、端口接管、会话/Profile 持久化和无重启循环。

### 11.4 Alpha.17-D：迁移与发布

1. 实现旧市场到第一方市场的事务迁移；
2. 验证旧状态、分组、禁用清单和 bundle 顺序；
3. 从源码、发行归档和 Homebrew 安装第一方市场；
4. 运行真实 Browser 安装、更新、等待、force、rollback、重启和恢复场景；
5. 更新 README、兼容矩阵、许可证清单、Changelog 和发行说明；
6. 发布后重新下载四平台产物并重复 market smoke。

## 12. 文件与仓库影响地图

| 工作 | `tessivum` | `tessivum-core` | 外部依赖 |
|---|---|---|---|
| 第一方市场源码 | `plugins/market/**` | 无 | 固定 dshmarket 来源，只读导入 |
| 市场 Browser bundle | release build、client graph、Browser tests | 无 | 冻结 rc.5 Browser compatibility plane |
| Profile mutation | `src/plugin_manager.rs`、tests | 无 | pnpm |
| Host 生命周期 wire | `src/bridge.rs`、`src/host.rs` | 通用 Node frame不变 | 无 |
| Cordis facade | 产品配置/服务权限 | `node/compat-host/src/host.ts` 与测试 | 无 |
| Web relaunch | `src/bin/tessivum.rs`、process smoke | 无 | OS process API |
| 发行 tarball | `scripts/package_release.sh`、inventory、licenses | 无 | 无浮动下载 |
| 旧市场迁移 | Plugin Manager、Profile tests、release smoke | 无 | 已安装 dshmarket fixture |

`hostLifecycle@1` 是 Tessivum 产品领域服务，不进入通用 Cordis Core API。只有 Node transport/facade 的通用承载修改可以进入 `tessivum-core`。

## 13. 验证矩阵

### 13.1 市场来源与品牌

| 场景 | 必须证明 |
|---|---|
| 无 `hostLifecycle` | 不猜 Tessivum，不开放 Host restart |
| Tessivum Host | 标题、subtitle、重启、超时和终端警告使用 Tessivum |
| 协议身份 | `/dsh-market`、update-api v1、`dsh.*` manifest 保持兼容 |
| 来源归属 | UI/源码/发行许可证保留 dshmarket MIT 来源 |
| 两类 catalog | 第一方与 DSH 社区来源可区分，未验证项不显示“兼容” |

### 13.2 更新与回滚

| 场景 | 必须证明 |
|---|---|
| 普通成熟版本 | 安装检查阶段冻结的 exact target |
| 新发布版本 | 返回 retryAfter，不安装较旧替代版本 |
| 显式 force | 只对一次 exact target 绕过等待，不改全局配置 |
| `1.38.1 → 1.37.0` 复现 | 失败、恢复原构建、UI 提供正确下一步 |
| Registry 分歧 | 独立错误，不伪装 release-age，不无限重试 |
| Dist-tag 前移 | 本次仍安装用户确认的版本，下次检查再提示 |
| Broken entry/composition | 回滚后再次验证，失败则阻止重启 |
| Cancel | 终止进程树、释放 Profile lock、报告 partial state |

### 13.3 重启

| 场景 | 必须证明 |
|---|---|
| 立即重启 | HTTP 响应完成后才关闭 listener |
| 自动重启 | 队列完全排空后只触发一次 |
| Agent running | Host 返回 `HOST_BUSY`，Agent 不被取消 |
| Unix | graceful shutdown 后 exec，相同 argv/cwd/env |
| Windows | 端口释放后 spawn，父进程退出，无可见额外控制台 |
| Relaunch failure | 稳定日志、非零退出，不报告成功 |
| 新 boot | boot ID 改变、页面恢复、Session/Profile 保持 |
| 重复请求 | 只存在一个 replacement process |

### 13.4 迁移与发行

| 场景 | 必须证明 |
|---|---|
| 未安装旧市场 | 第一方市场正常启用 |
| 已安装旧市场 | 原位替换 bundle，不出现重复 route/section |
| 迁移失败 | 精确恢复旧包、bundle 顺序和状态 |
| 无 Bun/pnpm | Core Web 继续工作；启用市场时报具体依赖错误 |
| Homebrew upgrade | profile 的 first-party tarball 路径不指向已删除 Cellar |
| 四平台归档 | tarball hash、许可证、client bundle 和 Host entry 齐全 |

## 14. 强制验证与发布门槛

代码完成后至少运行：

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cd web && bun install --frozen-lockfile && bun run build && bun run test
cd plugins/market && bun install --frozen-lockfile && bun run check
```

还必须完成：

1. Core Node Compat Host 的 `hostLifecycle` 注册、转发、generation cleanup 测试；
2. 真实 Rust Web Host + Bun + Chromium 的市场视觉和行为 E2E；
3. 本地 registry fixture 对 fresh/mature/divergent 三种版本时间线的确定性验证；
4. 子进程级 restart smoke，观察旧 listener 关闭、新 boot 接管和会话恢复；
5. 发行归档内 first-party tarball 的离线安装与旧市场迁移；
6. Homebrew upgrade 后清理旧 Cellar，再执行一次 market mutation；
7. 发布后从 GitHub Release 下载成品重复 hash、安装、迁移和 restart smoke。

测试不得通过修改 `node_modules`、静态替换错误文案、跳过 pnpm、伪造 boot ID 或 mock 成功响应宣称完成。

## 15. 完成定义

Phase 7 只有在以下全部满足后才能标记完成：

1. 第一方市场源码、来源、许可证、构建和 tarball 都由 Tessivum 仓库固定并可复现；
2. 未安装市场时 Tessivum Core Web 不新增 Bun/pnpm 强依赖；
3. 市场用户文案使用 Tessivum，兼容协议和第三方包真实身份未被重命名；
4. DSH 社区目录和 Tessivum 第一方目录在同一 UI 中具有明确来源与兼容级别；
5. npm mutation 使用用户确认的 exact target，不把 actual < target 当作成功；
6. 新版本等待结果具有绝对 retryAfter 和显式单次 force；
7. 任意更新失败后的“已恢复”都有 manifest、文件、入口和 composition 验证证据；
8. Rust Host 完成立即重启和 opt-in 自动重启，运行中的 Agent 不被中断；
9. 旧 `dshmarket` 可事务迁移，重启后只有一个市场 Fiber、Browser section 和 route owner；
10. 源码、Browser、真实 pnpm、进程重启、发行归档和 Homebrew 场景全部通过；
11. README、兼容矩阵、Changelog 和许可证清单只陈述真实完成范围；
12. `dshmarket@1.29.2` 在第一方市场稳定发布前继续通过既有兼容回归。

## 16. 风险登记

| 风险 | 控制 |
|---|---|
| 上游改动无法合并 | 第一方固定源码和显式 vendor bump，不依赖 upstream merge |
| Fork 漂移且难以维护 | 只导入市场真实使用面；每次 bump 生成来源 diff 和完整 E2E |
| 品牌修改掩盖来源 | Host 品牌与源码归属分离，保留 MIT 许可证和来源页 |
| 市场变成 Core 强依赖 | 保持可选 Profile plugin；无市场时不启动 Legacy Host |
| 本地 tarball 路径随升级失效 | 先复制到稳定数据根，再写 Profile spec |
| 新版本等待被全局关闭 | force 只作用于用户确认的单次 exact target |
| Registry metadata 与 pnpm 结果分歧 | expected/actual 校验、独立错误码、事务回滚 |
| 市场更新自己导致运行时代码混合 | 第一方市场不进入自身 mutation 队列，随 Tessivum release 更新 |
| 两个市场注册相同路由 | 原位 bundle 替换、trial validation、失败恢复 |
| 自动重启中断工作 | 默认关闭、队列 gate、Agent running guard、Host-owned shutdown |
| supervisor/端口接管失败 | Unix exec、Windows listener-drain spawn、真实进程 smoke |
| 兼容声明膨胀 | 第一方/已验证社区/未验证社区三态，固定版本矩阵继续有效 |

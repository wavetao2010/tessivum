# Tessivum Phase 6 DSH Profile 兼容与 `tsv` 命令开发计划

> 状态：实现与本地验收已完成，Alpha.15 公开发行验证进行中
> 计划日期：2026-08-28
> 实现基线：`v0.1.0-alpha.14`
> 目标发布：`v0.1.0-alpha.15`
> Core 基线：`tessivum-core v0.1.6` / `3571b75dd79bdcf658d8ad6b86da63005431b21e`
> 上游兼容基线：DeepSeek Harness `0.1.0-rc.5` / `47f943859bef60e4160492346772ded9b24f765a`
> 固定社区兼容目标：`dshmarket@1.29.2`、`dsh-better-sidebar@0.16.1`

## 1. 文档目的

本阶段统一 Tessivum 的插件安装记录、Host Bundle 激活顺序、Loader 运行状态与 dshmarket 展示状态，并增加 `tsv` 官方短命令。它解决三个相互关联的问题：

1. `dsh.profile.bundles` 尚未成为 Tessivum Profile 的权威 Host Bundle 记录；
2. dshmarket 按上游 Profile/Loader 契约判断状态，而 Tessivum 使用依赖驱动加载，导致插件已经运行但市场显示“已安装，未生效”；
3. 发行物只有规范命令 `tessivum`，没有方便且不冒充上游 DSH 的短命令。

关联文档：

- [二阶段开发计划](DEVELOPMENT_PLAN.md)：仓库边界、工作顺序与总体验收原则；
- [插件生态兼容方案](PLUGIN_COMPATIBILITY.md)：Native/WASM/Legacy Node/Browser 插件兼容范围；
- [Phase 4 计划](PHASE4_BRAND_DISTRIBUTION_MARKET_PLAN.md)：安装分发、pnpm Profile 与 dshmarket 初始兼容；
- [Phase 5 计划](PHASE5_NATIVE_AGENT_MODES_PLAN.md)：Agent Mode 与插件 Profile 的独立边界；
- [DeepSeek Harness 兼容基线](COMPATIBILITY_BASELINE.md)：冻结 Browser/Wire 契约。

本文只定义计划和验收门槛；状态改为“已完成”前，不能据此宣称实现已经落地。

## 2. 当前事实与共同根因

### 2.1 当前安装与加载路径

终端入口：

```text
tessivum plugin add/remove
  → Rust PluginMutation
  → pnpm add/remove
  → ${TESSIVUM_HOME:-$HOME/.tessivum}/plugins
```

市场入口：

```text
dshmarket
  → desktopPnpm.runPlugin
  → Compat Host pnpm.run
  → Rust PnpmProfileBoundary
  → pnpm add/remove/install
```

两条路径操作同一个 pnpm Profile，但当前成功结果主要体现为 `package.json.dependencies`、`pnpm-lock.yaml` 和 `node_modules`，没有完整维护上游 `dsh.profile.bundles` 语义。

### 2.2 当前 Loader 与 Browser 行为

当前产品启动时：

- Rust Plugin Manager 遍历顶层 `dependencies`；
- 包声明 `dsh.bundle.patch` 时直接应用该 patch；
- 未声明 bundle 的可路由 npm 包可能被直接解析为 Legacy Node Entry；
- Browser 独立扫描已安装包的 `dsh.client` 并发布 client-half；
- 插件变更统一要求重启，不建立第二套热挂载 Loader 权威状态。

该模型可以让插件实际运行，但它与 dshmarket 的上游判断模型不同。

### 2.3 市场误判

dshmarket 的激活判断同时读取：

- `package.json.dsh.profile.bundles`；
- 当前 Cordis Loader 中具有活动 Fiber 的 Entry；
- 市场自身的临时 hot-mount/shim 状态。

`dsh-better-sidebar@0.16.1` 实际声明：

```json
{
  "dsh": {
    "bundle": {
      "patch": "./cordis.patch.yml"
    },
    "client": {
      "platform": "web"
    }
  }
}
```

当 Tessivum 已按依赖直接加载它、但 Profile 未记录 `dsh.profile.bundles`，市场可能落入“未声明 dsh.bundle、纯客户端插件”的兜底分支。该提示在此场景下不是运行事实，而是两套权威状态不一致的结果。

### 2.4 当前 CLI 边界

当前规范入口包括：

```text
tessivum web
tessivum sdk
tessivum plugin add
tessivum plugin remove
tessivum plugin-report
```

当前没有 `tsv`、`tv` 或全局 `dsh` alias。README 明确不暴露全局 `dsh` shim，避免把受限兼容误导成完整 DSH CLI 兼容。

### 2.5 共同根因

> 安装清单、Bundle 激活顺序、实际 Loader inventory 和市场展示没有读取同一份 Profile 权威状态。

因此，本阶段不能只修改市场文案，也不能只补一个无人消费的 `bundles` 字段；必须统一写入、启动和观察契约。

## 3. 阶段目标

完成后，以下四项必须一致：

1. **安装事实**：包是否存在于 `dependencies` 和 pnpm lockfile；
2. **启用事实**：Host Bundle 是否存在于 `dsh.profile.bundles`；
3. **运行事实**：对应 Loader Entry/Fiber 是否在当前 generation 中活动；
4. **展示事实**：dshmarket 显示已生效、重启后生效、未生效、校验失败或已停用。

目标数据流：

```text
tessivum plugin / dshmarket
              │
              ▼
   统一的 Profile Mutation
              │
       ┌──────┴──────┐
       ▼             ▼
pnpm dependencies   dsh.profile.bundles
       │             │
       └──────┬──────┘
              ▼
      Tessivum Loader
              │
              ▼
   dshmarket 读取真实状态
```

## 4. 冻结决策

### 4.1 Profile 权威字段

统一格式：

```json
{
  "name": "tessivum-plugins",
  "private": true,
  "dependencies": {
    "dshmarket": "1.29.2",
    "dsh-better-sidebar": "0.16.1"
  },
  "dsh": {
    "profile": {
      "bundles": [
        "dshmarket",
        "dsh-better-sidebar"
      ]
    }
  }
}
```

字段语义：

| 字段 | 权威含义 |
|---|---|
| `dependencies` | Profile 已安装的顶层 npm 包 |
| `dsh.profile.bundles` | 已启用的 Host Bundle 包名及应用顺序 |
| Loader Entry/Fiber | 当前进程实际运行状态 |
| Browser boot graph | 当前启动发布的 `dsh.client` client-half |

### 4.2 Bundle 激活规则

1. 声明 `dsh.bundle.patch` 的包只有位于 `dsh.profile.bundles` 时才进入 Host Bundle 层。
2. `bundles` 数组顺序就是 patch 应用顺序；不得排序、去重后静默改写用户顺序。
3. 同一名称重复、未知依赖、缺少 bundle 声明、越界 patch 路径和无效 patch 必须 fail-loud。
4. 同时声明 `dsh.bundle` 与 `dsh.client` 的包，其 Browser half 只能在对应 Bundle 启用时发布，避免“Host 已停用但 UI 仍存在”。
5. 只有 `dsh.client`、没有 `dsh.bundle` 的真正纯客户端包不进入 bundles；其通用市场状态兼容不在本阶段扩展到未经验证的任意包。
6. Native、WASM、内置 Agent Mode 和 Rust Host 服务不进入 npm bundles。

### 4.3 安装与启用是不同事实

- 在 `dependencies` 中但不在 bundles：已安装、未启用；
- 在 bundles 中但当前进程尚未加载：重启后生效；
- 在 bundles 中且 Loader Fiber 活动：已生效；
- 从 bundles 删除不等于卸载 npm 包；
- 从 dependencies 删除时必须同步删除对应 bundle 记录。

### 4.4 重启与热挂载

本阶段继续使用 restart-required 模型：

- CLI、市场安装、更新、删除或启停操作写入持久 Profile；
- 当前进程不创建第二套 Node Loader truth；
- 下次 `tessivum web`/`tsv web` 按新 Profile 启动；
- 不为了消除提示而实现上游 Include 热挂载子树。

### 4.5 CLI 命名

- `tessivum` 保持唯一规范产品命令；
- `tsv` 是同一二进制的官方便利链接；
- 不创建第二套 Parser、Wrapper Runtime 或分叉配置；
- 不提供 `dsh` alias/shim。

## 5. 现有 Profile 迁移

### 5.1 触发条件

启动或首次插件 mutation 时：

- `dsh.profile.bundles` **不存在**：执行一次迁移；
- 字段存在且为数组：视为用户权威配置；
- 字段存在但类型错误：明确拒绝，不能覆盖；
- 显式空数组：保持空，不自动重新启用插件。

“字段不存在”和“字段为空”是两个不同状态，不增加额外迁移版本字段。

### 5.2 推导规则

迁移扫描当前顶层 dependencies：

1. 按当前 Loader 已使用的依赖遍历顺序读取；
2. 只选出安装包 manifest 明确声明 `dsh.bundle.patch` 的包；
3. 保持相对顺序写入 bundles；
4. client-only 和普通依赖不写入；
5. 任一包 manifest 或 bundle 路径损坏时迁移失败，原文件保持不变。

该规则确保 Alpha.14 已实际加载的固定社区 Bundle 在 Alpha.15 首次启动后仍然加载。

### 5.3 持久安全

迁移必须：

- 复用 Profile 独占锁；
- 在同目录写临时文件；
- flush/sync 后原子 rename；
- 保持原文件权限；
- 失败时删除临时文件；
- 不修改 session、settings、credentials 或插件版本；
- 不在 Node Compat Host 中直接写 Profile 主文件。

### 5.4 回滚性质

Alpha.14 忽略 `dsh.profile.bundles` 并继续按 dependencies 加载，因此 Alpha.15 写入该字段后回滚二进制不会删除插件数据。回滚前仍须按 Alpha 发布规则备份完整数据根；不承诺新版本写入的其他数据格式可由旧版本修改。

## 6. 统一 Profile Mutation

### 6.1 单一实现边界

CLI 与市场必须调用同一套 Rust Profile mutation/reconciliation 逻辑：

```text
CLI PluginMutation ─────────────┐
                               ├─ Profile lock → pnpm → reconcile → atomic manifest
Market PnpmProfileBoundary ─────┘
```

不得在 CLI、Rust Bridge 和 dshmarket 三处分别维护 bundle 推导算法。

### 6.2 操作前快照

在 pnpm 前记录：

- 顶层 dependencies 名称与 spec；
- bundles 数组及顺序；
- 目标包当前是否声明 bundle；
- package.json 与 lockfile 的基本可读状态。

快照只用于确定增删变化和失败诊断，不复制整个 `node_modules`。

### 6.3 成功后的 reconciliation

- **新增依赖**：若声明 bundle，追加到 bundles 尾部；若 client-only，不追加；
- **删除依赖**：从 bundles 删除同名项；
- **更新现有依赖**：保持操作前的启用/停用成员关系；
- **`pnpm install`/restore**：保留 manifest 已声明的 bundle 顺序，只剔除已不存在的依赖；
- **无关 mutation**：不得把用户已从 bundles 移除的插件重新加回；
- **重复操作**：结果幂等。

不能每次扫描后把所有声明 bundle 的包全部补入数组；那会重新启用用户主动停用的插件。

### 6.4 失败语义

- pnpm 非零退出、取消或超时：不提交新的 bundle 变更；
- pnpm 已部分修改文件：继续返回现有 partial-state 诊断；
- pnpm 成功但 manifest reconciliation 失败：明确报告“包已变更、Profile 激活记录未提交”，不能伪装回滚成功；
- package.json 原子替换失败：保留替换前文件；
- mutation 全程持有同一个跨 CLI/市场独占锁。

## 7. Loader Clean Cutover

### 7.1 Host Bundle 构造

`load_plugin_entries` 从“遍历所有 dependencies 并自动应用 bundle”切换为：

1. 读取并验证 Profile；
2. 完成必要的一次迁移；
3. 按 `dsh.profile.bundles` 顺序定位包；
4. 读取每个包声明的 patch；
5. 依次构造 Entry Tree；
6. 由现有真实 Cordis Legacy Node Host 启动 Fiber。

已安装但不在 bundles 的包不得偷偷进入 Host Loader。

### 7.2 Loader inventory

每个活动 Entry 必须保留 dshmarket 可观察的：

- `entry.options.id`；
- `entry.options.name`；
- `entry.fiber` 活动状态；
- generation-scoped 生命周期。

如果满足 bundles 契约后，dshmarket 仍无法从真实 `host.loader.entries()` 观察活动插件，才在 Compat Host 增加最小、通用的 Loader inventory 适配；不得为某个插件名称伪造市场响应。

### 7.3 Browser graph

- Agent Mode 不决定 Profile 级 Browser 插件是否安装；
- 对 bundle+client 包，Browser half 跟随 Profile Bundle 启用状态；
- 对固定兼容目标，Host 与 Browser 必须在同一次重启后同时生效或同时停用；
- Browser 仍只消费静态/已安装 client artifact，不把 Node 引入浏览器主路径；
- generic client-only shim/hot-mount 扩展不属于本阶段完成条件。

## 8. dshmarket 状态闭环

### 8.1 不修补文案掩盖问题

优先满足 dshmarket `1.29.2` 已有 Profile/Loader 契约，不直接把“未生效”字符串替换为“已生效”，也不在 HTTP 响应中无条件重写 activation。

### 8.2 目标状态矩阵

| Profile/Runtime 状态 | 市场显示 |
|---|---|
| bundle 已加载且 Fiber 活动 | 已生效 |
| bundle 已写入但当前 generation 尚未加载 | 重启后生效 |
| 包已安装但未进入 bundles | 已安装，未生效 |
| manifest、入口或 patch 损坏 | 校验未通过 |
| 用户已停用 | 已停用 |
| 包不存在 | 未安装 |

### 8.3 固定场景

`dsh-better-sidebar@0.16.1` 必须满足：

1. 安装后 dependencies 与 bundles 同时正确；
2. 重启前显示“重启后生效”；
3. 重启后 Host `/sidebar/api` 路由与 Browser Sidebar 同时存在；
4. 市场显示“已生效”；
5. 不再显示“未声明 dsh.bundle”；
6. 停用并重启后 Host 路由和 Browser UI 都消失；
7. 重新启用并重启后恢复；
8. 卸载后 dependencies、bundles、Loader Entry、路由和 Browser client-half 都消失。

## 9. `tsv` 官方短命令

### 9.1 行为

以下命令必须等价：

```bash
tessivum web
tsv web

# 等价
tessivum plugin add dshmarket@1.29.2
tsv plugin add dshmarket@1.29.2
```

`tsv` 不产生独立配置、数据目录、日志命名或进程身份；`--help` 可以继续显示规范产品名 `tessivum`。

### 9.2 分发

- Release archive：`bin/tsv` 为指向 `tessivum` launcher 的相对链接；
- Homebrew Formula：安装同一 launcher 的 `tsv` symlink；
- no-sudo installer：原子创建并在卸载时删除受管 `tsv` 链接；
- 源码开发：继续以 `cargo run -- ...` 为规范入口，不创建第二个 Rust binary target。

### 9.3 冲突策略

`TSV` 常用于 Tab-Separated Values，系统可能已有同名命令。安装器必须：

- 只覆盖自己管理、且目标位于当前 Tessivum 安装根的链接；
- 遇到普通文件、其他链接或其他包管理器所有的 `tsv` 时明确报告冲突；
- 不删除、不改名、不静默覆盖第三方命令；
- 即使 `tsv` 冲突，规范命令 `tessivum` 的安装结果也必须明确说明，不能留下含糊的半成功状态。

### 9.4 不提供 `dsh`

`dsh` 仍是上游产品命令名。Tessivum 只实现冻结的 Profile、Browser 和 DomainBridge 兼容面，不复制完整 DSH CLI，因此不得安装 `dsh` shim。

## 10. 实施切片与文件边界

### 10.1 Profile 与 Loader

主要修改：

- `src/plugin_manager.rs`：Profile schema、迁移、共享 reconciliation、Bundle 顺序加载；
- `src/bin/tessivum.rs` / `src/frontend.rs`：按激活状态发布固定 bundle+client 包；
- 现有 Plugin Manager、Web integration 测试：补充迁移、CLI/市场共享 mutation 和 Browser graph 契约。

不创建新的 Profile 服务框架；现有 Plugin Manager 是唯一所有者。

### 10.2 Compat Host

仅当真实 E2E 证明 Loader inventory 仍不完整时修改：

- `tessivum-core/node/compat-host/src/host.ts`：通用 Loader inventory 适配；
- 对应 Core 协议/生命周期测试。

不修改 vendored dshmarket 的状态字符串，不为 `dsh-better-sidebar` 硬编码名称。

### 10.3 分发

主要修改：

- `scripts/package_release.sh`：归档 `tsv` 链接；
- `packaging/homebrew/tessivum.rb.in`：Homebrew alias；
- `install.sh`：碰撞安全的创建、升级与卸载；
- release/install 脚本测试：四 target inventory 与冲突场景。

### 10.4 文档

实施完成时同步：

- `README.md`：`tsv`、Profile bundles 和 restart-required 说明；
- `CHANGELOG.md`：Alpha.15 Added/Changed/Fixed；
- `PLUGIN_COMPATIBILITY.md`：安装、启用与 Browser/Host 边界；
- `COMPATIBILITY_BASELINE.md`：固定市场状态与发行验证证据；
- 本文状态和真实发布证据。

## 11. 验证计划

### 11.1 Profile 单元契约

1. 缺少 bundles 的旧 Profile正确迁移；
2. 显式空 bundles 不被自动填充；
3. 新增 bundle 包追加且顺序稳定；
4. client-only 包不追加；
5. 删除包同步删除 bundle；
6. 更新包不改变启用状态；
7. unrelated mutation 不重新启用插件；
8. duplicate/unknown/malformed bundles fail-loud；
9. 原子写失败不损坏原 manifest；
10. CLI 与市场路径产生相同结果。

### 11.2 Loader 与生命周期

1. 只加载 bundles 中的 Host patch；
2. 严格遵守数组顺序；
3. 未启用包没有 Fiber、路由或工具残留；
4. Node crash、restart 和 shutdown 后 inventory 无陈旧 generation；
5. 重启后 Loader entries 可被 dshmarket 观察；
6. bundle 表达式与重复挂载防护不回退。

### 11.3 真实社区插件

在真实 Browser 场景验证：

```bash
tsv plugin add dshmarket@1.29.2
tsv plugin add dsh-better-sidebar@0.16.1
tsv web
```

必须观察：

- 市场页面存在；
- Sidebar 文件、终端、Git 和侧边对话入口存在；
- Host 路由真实工作；
- `pageerror=[]`；
- 受监控 `console.warn/error=[]`；
- 市场对两个固定插件显示“已生效”；
- 刷新和进程重启后状态保持；
- 停用、重新启用和卸载状态准确。

### 11.4 `tsv` 与安装器

四个平台发行归档验证：

- `tessivum --version`；
- `tsv --version`；
- 两个入口运行同一 launcher；
- Homebrew 安装、升级和卸载；
- no-sudo 安装、升级和卸载；
- 已存在外部 `tsv` 时拒绝覆盖；
- `tessivum` 与 `tsv` 使用同一数据根；
- 归档不包含第二份二进制。

### 11.5 升级回归

使用 Alpha.14 Profile 夹具：

1. 依赖已安装但无 bundles；
2. 首次 Alpha.15 启动完成原子迁移；
3. 两个固定社区插件继续工作；
4. 市场状态由误报变为真实状态；
5. Session、Settings、Credentials 和插件版本不变；
6. 回滚 Alpha.14 后程序仍能读取同一插件 Profile。

## 12. 风险与控制

| 风险 | 后果 | 控制 |
|---|---|---|
| 只补 bundles、不切 Loader | 形成第二份装饰性状态 | bundles 成为 Host Bundle 唯一权威；E2E 同时检查文件与 Fiber |
| 每次 reconcile 全量补包 | 用户停用被静默撤销 | 只追加新增依赖；更新保持成员关系 |
| 迁移改变 Bundle 顺序 | patch 覆盖和重复挂载行为变化 | 保持当前 Loader 依赖遍历顺序；顺序回归夹具 |
| 市场状态硬编码修正 | UI 显示成功但 Runtime 未运行 | 读取真实 Profile 与 Loader inventory；禁止名称特判 |
| Browser 与 Host 启停分叉 | UI 存在但路由缺失 | 固定 bundle+client 包使用同一 Profile 激活 gate |
| pnpm 成功、manifest 写失败 | 安装与激活部分成功 | 原子 manifest；明确 partial-state；不宣称回滚 |
| `tsv` 覆盖现有 TSV 工具 | 破坏用户环境 | 所有安装渠道碰撞检测；永不覆盖非受管入口 |
| 冒充完整 DSH CLI | 兼容承诺失真 | 不创建 `dsh` shim；文档继续冻结版本矩阵 |
| 为热挂载引入第二 Loader truth | restart 后状态漂移 | 保持 restart-required；删除热挂载扩展诉求 |

## 13. 明确不做

- 不实现完整上游 `dsh plugin` CLI；
- 不安装全局 `dsh` shim；
- 不实现 Tessivum 自有 npm registry、semver 或 lockfile 解析器；
- 不创建第二套 Node Loader 或 Include hot-mount 权威状态；
- 不直接修改 dshmarket 文案掩盖状态不一致；
- 不宣称任意 DSH/Cordis npm 插件兼容；
- 不在本阶段扩展 generic client-only shim 兼容矩阵；
- 不创建第二个 `tsv` Rust binary；
- 不改变 Agent Mode、Session 持久化或 Native/WASM ABI。

## 14. 发布与回滚

目标产品版本为 `v0.1.0-alpha.15`。发布顺序：

1. 若 Loader inventory 或 Node 生命周期需要 Core 改动，先发布下一版 `tessivum-core` 并固定 revision；
2. Tessivum 更新 Core、完成 Profile/Loader clean cutover；
3. 运行 Profile、Legacy、Browser 和发行包验证；
4. 同步 README、Compatibility Baseline、CHANGELOG 和本文状态；
5. 发布四平台归档、SHA-256、Formula 与 no-sudo installer；
6. 更新并验证 Homebrew Tap；
7. 真实执行 Alpha.14 → Alpha.15 升级与回滚 smoke。

Alpha.15 同时承载 Alpha.14 发布后已完成但尚未进入公开归档的侧边对话种子持久化、长时 pnpm 路由和 Legacy Node generation cleanup 修复；这些修复必须保留各自已有的定向测试，不与 Profile 兼容测试互相替代。

回滚以完整产品版本为单位，不保留运行时 feature flag 维持两套 Loader 语义。Profile 新增的 `dsh.profile.bundles` 对 Alpha.14 是可忽略字段；用户数据删除不属于二进制回滚。

## 15. 完成定义

Phase 6 只有同时满足以下条件才完成：

- `dependencies` 是安装清单，`dsh.profile.bundles` 是 Host Bundle 启用和顺序的唯一权威；
- 旧 Profile 在字段缺失时原子迁移，显式空数组不被覆盖；
- CLI 与 dshmarket 共享一个 Rust mutation/reconciliation 边界；
- Host Loader 只加载 bundles 中的包并暴露真实活动 Fiber inventory；
- 固定 bundle+client 插件的 Host 与 Browser 激活状态一致；
- `dshmarket@1.29.2` 和 `dsh-better-sidebar@0.16.1` 在市场中显示真实状态；
- `dsh-better-sidebar` 不再被误报为“未声明 dsh.bundle”；
- 安装、启停、重启、重新启用和卸载闭环通过真实 Browser 验证；
- `tsv` 在四平台归档、Homebrew 和 no-sudo 安装中作为碰撞安全的同二进制别名工作；
- 不提供 `dsh` shim，不引入第二套 Loader truth；
- Alpha.14 → Alpha.15 升级保留现有插件、Session 和设置；
- 侧边对话修复、Legacy lifecycle、Profile、Browser 和发行验证全部通过；
- GitHub Alpha.15 prerelease 与 Homebrew Tap 产物完成真实安装验证。

## 16. 实施证据

- `tessivum-core v0.1.6` 已发布并固定到 revision `3571b75dd79bdcf658d8ad6b86da63005431b21e`；Core 全目标格式、Clippy、29 个 Rust 测试套件 116/116 与兼容 Host 12/12 通过。
- Profile 迁移、显式空 bundles、顺序、CLI/市场统一 mutation、失败原子性和 Host inventory 定向契约 25/25 通过。
- Rust 全目标格式、Clippy 与 45 个测试套件 480/480 通过。
- `dshmarket@1.29.2` 与 `dsh-better-sidebar@0.16.1` 的真实 Browser 安装、停用、重启、启用和卸载闭环通过；每一步控制台、页面异常和请求失败均为 0。
- 上游 source Web 的 239 个测试文件、3163 个测试通过，类型检查通过；69/69 个 Chromium 迁移场景通过。
- 四平台发行归档、本地 Formula、no-sudo 安装、升级、碰撞拒绝和卸载夹具通过；归档内 `tsv` 与 `tessivum` 为同一文件，未复制第二个二进制。
- 真实 Alpha.14 Profile 已完成 Alpha.15 原子迁移及 Alpha.14 二进制回滚；pnpm lock、Session、Settings、Credentials 和 Workspace 文件哈希在升级前后保持一致。
- 剩余发布门槛仅为 GitHub Alpha.15 prerelease、Homebrew Tap 更新与公开产物真实安装；完成后再将本页状态改为“已完成”。

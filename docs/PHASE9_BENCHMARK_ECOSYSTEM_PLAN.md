# Tessivum Phase 9 性能证据与社区插件发布计划

> 状态：Phase 9-A 测量协议与三样本固定 Linux 试运行已完成，30 样本公开性能运行待完成；Phase 9-B 已用 `dsh-better-sidebar@0.16.1` 关闭社区发布/验证闭环
> 计划日期：2026-09-03
> Tessivum 测量目标：`v0.1.0-alpha.23` / `72a5f6104aaf35e19faa5d9897ec3cb845ad2ec0`
> Core Benchmark 基线：`tessivum-core v0.1.6` / `cedbeb9e1607056845b69e09b825eb7f5be67a69`
> 产品运行时 Core pin：`tessivum-core v0.1.6` / `bafb893f182d64b7b464b6cf827676f7ac368168`
> 上游对照基线：DeepSeek Harness `0.1.0-rc.5` / `47f943859bef60e4160492346772ded9b24f765a`

## 1. 文档目的

Phase 8 已关闭 Remote Access、通用 Node facade 与发行门槛。Phase 9 不继续扩张 Host 功能面，而是完成两项对公开采用同样重要的工作：

1. 用可复现横向 Benchmark 证明 Rust 迁移的实际收益，同时证明收益不是通过删除兼容功能获得；
2. 让第三方作者沿一条公开、可审核、无需 Tessivum 自建上传服务的路径进入内置市场，并把“目录收录”与“Tessivum 固定版本验证”明确分开。

Phase 9 开始前先统一独立品牌文案和中英文仓库入口。实现若偏离本文，先更新本文及关联兼容文档，不能让宣传口径、测试口径与代码行为分叉。

Phase 9 不预先绑定产品版本号；只有对应门槛通过后，才随当时的 Tessivum 版本发布结果或能力。

## 2. 当前事实

### 2.1 Benchmark 协议已实现，公开样本门槛尚未达到

`tessivum-core` 的旧 `docs/BENCHMARK_RESULTS.json` 已明确标为 historical/non-comparative：它只有 Rust、3 个 Darwin 样本，版本为 `0.1.1`，且 `context_fiber_memory_proxy` 只是浅层 `size_of`。

Phase 9-A 现在新增：

- TypeScript Cordis `4.0.1` 与 Rust Core 的共享工作量和 process-cold 配对 driver；
- Base/Compatibility 产品 manifest、真实 Chromium 固定 Replay、1/10 个已完成相同 Replay 的 resident Session；
- Linux `/proc/*/smaps_rollup` 完整 Host 后代进程树 PSS；
- 固定 Ubuntu 24.04 arm64 容器、失败样本保留、原始 JSON checkpoint 和 median/p95/min/max 汇总。

三样本固定 Linux 运行只验证协议和结果形状。达到每项 30 个有效 process-cold 样本前，README 与首页不得发布性能倍数。

### 2.2 内置市场已有可复用的社区目录

当前 `tessivum-market`：

- 每次请求重新验证 `awesome-dsh-plugin.com` 社区目录；
- 把 `plugins/market/catalog.json` 中的 Tessivum 第一方条目标为 `official`；
- 把社区条目标为 `dsh-community`；
- 已冻结 `official | verified | unverified` 三种兼容状态；
- 通过现有 pnpm Profile mutation、精确版本检查、恢复点和 Host-owned restart 完成安装与更新。

Phase 9-B 已补齐公开申请、固定版本 ledger、无密钥 CI、精确版本状态和撤销路径；社区目录仍是插件描述与发现的唯一真相。

## 3. 共享前置：独立品牌与双语入口

### 3.1 Slogan

Tessivum 的中文 Slogan 冻结为：

> **道器相成**

英文对应文案冻结为：

> **Principle and implementation, in concert.**

含义边界：

- “道”指可组合上下文、生命周期、权限与兼容原则；
- “器”指 Rust Host/Core、Agent、Session、Tool 与插件运行时；
- “相成”表达架构原则与可运行产品互相成立，不表达官方隶属或未经测量的性能承诺。

DeepSeek 的“探索未至之境”及其英文对应文案不得继续作为 Tessivum 的用户可见 Slogan。DeepSeek 名称只在以下事实场景保留：

- 兼容性基线与上游来源；
- DeepSeek Provider/API；
- `@deepseek-ai/*` 包、协议和环境变量；
- 许可证、NOTICE 与第三方归属。

品牌变更继续通过现有有界 Browser overlay 完成，不 fork React UI，不全仓替换 `deepseek`/`dsh` 协议字符串。必须覆盖中英文欢迎面、Sidebar/Hero、加载与空状态、PWA 元数据、截图、Browser 断言和 Golden snapshot。

### 3.2 中文 README

保留英文 `README.md`，新增内容等价的 `README.zh-CN.md`，两者顶部互链：

```text
English | 简体中文
```

中文 README 至少覆盖：

1. 独立社区项目定位与非 DeepSeek 官方声明；
2. Alpha 状态和精确兼容边界；
3. Homebrew、安装脚本、升级与卸载；
4. Web、Headless 与 SDK 快速开始；
5. 模型配置和 Remote Access；
6. Native/WASM/Legacy Node 插件路径；
7. 内置市场、社区投稿与三种兼容状态；
8. Phase 9 Benchmark 原始结果入口；
9. 安全限制、许可证与上游归属。

不引入翻译框架或自动机器翻译。发行门槛只检查两份 README 中不可漂移的事实：产品版本、Core revision、DSH 基线 commit、安装/升级/启动命令、Release/Benchmark/投稿链接和 Remote Access 默认安全姿态。

## 4. Phase 9-A：可复现性能与兼容性 Benchmark

### 4.1 目标与主对照

第一版可比较证据分成两层：

```text
Core：DeepSeek Harness `47f943859bef60e4160492346772ded9b24f765a`
vendored TypeScript Cordis `4.0.1`

vs

tessivum-core 0.1.6
commit 4674aeda870989fede1fc79fb07afbe764d3a1eb

Product：Tessivum 0.1.0-alpha.23
commit d455d99270673be208aecc3182cbf47b9b17989e
vs DeepSeek Harness 0.1.0-rc.5 clean commit 47f943859bef60e4160492346772ded9b24f765a
```

DeepSeek Harness `0.1.0-rc.5` 是产品兼容基线。首个试运行因缺少同构上游 driver 将它明确标为 `unmeasured`；当前实现已用上游原生 replay 插件补齐相同的可见 Prompt、工具 marker、十 Session、Browser 和清理契约。两边 replay 字节及内部路径不同，因此结论只覆盖该冻结离线产品契约，不外推为完整功能或生产 LLM 性能对等。

JCode、OMP、Claude Code 等 Terminal Coding Agent 不进入第一版主表。它们的功能面和进程模型不同，只能在未来作为明确标注的行业参考，不能替代同一兼容基线的一对一比较。

### 4.2 两组固定配置

#### Base

两个 runtime 都只启用冻结的基础 Host/Browser 功能面，不安装 Benchmark 插件，用于观察架构基础成本。

#### Compatibility

两个 runtime 均启用以下固定插件与 Browser Client bundles：

- Market；
- Better Sidebar；
- Dream Skin；
- 相同可见 Web Profile、Prompt/工具 marker 契约和 Browser 资源。

Tessivum 统计完整 Bun Legacy Host 子进程；DeepSeek Harness 统计完整 Node Host 子进程。Compatibility 只有在插件启动图、Session 工作量与 Browser 路径全部闭合后才进入横向结果。

每组配置都保存机器可读 manifest，固定插件版本、Profile、环境变量和预装状态。公开运行已闭合 Tessivum 与 DeepSeek Harness 两个产品列，各有 30 个有效样本。

### 4.3 产品级公开指标

| 指标 | 固定测量边界 | 公开意义 |
|---|---|---|
| Headless replay wall time | 新进程启动到同一固定 Replay 完整结束 | 排除模型和网络噪声 |
| Web Host ready | 新进程启动到 API 与 Boot Graph 可用 | Host 启动成本 |
| Web UI usable | HTTP ready 后，从 Browser worker 启动到真实 Chromium Composer 可输入 | Browser 初始化与必要的工作区选择成本 |
| Idle process-tree PSS | Ready 后相同稳定窗口内 Host 及全部后代进程 PSS | 防止漏算 Node/Bun |
| 1/10 resident Session PSS | 同一 Host 内加载同一固定 Session 工作量后的进程树增量 | 多会话扩展成本 |
| Create/dispose stress | 固定数量 Scope/Agent 创建、注册、销毁后的时间与残留 | 生命周期与清理价值 |

“Resident Session”必须完成同一固定数据加载或 Replay，不得只创建空 ID。内存同时报告总 PSS、相对 idle 增量和每 Session 增量，不能只报告父进程 RSS。

### 4.4 Core 配对附录

TypeScript Cordis 与 `tessivum-core` 必须消费同一份工作量描述；在既有 `tessivum.conformance/v1` fixture 与 TypeScript Oracle 旁增加 benchmark driver，不创建第二套语义模型。

第一版附录覆盖：

| 相同行为 | TypeScript Cordis | tessivum-core |
|---|---:|---:|
| Service lookup/s | 待实测 | 待实测 |
| Event emit/s | 待实测 | 待实测 |
| 16-entry Loader load | 待实测 | 待实测 |
| Loader update | 待实测 | 待实测 |
| 32-child root dispose | 待实测 | 待实测 |
| 1,000 Scope 创建/销毁 | 待实测 | 待实测 |
| 创建后的峰值 PSS | 待实测 | 待实测 |
| 全部销毁后的残留 PSS | 待实测 | 待实测 |

WASM 和 Legacy Node Bridge 的冷/热结果继续作为 Tessivum 路径成本附录，不伪装成 TypeScript Cordis 存在的同类路径。浅层 `size_of` 可以保留调试名称，但不得进入营销图表。

### 4.5 公平测量口径

公开结果必须满足：

1. 同一台固定 Linux 机器，记录 CPU、内存、内核、Rust、Node、Bun 和 pnpm 版本；
2. 两边使用正式 Release build 和已安装依赖，不把下载、`npx` 或首次 `pnpm install` 混入启动时间；
3. 每项至少 30 次独立 process-cold、filesystem-warm 测量；
4. A/B 样本交错执行，避免热漂移或后台负载只影响一方；
5. Headless 使用同一离线 Replay，不访问真实模型或网络；
6. Web usable 由真实 Chromium DOM 状态判定，不以 stdout 或 HTTP 200 代替；
7. Linux PSS 来自 Host 及其全部存活后代的 `/proc/*/smaps_rollup`；
8. 每个样本使用同构、隔离的数据根；失败、超时和异常退出仍写入原始结果；
9. 报告 raw samples、median、p95、min、max，不只报告最优值；
10. 原始 JSON、driver、配置 manifest、commit SHA 与复现命令随结果公开。

共享 GitHub runner 的数字只能验证脚本可运行，不能作为稳定产品结论。真实模型成功率与系统性能分表处理。

### 4.6 发布表达

结果未产生前不得在 README 或首页填写占位倍数。公开标题必须同时包含性能和兼容证据，例如：

```text
DeepSeek Harness rc.5-compatible Web surface

X× process startup
Y% lower idle process-tree PSS
Z× lower memory per resident Session

52/52 frozen Core RPC names
69/69 source-Web Chromium scenarios
exact lifecycle/service/event oracle traces
```

所有数字必须链接到对应 raw JSON、环境和复现命令。

### 4.7 Phase 9-A 完成定义

- 产品与 Core 两层 driver 均可在固定 Linux 机器从干净 checkout 复现；
- Base 与 Compatibility manifest 闭合；
- 十项产品指标和 Core 配对附录都有 30 个有效或显式失败样本；
- PSS 覆盖完整进程树；
- 真实 Chromium 证明 Web usable；
- 兼容性结果与性能结果并列；
- 旧 3/5 样本和浅层内存数字不再被误用为横向宣传；
- README 只引用已发布、可追溯的结果。

### 4.8 当前实施状态

已实现并纳入 `v0.1.0-alpha.23`：

- `tessivum-core/oracle/paired.ts` 与 `scripts/run_paired_benchmarks.py`；
- `tessivum/scripts/benchmark_product.py`、真实 Chromium worker 与上游 DeepSeek Harness 适配器；
- `tessivum/benchmarks/manifests/{base,compatibility}.json`；
- `tessivum/benchmarks/Dockerfile`、`run-linux.sh` 与容器入口；
- Linux 三样本协议试运行及每个 runtime/manifest 单元 30 样本的公开 raw snapshot，记录于 `benchmarks/fixtures/phase9-alpha23/`；
- 中英文公开报告、README 数字和 `scripts/check_release_facts.py` 漂移门槛。

Phase 9-A 已关闭：Core 两种 runtime 各 30 个有效样本；产品层 Tessivum/DeepSeek Harness × Base/Compatibility 四个单元均为 30/30 成功，真实 Chromium 十 Session 路径、相同可见插件图和完整进程树销毁残留均通过。公开结果同时披露 `loader_update` 回归、Tessivum Compatibility 启动回归及两产品的启动/内存差异。Phase 9-B 也已关闭；Phase 9 总验收完成。

## 5. Phase 9-B：社区插件发布与 Tessivum 验证

### 5.1 冻结发布模型

Tessivum 不建设插件上传服务器。三层职责固定为：

```text
npm / GitHub
  └─ 托管插件代码和发行包

awesome-dsh-plugin
  └─ 社区目录投稿、基础 CI 与人工收录

Tessivum
  └─ 内置市场安装 + 固定版本兼容验证
```

作者进入内置市场的第一步仍是向 `awesome-dsh-plugin` 提交一个独立 YAML 条目。社区目录合并后，插件自动进入 Tessivum Market，并默认显示为 `dsh-community / unverified`。

### 5.2 三种状态

| 状态 | 含义 | 谁可以授予 |
|---|---|---|
| `official` | Tessivum 自有、随产品构建和发行 | Tessivum 产品仓库 |
| `verified` | 精确社区版本通过 Tessivum 验证矩阵 | Tessivum 维护者与无密钥 CI |
| `unverified` | 已被社区目录收录，但没有当前 Tessivum 固定版本证据 | 自动默认 |

“收录”与“verified”都不是全面安全审计。市场卡片、安装确认和双语文档必须明确该边界。

### 5.3 Tessivum 验证申请

作者先完成社区目录收录，再向 Tessivum 提交验证申请。申请必须固定：

- 社区目录 identity 与源码仓库；
- npm 精确版本、Git commit 或不可变 Release tarball；
- 许可证；
- 目标 Profile；
- Native/WASM/Legacy Node/Browser 运行时；
- `dsh.bundle`、`dsh.client` 与所需 capability；
- 最低 Tessivum 版本和作者声明的功能范围。

Tessivum 不复制插件描述、stars、下载量或截图作为第二份目录真相。验证结果只保存按来源 identity 和精确版本索引的小型兼容 ledger，并覆盖社区目录条目的兼容状态。

社区目录中不存在的插件不能仅靠兼容 ledger 出现在市场。第一方 `catalog.json` 继续只保存 `official` 条目。

### 5.4 验证矩阵

固定版本至少验证：

1. 来源、包名、repository、许可证与不可变版本一致；
2. Profile preflight 和依赖闭包通过；
3. 安装后 Host 与 Browser 可启动；
4. 插件声明的核心功能存在，不以空成功替代；
5. 更新使用精确目标，实际安装版本一致；
6. 卸载清理 Bundle、Fiber、route、upgrade、subscription 与 client entry；
7. 安装、更新或启动失败时恢复原 Profile；
8. 无未预期 `pageerror`、console error、HTTP 4xx/5xx 和遗留子进程。

验证第三方代码的 CI 必须运行在临时、无仓库写权限、无发布密钥的环境；默认阻止安装脚本。确需构建脚本的插件只能在显式审查后进入单独的受限验证步骤。

### 5.5 更新与撤销

`verified` 绑定精确版本或 commit，不跟随 `latest`。社区插件发布新版本后：

- 市场仍可按现有策略展示和安装更新；
- 新版本在重新验证前显示为 `unverified`；
- 旧版本证据保留在 ledger 中，不被改写；
- 仓库消失、来源改变、恶意行为或严重缺陷可撤销验证并进入拒绝名单；
- 撤销验证不静默卸载用户插件，但后续安装/更新必须显示明确风险。

### 5.6 投稿文档与用户界面

中英文 README 和市场帮助入口必须解释两条路径：

```text
想进入目录
→ 向 awesome-dsh-plugin 提交 PR

想获得 Verified on Tessivum
→ 固定已收录版本，向 Tessivum 提交验证申请
```

市场卡片至少显示来源、版本、安装目标和兼容状态。安装确认不能用颜色或图标单独表达信任等级，必须有可读文本。

### 5.7 Phase 9-B 完成定义

使用一个真实第三方插件完成：

```text
社区目录 PR
→ 目录合并
→ Tessivum Market 显示 unverified
→ Tessivum 验证申请
→ 无密钥 CI 安装与运行
→ 固定版本升级为 verified
→ 市场安装、启动、更新、卸载
→ 失败场景回滚
```

同时满足：

- `official`、`verified`、`unverified` 不混淆；
- 社区目录仍是描述与发现的唯一真相；
- Tessivum 只维护最小兼容 ledger；
- 没有账号系统、上传服务、签名后台或第二套 npm registry；
- 新版本不会继承旧版本的 verified 状态；
- 投稿、验证、撤销和风险声明均有中英文文档。

### 5.8 当前实施状态

`dsh-better-sidebar@0.16.1` 已完成社区目录、精确 npm/repository/integrity/license 核对、Profile preflight、pnpm 安装、Legacy Host/真实 Chromium 启动、更新、卸载与失败回滚闭环。Market 对未安装或安装版本 `0.16.1` 显示 `verified`；安装 `0.17.1` 后按 `unverified` 显示；ledger 可将原精确版本改为带原因的 `revoked`。

申请入口、双语风险说明、可复现命令和原始结果分别记录于 `.github/ISSUE_TEMPLATE/plugin-verification.yml`、`docs/PLUGIN_VERIFICATION*.md`、`.github/workflows/plugin-verification.yml` 与 `docs/PLUGIN_VERIFICATION_REPORT.md`。Tessivum 没有新增账号、上传服务、签名后台或第二套 Registry。

## 6. 实施顺序与仓库边界

1. 应用“道器相成”中英文品牌文案，移除 DeepSeek Slogan 的产品身份用途；
2. 新增 `README.zh-CN.md`，建立中英文事实同步门槛；
3. 冻结 Benchmark 配置 manifest 和机器环境；
4. 在 `tessivum-core` 增加共享 fixture 驱动的 TypeScript/Rust 配对测量；
5. 在 `tessivum` 增加产品级进程、Chromium 和进程树 PSS 驱动；
6. 产出 raw JSON 后再撰写公开结果；
7. 保持社区目录入口不变，增加 Tessivum 固定版本验证 ledger 与申请流程；
8. 用一个真实第三方插件关闭 Phase 9-B 端到端验收。

仓库职责：

| 工作 | 所属仓库 |
|---|---|
| Core 配对 Benchmark 与 raw Core 结果 | `tessivum-core` |
| 产品进程/Browser/PSS Benchmark | `tessivum` |
| Slogan、README、市场状态与验证流程 | `tessivum` |
| 社区目录条目和通用元数据 | `awesome-dsh-plugin` 上游 PR |
| 插件代码与发行包 | 作者自己的 npm/GitHub |

Phase 9-A 与 Phase 9-B 在共享品牌和文档基线冻结后可独立实施；任一方不得阻塞另一方的内部验证，但统一发布声明必须只包含已完成的部分。

## 7. 明确不做

- 不建立 JCode 式多产品排行榜；
- 不把共享 CI runner 的偶然数字当作产品承诺；
- 不用 RSS 父进程值代替完整进程树 PSS；
- 不把真实模型网络延迟混入 Harness 性能；
- 不宣传浅层 Context/Fiber `size_of`；
- 不为了漂亮数字关闭 Compatibility 配置中的 Legacy Node 子进程；
- 不复制或 fork `awesome-dsh-plugin` 的完整社区目录；
- 不建设插件账号、上传、支付、推荐、签名或远程审核服务；
- 不把目录收录、Tessivum verified 或 official 混写成安全保证；
- 不通过全仓文本替换破坏 DeepSeek/Cordis 兼容协议与许可证事实；
- 不维护第二套 React UI 或翻译框架。

## 8. Phase 9 总完成定义

Phase 9 只有在以下全部满足后才能标记完成：

- “道器相成”成为 Tessivum 唯一中文产品 Slogan，DeepSeek Slogan 不再出现在产品身份面；
- 英文与简体中文 README 均能独立完成安装、运行、安全和兼容说明；
- Phase 9-A 的环境、脚本、原始样本、统计和兼容证据可公开复现；
- 首页性能结论能追溯到固定 commit、配置和 raw JSON；
- Phase 9-B 的社区收录、unverified、verified、更新降级和撤销路径均可观察；
- 一个真实第三方插件完成目录到安装、验证、更新、卸载和失败回滚闭环；
- 所有新增公开声明保持 Tessivum 独立社区项目边界。

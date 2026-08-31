# Tessivum Phase 4 品牌、分发与社区市场开发计划

> 状态：`v0.1.0-alpha.12`
> 发布日期：2026-08-27
> 实现基线：`v0.1.0-alpha.12`
> 上游兼容基线：DeepSeek Harness `0.1.0-rc.5` / `47f943859bef60e4160492346772ded9b24f765a`  
> Core 基线：`tessivum-core v0.1.5` / `a1a6d2e5584253391b9962c482f2140263b703bf`
> 社区市场基线：`dshmarket@1.29.2`
> 侧边栏兼容基线：`dsh-better-sidebar@0.16.1`

## 1. 文档目的

阶段一、阶段二和 Phase 3 已完成 Rust Cordis 内核、Rust-native Harness、冻结的 Browser 兼容面与产品能力。本计划定义下一轮产品化工作：

1. 清除用户可见和模型可见的上游产品身份，建立 Tessivum 原创品牌；
2. 把当前“下载归档后从解压目录运行”升级为可安装、可升级、可卸载的分发体验；
3. 保留 DSH/Cordis 协议兼容名，在不伪装完整兼容的前提下接通 dshmarket 的真实 Host、Browser 和包管理链路。

关联文档：

- [二阶段开发计划](DEVELOPMENT_PLAN.md)：总路线、历史里程碑与仓库边界；
- [目标运行时架构](ARCHITECTURE.md)：Legacy Node、Browser 和跨运行时所有权；
- [插件生态兼容方案](PLUGIN_COMPATIBILITY.md)：npm、Browser、WASM 与 Native 插件分类；
- [DeepSeek Harness 兼容基线](COMPATIBILITY_BASELINE.md)：冻结的 Web/wire 契约；
- [Phase 3 产品能力开发计划](PHASE3_PRODUCT_PLAN.md)：已完成的 Alpha.2–Alpha.4 实施记录。

如实现偏离本文，先更新本文和被影响的契约文档；不得用发布说明代替架构决策，也不得把计划中的目标写成当前已实现事实。

## 2. 当前事实与根问题

### 2.1 品牌迁移不完整

当前 Rust 产品、CLI、仓库和 HTML 标题已使用 `Tessivum`，但构建自冻结上游源码的 Browser 仍残留：

- PWA Manifest 的 `DeepSeek Harness` / `DSH`；
- DeepSeek 鱼形 Favicon；
- Sidebar 和 Hero 使用的 `BrandWordmark` / `FishLogo` 图形；
- 首次启动声明中的 DeepSeek Harness 产品身份；
- 部分模型提示词、Browser fixture 和快照中的 DeepSeek Harness 身份。

这会让用户误认为 Tessivum 是 DeepSeek 官方发行版，与“独立社区 Rust-native 项目”的公开定位冲突。

### 2.2 分发仍暴露归档内部结构

Alpha.9 已提供四个平台归档与相邻 SHA-256，但用户仍需：

1. 手工选择 target；
2. 手工下载、校验和解压；
3. 从版本化解压目录执行 `./bin/tessivum`；
4. 自行统一 `--data-dir`；
5. 自行安装 Legacy 插件所需 Bun、npm/pnpm。

归档适合作为分发底座，不是最终安装体验。

### 2.3 dshmarket 不是“下载成功即兼容”

`dshmarket@1.29.2` 的实际运行链路要求：

- Host Context 同时提供 `webServer` 与 `loader`；
- Host 路由能注册 `/dsh-market/*` 并执行 Node handler；
- 普通 DSH 模式能解析 `$DSH_HOME/profiles/<profile>`，并执行 `dsh plugin --profile <profile> ...`；
- 可选 Desktop 模式能取得 `desktopProfiles.current.dir` 和 `desktopPnpm.runPlugin(...)`；
- Host 能解析 `@deepseek-ai/dsh-settings` 和 `@deepseek-ai/schemastery`；
- Browser 能发现并装载包声明的 `dsh.client` half。

Alpha.9 已有 Node Loader 和 Browser client-half 扫描，但没有 Node `webServer` 路由桥，也没有 dshmarket 所需的活动 Profile/包管理服务。现有 `tessivum plugin add` 使用 npm，目录和 CLI 形状也不同。因此只有完成本计划的 Alpha.11 门槛后，才可以声明 dshmarket 兼容。

## 3. 冻结决策与明确不做

### 3.1 冻结决策

1. 产品、CLI、仓库和用户可见品牌统一使用 **Tessivum**。
2. 名称来源保持为 *tessera*（可组合单元）+ *aevum*（时间/生命周期）。
3. 原创 Logo 采用“**Tessera Loop**”方向，不继续使用或描摹鱼、鲸及 DeepSeek 字标。
4. Browser 继续直接构建冻结的上游 Client 源码；品牌通过有界、可审计 Overlay 应用，不维护第二套 UI。
5. `@deepseek-ai/dsh-*`、`dsh.client`、`dsh.bundle.patch`、`DSH_*` 等协议名作为兼容契约保留。
6. macOS/Linux 的首选安装渠道是独立 Homebrew Tap；GitHub 归档仍是唯一二进制来源。
7. 无 Homebrew 环境提供无 sudo 安装脚本；不创建 Tessivum 自有系统包管理器。
8. 用户数据默认统一到 `${TESSIVUM_HOME:-$HOME/.tessivum}`；`--data-dir` 继续具有最高优先级。
9. Legacy npm Profile 统一由 pnpm 管理；不长期并存 npm/pnpm 两套 lockfile 和 mutation 语义。
10. dshmarket 使用其现有的 Host-owned Profile/包管理扩展，不安装或暴露全局 `dsh` 伪命令。
11. Alpha.11 采用 restart-required 激活；不允许 dshmarket 在 Node 子图内创建绕过 Rust Loader 权威状态的热挂载。

### 3.2 明确不做

- 不把所有 `deepseek`/`dsh` 字符串做全仓文本替换；
- 不重命名第三方包、wire、环境变量或插件 Manifest；
- 不复制一套 Tessivum React 应用来替代上游 Browser Cordis；
- 不引入品牌字体、动画框架、渐变系统或新的设计依赖；
- 不让安装脚本使用 sudo、静默修改 shell rc 或删除用户数据；
- 不在 Rust 中重写 npm registry/semver/lockfile/package extraction；
- 不把 Legacy Node 宣称为沙箱；
- 不允许第三方 Node 插件注册任意 Host 路由、升级连接、fallback 或 index transform；
- 不在 Alpha.11 实现插件安装后的 Host/Browser 无重启热切换；
- 不承诺未经真实场景验证的任意 dshmarket `latest` 或整个社区目录全部兼容。

## 4. Tessivum 产品身份契约

### 4.1 Logo 与字标

“Tessera Loop”必须满足：

- 由四块相互咬合的几何拼片形成开放环；
- 负空间可识别出抽象 `T` 与生命周期沙漏，但不依赖说明才能辨识主体；
- 仅使用原创路径，不从 DeepSeek/Figma 原图修改或描摹；
- 原始画布为正方形，16/24/32/128/512 px 下均不粘连；
- React/SVG 使用 `currentColor`，明暗主题不维护两套图形；
- 不使用外部字体、滤镜、位图或运行时生成路径；
- 图标本身 `aria-hidden`，可交互入口由相邻文字或 `aria-label` 提供名称。

字标使用“Logo + `Tessivum` 文本”，文本继承现有 UI 字体和 Token。Sidebar 不再嵌入一整段手工字母路径；这减少资产重量，也避免另一个不可维护的品牌字体实现。

### 4.2 资产源与尺寸

唯一源资产应能生成：

| 资产 | 用途 | 约束 |
|---|---|---|
| `TessivumMark` | Sidebar rail、Hero、按钮 | 纯 SVG、`currentColor` |
| `TessivumWordmark` | 展开 Sidebar | Mark + 语义文本 |
| `favicon.svg` | 浏览器 | 独立 SVG，不依赖 CSS bundle |
| PWA icon | 安装图标 | 正方形安全区，支持 maskable 版本时另行导出 |
| Release/GitHub 图 | 发布页 | 从同一几何源导出，不创建第二 Logo |

### 4.3 必须替换的产品身份

Alpha.10 必须覆盖：

- HTML `<title>` 与动态会话标题后缀；
- PWA `name`、`short_name`、icons；
- Favicon；
- Sidebar 展开字标和折叠标记；
- Empty Hero 标记；
- Loading、Noscript、空状态和错误页中的产品名；
- 中英文首次启动声明；
- Tessivum Web surface 与 Host identity 的模型可见提示词；
- 受影响的 Browser 单元测试、Chromium 断言和 Golden snapshot。

DeepSeek 官方 Provider、DeepSeek API Key、兼容包名、上游来源和许可证仍按事实显示，不属于产品身份残留。

### 4.4 组件兼容

新增一等导出：

```text
TessivumMark
TessivumWordmark
```

Tessivum 自有 Sidebar/Hero 只使用新导出。若已发布 Browser 插件依赖旧的 `FishLogo`/`BrandWordmark` 导出，为保持 Browser 包 API 可保留兼容导出，但旧名必须渲染 Tessivum 新资产，不能携带原鱼形或 DeepSeek 字标。兼容导出属于上游 Browser API 适配，不得出现在 Tessivum 自有源码的新调用点。

### 4.5 归属与商标边界

- README、About/credits 和发行包继续包含 DeepSeek Harness、Cordis 及依赖许可证；
- 对外描述使用“independent”与“compatible with”，不用“Rust DeepSeek Harness”；
- 上游名称只出现在兼容性、来源、模型 Provider 或许可证语境；
- Tessivum Logo、下载页和启动页不得并排使用 DeepSeek Logo 造成联合品牌印象。

## 5. Browser 源码与品牌 Overlay

### 5.1 唯一实现

Browser 的入口、Cordis 生命周期、38-package graph、slots、stores、wire 和 CSS 组合仍来自冻结上游源码。品牌 Overlay 只能修改：

- 静态品牌资产；
- 产品名称和独立项目声明；
- 对应可访问性标签；
- 直接受影响的测试期望。

Overlay 禁止修改 RPC、SessionEvent、slot id、package id、`window.__DSH_BOOT__`、module handoff、Loader 依赖和 Host 权威状态。

### 5.2 构建纪律

1. 品牌源文件和 Patch 均提交在 `web/`；
2. `prepare-deepseek-source.mjs` 对固定提交做 exact-once 应用；
3. Patch 必须先 `git apply --check`，失败即停止构建；
4. `web/client-packages` 和 `web/dist` 是生成物，不接受手工品牌修补；
5. Source audit 增加“禁止用户可见 DeepSeek Harness 身份和原鱼形路径”的检查，但允许兼容包名、Provider 名和 fixture 中明确标注的上游事实；
6. 上游基线升级时，先重放 Overlay、审查冲突，再更新快照，不用宽松文本替换掩盖漂移。

### 5.3 模型身份

Native `SystemPrompt` 是模型可见身份的权威。Web 组合必须形成：

```text
You are an AI agent powered by Tessivum.

You are interacting with the user through the Tessivum Web GUI at ...
```

上游 `harness:identity` 和 `harness:source` 不能与 Native 身份同时出现。实现必须在组合层关闭或替换上游身份，不在每次请求后做字符串删除。`@deepseek-ai/dsh-*` 作为消息 source/plugin id 继续保留。

## 6. 数据目录与 Profile 统一

### 6.1 路径优先级

目标优先级：

```text
--data-dir <DIR>
> TESSIVUM_HOME
> $HOME/.tessivum
```

同一解析函数必须供 Headless、Web、Plugin 和 Legacy Host 使用。不得让 `tessivum web` 与 `tessivum plugin add` 因当前工作目录不同而看到两个 Profile。

目录结构：

```text
~/.tessivum/
├── settings.yaml
├── credentials.yaml
├── .agent-presets/
├── attachments/
├── plugins/
│   ├── package.json
│   ├── pnpm-lock.yaml
│   └── node_modules/
└── <现有 session/workspace/feedback durable files>
```

本节冻结统一根和 `plugins/` 所有权；现有 Host durable 文件保持当前命名与格式，不为目录美观做无价值迁移。

### 6.2 Alpha 数据迁移

首次使用新默认根时：

- 如果用户显式传入 `--data-dir`，完全遵从，不探测其他目录；
- 如果默认根不存在但当前目录有旧 `.tessivum`，不静默复制或合并；
- 启动返回可操作诊断，给出继续使用 `--data-dir .tessivum` 和一次性移动到 `$HOME/.tessivum` 的两种命令；
- 升级说明列出 sessions/settings/plugins 的迁移影响；
- 安装器升级和卸载都不修改数据根。

该 fail-loud 迁移只服务 Alpha 目录切换；完成一个发布周期后可删除探测，但不能长期维持“有时全局、有时项目内”的隐式规则。

## 7. 安装与升级设计

### 7.1 Homebrew Tap

建立独立仓库 `wavetao2010/homebrew-tap`，Formula 名为 `tessivum`。用户路径：

```bash
brew install wavetao2010/tap/tessivum
tessivum --version
tessivum web
```

Formula 必须：

- 按 macOS/Linux 与 x86-64/ARM64 选择 Alpha.10 GitHub Release 归档；
- 固定每个归档 SHA-256；
- 把完整归档安装到 `libexec`，再把打包 launcher 链接到 `bin/tessivum`；
- 保持 launcher 相对定位 compat-host、Cordis vendor 和 Agent Presets 的能力；
- 声明 `bun` 与 `pnpm` 运行依赖；pnpm 的 Node 依赖由包管理渠道解析；
- `brew upgrade` 原子切换程序版本，不触碰 `$HOME/.tessivum`；
- `brew uninstall` 删除程序，不删除用户数据；
- 在 Formula smoke 中运行 `tessivum --version` 和一次无 Legacy 插件的 Web 启动。

Alpha 阶段使用 Tap，不申请进入 `homebrew-core`。CLI 二进制尚未签名或 notarize 的事实继续在下载说明中披露。

### 7.2 无 sudo 安装脚本

官方脚本是 Homebrew 的备用入口，不是第二种构建产物。目标：

```bash
curl -fsSL https://raw.githubusercontent.com/wavetao2010/tessivum/main/install.sh | sh
```

脚本必须：

1. 接受可选固定版本，不在测试中依赖浮动 latest；
2. 只接受四个已发布 target；
3. 同时下载 `.tar.gz` 和 `.sha256`；
4. 使用 `sha256sum` 或 `shasum -a 256` 验证后才解压；
5. 拒绝非 HTTPS、校验缺失、版本与归档目录不一致；
6. 安装到 `~/.local/lib/tessivum/<version>`；
7. 通过同目录临时链接原子更新 `~/.local/bin/tessivum`；
8. 不使用 sudo，不修改 shell rc；PATH 缺失时打印一条准确命令；
9. 不自动安装 Bun/pnpm/Node；检测缺失并说明只有 Legacy 插件管理需要它们；
10. 失败时保留旧链接，不留下半安装版本。

卸载说明必须区分“删除程序”和显式“删除 `$HOME/.tessivum` 数据”。本阶段不增加交互式安装向导。

### 7.3 归档继续作为单一底座

GitHub Release 继续发布四个平台归档及 checksum。Homebrew 和安装脚本只消费同一归档，不重新打包另一份内容。Release workflow 必须在上传前验证：

- launcher 相对路径；
- `--version`；
- Agent Preset inventory；
- Browser embedded assets；
- Legacy vendor identity；
- checksum；
- 解压后 Web smoke。

## 8. 插件 Profile 与包管理器

### 8.1 pnpm 成为唯一 mutation backend

`tessivum plugin add/remove` 和 dshmarket 必须操作同一个 `${TESSIVUM_HOME}/plugins` Profile。目标行为：

```text
add     → pnpm add --save-exact <target>
remove  → pnpm remove <package>
repair  → pnpm install
```

实现不得把用户输入拼成 shell 字符串；程序与参数分离传给子进程。包名、file/github spec、工作目录 containment、超时、取消和输出上限必须在 spawn 前验证。

Profile mutation 使用单个跨 CLI/market 的独占锁。任何失败都必须重新读取 `package.json`、lockfile 和目标包入口，确认是否发生部分写入；不能仅凭进程退出码报告回滚成功。

### 8.2 Lifecycle scripts

默认继续拒绝未授权的 lifecycle/build scripts。dshmarket 的“允许构建脚本”动作只能修改 Profile 中 pnpm 支持的精确包 allowlist，并在用户明确操作后重试一次。禁止：

- 全局允许 scripts；
- 把 registry 返回的任意名字直接写入 allowlist；
- 在失败后自动放行；
- 把 Legacy 插件称为沙箱插件。

### 8.3 依赖发现

- Native/WASM/Browser-only 路径不因安装 Formula 而必须启动 Node；
- 只有 Legacy 插件激活时启动 Bun compat-host；
- `plugin add/remove` 或 dshmarket mutation 需要 pnpm；
- 缺少 Bun/pnpm 时在进入 mutation 或 Legacy boot 前返回具体诊断，不等到 Node Fiber 无限 Pending；
- 发行说明区分“核心 Web 可运行”和“Legacy 社区插件环境完整”。

## 9. dshmarket 兼容架构

### 9.1 冻结兼容目标

Alpha.11 首个声明只覆盖 `dshmarket@1.29.2`，并记录：

```text
Cordis Host API:              @deepseek-ai/cordis 4.0.1
Browser compatibility plane: DeepSeek Harness 0.1.0-rc.5
Market:                       dshmarket 1.29.2
Profile manager:              pnpm
Activation:                   restart-required
```

升级 dshmarket 前必须单独更新 package/peer/route/CLI 行为快照。文档不得把该矩阵外推为所有版本兼容。

### 9.2 Legacy `webServer` 有界 facade

`dshmarket` 使用 `webServer.register({ kind, path, handler })` 注册 HTTP 路由。Tessivum 不启动第二个 Node HTTP listener；Rust Axum 仍是唯一监听者。

新增可协商扩展 `web.route/v1`：

```text
Node plugin
  → webServer.register(exact|prefix, path, callbackId)
  → cordis.node/v1 extension frame
  → Rust route registry
  → existing Host/Origin authority middleware
  → bounded request frame to Node callback
  → bounded response frame to Axum response
```

v1 只提供 dshmarket 真实使用的 `register`：

- `exact` 与 `prefix`；
- method、path/query、受控 headers、最多 2 MiB request body；
- status、受控 response headers、最多 8 MiB response body；
- request cancellation；
- generation-owned disposer；
- duplicate route 冲突；
- Node crash/timeout 后撤销路由并返回结构化 502/504。

v1 不提供：

- `registerUpgrade`；
- `registerFallback`；
- `tapIndex`；
- WebSocket/SSE/无限 streaming；
- Node 自行 listen；
- 绕过 Host/Origin 或 body limit。

Tessivum 产品 policy 只允许固定兼容样本注册其自有前缀：`/dsh-market`、`/sidebar` 与 `/dream-skin`。Core transport 负责 framed callback、取消和 generation cleanup；产品仓库负责 Axum 路由、前缀 allowlist、HTTP DTO 与 authority。

### 9.3 Host-owned Profile 与包管理服务

Compat Host 在明确配置后发布：

```text
desktopProfiles.current = {
  name: "web",
  dir: "<absolute TESSIVUM_HOME>/plugins"
}

desktopPnpm.runPlugin(args, invokingDir, signal)
```

约束：

- Profile 路径由 Rust launcher 通过只读启动配置传入，插件不能选择任意目录；
- `invokingDir` 不决定 mutation cwd；所有 mutation 固定在 Profile 根；
- 仅接受 pnpm add/remove/install 及实现 dshmarket 恢复流程所需的已冻结 flags；
- 子进程 stdout/stderr 以有界 stream 暴露，取消终止整个进程树；
- 同一 Profile 同时只运行一个 mutation；
- 不发布全局 `dsh` shim，不修改用户 PATH；
- `runExternalMarketPluginInstall` 第一版不提供，避免制造只支持 npm exact version 的第二边界。

### 9.4 Node 模块解析

发布物为 Legacy Host 提供 dshmarket 启动所需、版本固定并带许可证的兼容模块：

- `@deepseek-ai/cordis`；
- `@deepseek-ai/cosmokit`；
- `@deepseek-ai/cordis-plugin-loader`；
- `@deepseek-ai/dsh-settings@0.1.0-rc.7`；
- `@deepseek-ai/schemastery@3.18.1`。

Browser 仍固定 rc.5，不能因为 Host compatibility module 引入 rc.7 Browser bundle。Legacy Host 组合一个由 Rust `Settings`/`settings.yaml` 持久化支撑的 `settings` provider；插件命名空间仍由 Node 侧公开 schema 解析和监听，Rust 侧只提供受限的整文档加载与未注册命名空间写入。该边界已由 `dsh-better-sidebar@0.16.1` 的设置读写和重启恢复验证，不伪造未挂载服务。

每个额外包必须进入 release license inventory、hash 和 smoke；禁止构建时从浮动 `latest` 取包。

### 9.5 Loader 与激活权威

Rust Loader/Plugin Profile 是持久权威。为避免 Node 内部热挂载形成第二状态源：

- 不随 Alpha.11 打包 dshmarket 热挂载依赖的 `cordis-plugin-include`；
- dshmarket 因 Include 不可用走其既有 restart fallback；
- 安装/更新/卸载完成后返回 `restartRequired: true`；
- 当前进程的 Browser graph 和 Host entry tree不做局部伪更新；
- 重启后 Rust 重新读取 package.json/lockfile/bundle patch，生成唯一 Entry Tree 与 client graph；
- dshmarket 的 `.dsh-market/state.json` 只保存其自有 UI/组/渠道状态，不覆盖 Tessivum Loader truth。

### 9.6 HTTP 与供应链安全

- `/dsh-market/*` 先经过 Tessivum 现有 authority middleware，再进入 Node；
- POST 的 Origin/Host 由 Rust 验证，原始规范化值传给 Node 供 dshmarket 再验证；
- hop-by-hop、Host 重写、绝对 URL 和多值异常 header 不跨桥；
- 2 MiB backup restore 是 v1 request 上限的最大合法用户输入；
- Node handler 超时、崩溃或写超限不能影响其他 Rust API 路由；
- pnpm target 必须来自用户明确输入或 dshmarket 已验证目录项，仍经过 Tessivum runner allowlist；
- 默认忽略 scripts；显式 allowBuilds 是独立高风险操作；
- Legacy Node 与包管理子进程使用真实用户权限，文档不得称其沙箱。

## 10. 里程碑 A：Alpha.10 独立品牌与可安装分发

### 10.1 交付物

1. 原创 Tessera Loop SVG 与 React 组件；
2. Sidebar、Hero、Favicon、PWA 和启动文案全量切换；
3. 模型可见 Tessivum identity；
4. 品牌 Overlay/source audit；
5. `$HOME/.tessivum` 默认根与旧 cwd 数据诊断；
6. Homebrew Tap Formula；
7. 无 sudo checksum 安装脚本；
8. 更新 README、下载说明、许可证归属和升级说明；
9. 四平台 Alpha.10 Release。

### 10.2 实施顺序

1. 冻结 Logo 几何、组件 API、可见字符串 allow/deny 表；
2. 在上游源码 Patch 中加入新组件，更新一等消费者和兼容导出；
3. 替换静态 PWA/Favicon/HTML 与中英文 Onboarding；
4. 切换 Native/Web prompt identity，更新快照；
5. 增加 source audit，确认生成 bundle 不含旧图形和产品身份；
6. 统一数据根解析与迁移诊断；
7. 完成安装脚本并对四个已发布归档做离线 fixture 验证；
8. 创建 Tap Formula，验证 install/upgrade/uninstall；
9. 运行真实 Browser 视觉/语义验收与四平台 release smoke；
10. 发布 Alpha.10 后再开始 Alpha.11 的 market wire。

### 10.3 Alpha.10 完成定义

- 用户可见和模型可见产品身份均为 Tessivum；
- DeepSeek Logo 不存在于发布 Web 资产；
- DeepSeek 只在 Provider、兼容名、来源和许可证语境出现；
- Browser 仍使用同一上游 entry、package graph、slot 与 wire；
- `brew install .../tessivum && tessivum web` 可运行；
- 安装脚本校验 checksum，失败不破坏旧版本；
- Web/Plugin 默认共享 `$HOME/.tessivum`；
- 四平台归档、Tap 和直接安装消费同一 release bits；
- 上游 69 个 Chromium 场景除有意品牌期望外保持通过，且 `pageerror=[]`、受监控 console 为空。

## 11. 里程碑 B：Alpha.11 dshmarket 真实兼容

### 11.1 交付物

1. pnpm Profile manager 与独占 mutation gate；
2. `web.route/v1` 协商、Node facade、Rust route registry 与 cleanup；
3. `desktopProfiles` 与 `desktopPnpm` Compat Host 服务；
4. 固定 Host compatibility modules 与 license inventory；
5. dshmarket Browser client graph 集成；
6. restart-required install/update/remove flow；
7. `dshmarket@1.29.2` 兼容报告、fixture 和真实 Chromium E2E；
8. README 的安装命令、依赖、重启语义和已知限制。

### 11.2 实施顺序

1. 冻结 `web.route/v1` frame、HTTP DTO、limits、错误码和能力协商；
2. 在 `tessivum-core` 实现 extension transport、callback、取消与 generation cleanup；
3. 在 `tessivum` 实现 Axum 路由 owner、前缀 policy 和 Host authority 集成；
4. 在 Compat Host 发布 `webServer` facade，只暴露协商成功的 v1 面；
5. 将 Plugin manager 从 npm 切到 pnpm，并加入 Profile lock、取消和部分写入诊断；
6. 发布 `desktopProfiles`/`desktopPnpm`，固定实际 Profile；
7. 打包 dsh-settings/schemastery Host compatibility modules；
8. CLI 安装固定 dshmarket 版本，重启后验证 Host routes 和 client bundle；
9. 完成 registry、installed、check、install、cancel、uninstall、rollback、backup/restore 路径；
10. 验证 self-update 只更新 Profile 并要求重启，不生成第二 Loader truth；
11. 运行 Core/产品/Browser/发行包矩阵；
12. 发布 Alpha.11 和明确兼容矩阵。

### 11.3 Alpha.11 完成定义

只有以下全部满足，才可以写“支持 dshmarket”：

1. `tessivum plugin add dshmarket@1.29.2` 在统一 Profile 完成；
2. 重启后市场 Host fiber Active、Browser 页面可见；
3. `/dsh-market/status`、registry、installed、check 通过真实 Rust HTTP → Node handler；
4. 市场安装一个 npm 社区插件，取消一次进行中安装，卸载并回滚一次失败；
5. 每次 mutation 后重启，Rust inventory、Node Loader entries、Browser graph 与 package manifest 一致；
6. route disposer、Node crash、timeout、Host shutdown 后无残留路由或子进程；
7. 跨源 POST、超限 body、未知路由前缀和未授权 build script 被拒绝；
8. dshmarket peer/duplicate/entry diagnostics 不被 Tessivum 吞掉或改成成功；
9. Browser `pageerror=[]`、受监控 `console.warn/error=[]`；
10. 文档明确版本矩阵、restart-required、Legacy 信任边界和 Bun/pnpm 依赖。

## 12. 验证矩阵

### 12.1 品牌与 Browser

| 场景 | 证明 |
|---|---|
| 首次启动 | Tessivum 声明、Logo、标题、PWA，无 DeepSeek 产品身份 |
| 已有会话 | `会话标题 — Tessivum`，刷新/重连不恢复旧标题 |
| Sidebar | 展开 Wordmark、折叠 Mark、键盘/ARIA 行为不变 |
| Hero | 新 Mark、现有文案与布局无溢出 |
| 明暗主题 | `currentColor` 对比度、Favicon 可辨识 |
| Prompt | 只有一份 Tessivum identity，无上游 Harness identity 重复 |
| 兼容名 | `@deepseek-ai/*` graph、Provider 和 wire 不被品牌 audit 误删 |

### 12.2 安装与升级

| 场景 | 证明 |
|---|---|
| Homebrew 四 target | 正确 archive/SHA、launcher、`--version`、Web smoke |
| Upgrade | 新程序生效，`$HOME/.tessivum` 数据不变 |
| Uninstall | 程序删除，用户数据保留 |
| 安装脚本 | 无 sudo、checksum、原子 symlink、失败保留旧版 |
| PATH 缺失 | 只打印修复命令，不修改 shell rc |
| 旧 cwd 数据 | fail-loud 迁移指引，不静默分叉 |
| 缺少 Legacy 依赖 | Core Web 可运行；plugin mutation/Legacy boot 给出具体诊断 |

### 12.3 dshmarket

| 场景 | 证明 |
|---|---|
| Boot | webServer/loader 注入完成，Host fiber Active |
| Read routes | status、registry、installed、updates 返回有界响应 |
| Mutations | add/remove/install、Profile lock、pnpm exact pin |
| Cancel | AbortSignal 与进程树终止，lock 释放 |
| Partial failure | package.json/lock/node_modules 复核，诊断准确 |
| Restart | 新 Host/Browser entry 激活，旧 route/generation 失效 |
| Backup | 2 MiB 上限、secret exclusion、restore 校验 |
| Security | same-origin、prefix allowlist、headers/body/response limits |
| Crash | Node 退出后 route 502、全部 registration cleanup |
| Compatibility | dshmarket 1.29.2 peer/duplicate/loadability preflight |

## 13. 强制验证与发布门槛

代码完成后至少执行：

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cd web && bun install --frozen-lockfile && bun run build
cd web && bun run test
```

此外必须运行：

- Tessivum Core 对 `web.route/v1` 的 framing/cancel/generation conformance；
- 真实 Rust Web Host + Bun Compat Host + Chromium 的 dshmarket 场景；
- 打包归档内的 Legacy Host 与 market smoke，不能使用源码路径 fallback；
- Homebrew Formula 的 install/test/uninstall；
- 安装脚本对四个 fixture archive 的 checksum、架构选择和原子升级；
- 发布后从 GitHub Release 下载成品再执行一次安装与 market smoke。

测试不得通过静态 HTML、mock 成功响应、跳过 Node handler 或手工复制插件文件伪造完成。

## 14. 文件与仓库影响地图

| 工作 | `tessivum` | `tessivum-core` | 其他仓库 |
|---|---|---|---|
| 品牌 Overlay | `web/patches`、public、tests、prompt composition | 无 | 无 |
| 数据根 | CLI/Host/Plugin path resolver、README | 无 | 无 |
| 安装脚本 | release scripts/workflow、README | 无 | Tap 消费 release |
| Homebrew | release contract | 无 | `homebrew-tap/Formula/tessivum.rb` |
| pnpm Profile | `plugin_manager.rs`、CLI、tests | 无 | 无 |
| HTTP route owner | Axum registry、policy、API tests | extension consumer | 无 |
| `web.route/v1` | product DTO/policy | Rust transport + Compat Host facade | 无 |
| dshmarket modules | release packaging、aliases、licenses | Compat Host resolver | npm 固定 tarball输入 |
| Market E2E | Browser fixtures、release smoke | Node bridge conformance | 无 |

通用 framed callback、generation cleanup 和 Compat Host `webServer` facade 进入 `tessivum-core`；Axum 路由、`/dsh-market` policy、Profile 路径、包管理与品牌归 `tessivum`。

## 15. 风险登记

| 风险 | 控制 |
|---|---|
| 品牌 Patch 漂移成 UI fork | Overlay allowlist、上游提交 pin、source audit、69-file gate |
| 误删兼容名导致插件失效 | 用户可见身份与协议名分表审计，不做全仓替换 |
| Homebrew 与归档内容漂移 | Formula 只消费 release archive + SHA，不重新打包 |
| 默认数据根让旧数据“消失” | 旧 cwd 检测后 fail-loud，显式迁移，不静默合并 |
| npm/pnpm 双树 | Alpha.11 清洁切换为 pnpm，删除 npm mutation path |
| Market 直接调用 `dsh` | Host-owned desktopPnpm adapter，不提供全局 shim |
| Node 路由绕过 Rust 安全 | 单一 Axum listener、authority first、prefix/size/header policy |
| Node handler 卡死 | bounded body/response、deadline、cancel、generation cleanup |
| Market 热挂载形成第二权威 | 不提供 Include hot-mount，统一 restart-required |
| build scripts 供应链风险 | 默认拒绝，精确 allowlist，用户显式动作 |
| dshmarket 更新破坏兼容 | 固定 1.29.2，升级单独变更矩阵与 E2E |
| rc.7 Host module污染 rc.5 Browser | Host/Browser artifact roots和 graph scan 分离，hash/inventory gate |
| 安装器破坏旧版本 | checksum-before-extract、versioned dir、atomic symlink |

## 16. 总完成定义

Phase 4 完成必须同时满足：

1. Tessivum 具有原创、可访问、明暗主题一致的产品身份，发布资产不再使用 DeepSeek Logo；
2. 品牌 Overlay 不改变冻结的 Browser package graph、wire、slot 和 Host 权威状态；
3. Homebrew 与安装脚本都从四平台 GitHub Release 成品完成安装，用户可直接运行 `tessivum web`；
4. Web、Plugin 和 Legacy Host 共享唯一 `$HOME/.tessivum` 数据根；
5. pnpm 是唯一 Legacy Profile mutation backend，部分失败和 build scripts 均 fail closed；
6. `web.route/v1` 只暴露有界、可取消、generation-owned 的 HTTP register 面；
7. `dshmarket@1.29.2` 的 Host routes、Browser UI、安装、取消、卸载、回滚和重启恢复通过真实 E2E；
8. Rust Loader、Node entries、Browser graph 和 Profile manifest 在重启后收敛到同一状态；
9. Native/WASM/Core Web 在没有 Legacy 依赖时继续运行；
10. README、兼容文档、许可证、下载说明和 release notes 不夸大品牌归属、沙箱或插件兼容范围。

## 17. 关闭记录

2026-08-26 完成 Alpha.10 与 Alpha.11 的实现、验收和公共发布：

- `cargo clippy --all-targets -- -D warnings` 与 `cargo test --all-targets` 通过；Tessivum 共 43 个 suite、409 个测试通过；
- Tessivum Core workspace 共 33 个 suite、116 个测试通过，Node Compat Host 9 个测试通过；`web.route/v1` framing、cancel 与 generation cleanup conformance 通过；
- pinned Browser source suite 共 239 个文件、3302 个测试通过，69 个迁移 Browser 场景全部通过；
- 四 target installer fixture、checksum 失败、原子升级、幂等卸载以及 Homebrew 实际 install/test/uninstall 通过；
- 本地 Alpha.11 发布归档完成打包，并从归档 launcher 安装 `dshmarket@1.29.2`；Chromium 中 Tessivum 标题、插件市场 Browser entry 和 `/dsh-market/status` Rust→Bun route 均通过，未观察到 Browser error；
- 最终集成审查发现 release/CI 使用旧 Core revision；三个 checkout 已统一为 `e894744e88cbed359179745e31eed00c1f45201b`，并由 `check_compat_baseline.py` 固定校验，复审确认关闭。
- Alpha.11 发布后的兼容跟进把 Core 基线推进到 `a1a6d2e5584253391b9962c482f2140263b703bf`：增加 generation-owned WebSocket upgrade proxy、Bun-native `ws` 后端与 native-backed Legacy settings，并用 `dsh-better-sidebar@0.16.1` 的真实 Browser UI、HTTP、WebSocket、设置写入和重启恢复完成验证。

GitHub [`v0.1.0-alpha.11`](https://github.com/wavetao2010/tessivum/releases/tag/v0.1.0-alpha.11) prerelease 已由 `release.yml` run `32945278216` 发布四平台归档、SHA-256 与 Formula。发布后重新下载并验证 Apple Silicon 归档，从该归档安装 `dshmarket@1.29.2`；真实 Chromium 插件市场与 `/dsh-market/status` 通过且无 Browser error。公共 `wavetao2010/homebrew-tap` 的 install/test/uninstall 也已通过。

GitHub [`v0.1.0-alpha.12`](https://github.com/wavetao2010/tessivum/releases/tag/v0.1.0-alpha.12) prerelease 已由 `release.yml` run `33029507732` 发布四平台归档、SHA-256 与 Formula。发布前从本地 Apple Silicon 归档安装 `dsh-better-sidebar@0.16.1`，真实 Browser UI 展示 Files 面板和归档文件，6 个 Sidebar API 请求无失败且 Agent Terminal WebSocket 可建立连接；发布后重新下载归档并通过 SHA-256 与版本检查。公共 `wavetao2010/homebrew-tap` 已更新至 Alpha.12，并由 Homebrew 成功解析与下载。

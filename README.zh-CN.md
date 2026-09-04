# Tessivum

[![CI](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml/badge.svg)](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml)

[English](README.md) | 简体中文

> 道器相成  
> Principle and implementation, in concert.

Tessivum 是独立的 Rust 原生智能体框架。它面向两个明确的兼容平面：为现有 npm/Cordis 插件保留的 Legacy Node，以及面向上游 React UI 的 Browser Cordis；同时将 Host、Agent、会话、工具、API 和 SDK 运行时迁移至 Rust。

> Tessivum 是社区项目，与 DeepSeek 没有关联，不是官方 DeepSeek 项目，也无意取代官方 DeepSeek Harness 仓库或其发布流程。

## Alpha 状态

`v0.1.0-alpha.23` 是预发布版本，不承诺生产稳定的 API 或数据格式。远程访问 **disabled by default**，且仅限 **loopback-only**；本地用户可以在 `/remote` 一次性启用 Cloudflare 隧道，该选择会在 Web Host 后续启动时保留，也可从同一页面关闭。

当前实现的基础包括：

- Rust Host、Agent、Agent Loop、会话、工具、系统提示词和与提供商无关的 LLM 运行时；
- 由会话拥有的原生 Agent Mode：Standard、PTC、Minimal、Composition，以及严格的自定义 `mode.toml` 包；
- 持久化 JSONL/SQLite 会话、冷恢复、回滚和有界传输；
- 无头 CLI，以及带 TypeScript 和 Python 客户端的 NDJSON JSON-RPC/ACP SDK；
- HTTP 全表单 RPC、持久 SSE 和 Browser WebSocket 下行链路；
- 原生/WASM/Browser 路由，以及通过有界 `cordis.node/v1` 桥和 DomainBridge 服务实现的真实 Legacy Node compat-host；
- Extism 服务权限、设置/凭据、多工作区权限、附件、OpenAI Responses 适配器，以及冻结的上游 `AppWebEntry` 源码 shell；
- 由 pnpm 管理的插件配置，具有有序的 Host Bundle 权限、精确的 Loader/Fiber 清单、有界 HTTP/WebSocket 插件路由、打包的 Host 兼容模块、第一方 `tessivum-market`、经验证的 `dsh-better-sidebar@0.16.1`、固定的 `dshmarket@1.29.2` 与 `dsh-dream-skin@8.30.1` 兼容样例，以及版本化 Node Host facade 和由 Rust 拥有的远程访问。

冻结的 DeepSeek Harness `0.1.0-rc.5` 基线（提交 `47f943859bef60e4160492346772ded9b24f765a`）仍是兼容性目标。Tessivum `v0.1.0-alpha.23` 固定 `tessivum-core v0.1.6` 的修订版 `bafb893f182d64b7b464b6cf827676f7ac368168`；当前已实现的切片为：

- 源码 Web shell 和全部 38 个组合客户端包均从提交 `47f943859bef60e4160492346772ded9b24f765a` 构建；Tessivum 会先应用已检入的兼容补丁，再审计并构建源码树；
- Rust `/api` 分发器实现全部 52 个冻结的 Core RPC 方法名和两条 Browser WebSocket 下行链路；
- 与提供商无关的流式处理、基本重试/取消、持久会话、原生 Agent Mode、Subagents、Workflow 和原生/WASM 工具都有针对性的 Rust 契约；
- 全部 69 个源码 Web Chromium 场景仍是 Browser schema/event 对等性的门槛。

完整的 Agent/LLM 兼容性尚未完成：block/chunk/source/usage 线路保真度、冻结的 prepared-call/header/context 语义、可恢复的重试账本、上游 queue/steer 状态机和 JSONL replay consumer 仍未实现。权威边界见 [`docs/COMPATIBILITY_BASELINE.md`](docs/COMPATIBILITY_BASELINE.md)。

Alpha.15 的 DSH Profile 权限、市场激活、升级、回滚和分发别名门槛记录于 [`docs/PHASE6_DSH_PROFILE_COMPATIBILITY_PLAN.md`](docs/PHASE6_DSH_PROFILE_COMPATIBILITY_PLAN.md)。
Alpha.18 的第一方市场所有权、打包、迁移、精确版本变更、重启和 Browser E2E 门槛记录于 [`docs/PHASE7_FIRST_PARTY_MARKET_PLAN.md`](docs/PHASE7_FIRST_PARTY_MARKET_PLAN.md)。
Alpha.19-A/B 的兼容性预检和 Legacy Host facade，以及 Alpha.19-C/D 由 Rust 拥有的远程访问及其内建配对/设备界面均已完成。契约、安全边界、精确兼容状态、Browser 证据和发布门槛记录于 [`docs/PHASE8_REMOTE_ACCESS_COMPATIBILITY_PLAN.md`](docs/PHASE8_REMOTE_ACCESS_COMPATIBILITY_PLAN.md)。

Phase 9 已完成。固定 Linux 30 样本运行现在同时配对 Core 和完整产品路径。tessivum-core 相比 TypeScript Cordis 4.0.1 实现 **Scope 创建/销毁快 24.05×**、**1,000 个 Scope 存活时的进程 PSS 低 17.15×**。四个真实 Chromium 产品单元均通过 **30/30**，关闭后进程残留为零。与 DeepSeek Harness `0.1.0-rc.5` 相比，Tessivum Base 的 HTTP ready **快 5.83×**、空闲 Host 进程树 PSS **低 4.52×**；Compatibility 的空闲 PSS **低 1.63×**，但 Legacy Node 插件桥使 HTTP ready **慢 9.31×**。见[中文 Benchmark 报告](docs/PHASE9_BENCHMARK_REPORT.zh-CN.md)、[插件证据](docs/PLUGIN_VERIFICATION_REPORT.md)与 [Phase 9 计划](docs/PHASE9_BENCHMARK_ECOSYSTEM_PLAN.md)。

## 架构

```text
Rust CLI / HTTP / SDK
        |
Rust Host + Agent Runtime
        |
Native Agent Mode Registry
Standard | PTC | Minimal | Composition | custom mode.toml
        |
Tessivum Core
   |         |          |
Native    Extism     Legacy Node
plugins   boundary   npm/Cordis plugins
        \
         Browser Cordis + React UI
```

Host 始终是权限和持久事实的权威。Legacy Node 和 Browser Cordis 是有意保留的兼容平面，不是临时的 TypeScript Host/Agent 实现。

### Legacy Node 边界

Tessivum 不会把旧版 Host 模块 `agentCore`、`llm`、`systemPrompt`、`sessionStore` 或 `toolRuntime` 导出到 Node Cordis 上下文。要求其中任一模块名的插件均不受支持：直接查询仍应缺失，具名桥接调用必须返回明确的 `SERVICE_UNAVAILABLE`/`UNKNOWN_SERVICE` 错误——绝不能成功地无操作、返回空模型结果或伪造服务。

支持的跨运行时操作使用有界、版本化的 DomainBridge 契约（`agents@1`、`llm@1`、`systemPrompt@1`、`sessions@1` 和 `tools@1`）。这些契约不是被省略的旧版模块的别名。

### 原生 Agent Mode

Agent Mode 是不可变、由会话拥有的包，包含提示词策略、面向模型的工具呈现、原生工具允许列表、Skills、规划、压缩以及原生/WASM/Legacy 插件条目。Rust Host 拥有解析和生命周期；Browser `agentPreset.*` 名称仅为冻结线路兼容性而保留。Tessivum 不执行上游 `agent.cordis.yml` 或任意 JavaScript 模式定义。

内建模式：

- **Standard** 直接暴露 Host 可用的原生工具目录。
- **PTC** 暴露一个由 Bun 支持且带受限嵌套原生工具 SDK 的 `run_code` 工具。
- **Minimal** 暴露持久化 `bash` 和带完整替换提示词的 `str_replace_editor`。
- **Composition** 增加有类型的 `composition_inspect`、`composition_define`、`composition_validate`、`composition_run` 和 `composition_stop` 工具；描述符可引用原生、WASM 或 Legacy 条目，但不能包含可执行源码。

自定义模式位于 `${TESSIVUM_HOME:-$HOME/.tessivum}/modes/<id>/mode.toml`。若要使用隔离的数据根：

```toml
# /tmp/tessivum-data/modes/review/mode.toml
schema = 1
id = "review"
name = "Review"
description = "Read-only repository review"

[prompt]
complete = true
text = "Review the workspace without modifying it."

[tools]
presentation = "direct"
enabled = ["fs.read", "search.glob", "search.grep"]

[capabilities]
skills = false
planning = false
compaction = false
```

通过有序 CLI 补丁选择它：

```yaml
# /tmp/review-mode.yml
agent-presets:
  default: review
```

```bash
tessivum --data-dir /tmp/tessivum-data web --patch /tmp/review-mode.yml
```

后面的 `--patch` 文件会递归覆盖前面的文件。未知字段、未知工具能力、重复 ID、路径逃逸、缺少必需原生工具、缺少 Bun，以及不可用的插件运行时都会以结构化错误失败；Tessivum 不会悄悄扩展或降级模式。

## 要求

从源码构建需要带有 `rustfmt` 和 `clippy` 的 Rust stable、Bun 1.3.14 或更高版本、pnpm 10 或更高版本、Git，以及能访问固定 `tessivum-core` 修订版和冻结 npm 输入的网络。

预构建归档不需要 Rust、Git 或系统 Node.js。没有 Legacy 插件的无头运行不需要 Bun 或 pnpm。打包的第一方市场是 Legacy Node + Browser 插件，因此 `tessivum web` 需要 Bun，并在首次安装或升级时使用 pnpm；Homebrew formula 会安装这两个运行时依赖。

## 安装

### Homebrew Tap

```bash
brew tap wavetao2010/tap
brew install tessivum
tessivum --version
tsv --version
```

### 无 sudo 安装器

请先下载安装器再运行；它会将带版本的发布版本安装到 `~/.local/lib/tessivum`，并原子更新 `tessivum` 与 `tsv` 启动器：

```bash
curl -fsSLO https://raw.githubusercontent.com/wavetao2010/tessivum/v0.1.0-alpha.23/install.sh
sh install.sh 0.1.0-alpha.23
```

脚本会验证相邻的 SHA-256 文件、拒绝不安全的归档路径、不使用 `sudo`，也不会修改 shell 启动文件。

### 预构建归档

从 [Alpha.23 发布页](https://github.com/wavetao2010/tessivum/releases/tag/v0.1.0-alpha.23) 下载适用于平台的归档及相邻的 `.sha256` 文件，验证校验和后运行任一打包启动器：

```bash
target=x86_64-unknown-linux-gnu  # 或 aarch64-unknown-linux-gnu、x86_64-apple-darwin、aarch64-apple-darwin
sha256sum -c "tessivum-0.1.0-alpha.23-$target.tar.gz.sha256"
tar -xzf "tessivum-0.1.0-alpha.23-$target.tar.gz"
"./tessivum-0.1.0-alpha.23-$target/bin/tessivum" --version
"./tessivum-0.1.0-alpha.23-$target/bin/tsv" --version
```

在 macOS 上，请使用 `shasum -a 256 -c`，而非 `sha256sum -c`。归档经过校验和验证，但未进行代码签名或公证。

## 源码快速开始

### 确定性的无头 smoke

这会运行已检入的录制模型流，并明确启用受信任的 Bash fixture：

```bash
cargo run --release -- \
  --session alpha-smoke \
  --data-dir /tmp/tessivum-alpha \
  --replay fixtures/headless/recorded-replay.jsonl \
  --trusted-bash \
  "prove the CLI tool round trip"
```

预期最终输出：

```text
CLI tool round trip complete: CLI_TOOL_ROUND_TRIP
```

`--trusted-bash` 会授予进程原生 shell 权限。请勿将其用于不受信任的提示词。

### OpenAI Responses 中继

原生适配器面向带 Bearer 认证的标准 Responses 协议。`OPENAI_BASE_URL` 是前缀；Tessivum 会附加 `/responses`。

```bash
export OPENAI_API_KEY='relay-key'
export OPENAI_BASE_URL='https://relay.example/v1'
export OPENAI_MODEL='codex-model-name'

# Browser 或 SDK
cargo run --release -- web

# 无头
cargo run --release -- \
  --provider openai-responses \
  --model "$OPENAI_MODEL" \
  "inspect this repository"
```

Responses 适配器发送 `store: false`，流式传输文本/推理/函数调用，为无状态工具调用续接持久化加密的推理项，并将经过验证的 AttachmentRef 图像具体化为 Responses 数据 URL。自定义提供商路由还会将 `openai-completions` 分发至 `/chat/completions`，将 `anthropic-messages` 分发至 `/messages`；所选协议不再被当作 Responses。`openai-codex-responses` 仍不在范围内，因为它是 OAuth 传输，而不是 API-key Responses 契约。

### 第一方市场和社区插件

打包启动器携带 `tessivum-market-<version>.tgz` 及其 SHA-256 文件。执行 `tessivum web` 时，Host 会验证校验和，将工件复制到 `${TESSIVUM_HOME:-$HOME/.tessivum}/artifacts/market/<version>/`，在 pnpm 管理的插件配置中安装或升级它，并替换已有的 `dshmarket`/`dsh-market` 依赖与 bundle 条目，而不丢弃 `.dsh-market/state.json`。

所有其他包变更都面向 `${TESSIVUM_HOME:-$HOME/.tessivum}/plugins` 并使用 pnpm。除非配置中存在非空、明确的 `pnpm.onlyBuiltDependencies` 允许列表，否则安装会忽略生命周期脚本。`package.json.dependencies` 是已安装包的清单；`dsh.profile.bundles` 是唯一的 Host Bundle 启用和排序权威。通用仅客户端包不进入 Host Bundle 栈。

```bash
tessivum plugin add @scope/package
tessivum plugin remove @scope/package

# 冻结的兼容目标
tessivum plugin add dsh-better-sidebar@0.16.1
tessivum plugin add dsh-dream-skin@8.30.1
tessivum web
```

`tessivum` 和 `tsv` 是同一启动器，并解析至同一数据根。CLI 或市场变更报告“重启后生效”后，请重启 Web 进程。`tessivum-market` 读取当前 Profile 及已稳定的 Loader/Fiber 清单；Tessivum 不会暴露全局 `dsh` shim，也不维护第二份 Node 侧 Loader 状态。

Market 使用 awesome-dsh-plugin 的实时社区目录，并叠加 Tessivum 自己维护的精确版本证据。卡片区分 **Tessivum 官方**、**Tessivum 已验证 · VERSION** 和 **DSH 社区 · 未验证**；验证只适用于标明的版本，是兼容证据而非安全审计。详见[投稿与验证流程](docs/PLUGIN_VERIFICATION.zh-CN.md)和 [DSH-better-sidebar 0.16.1 证据](docs/PLUGIN_VERIFICATION_REPORT.md)。

Legacy 插件及其生命周期脚本是以用户权限运行的受信任代码，不是沙箱。`web.route/v1` 注册仍由 Rust 拥有，保持同源、前缀受限、大小有界、截止时间有界、可取消且按 generation 作用域隔离。打包部署相对于启动器定位 compatibility host、Cordis vendor、Host 模块和 Agent Presets；源码检出使用其固定的开发路径。

### Browser shell

```bash
cd web
bun install --frozen-lockfile
bun run build
cd ..
cargo run --release -- web
```

打开 <http://127.0.0.1:3000>。Web 可从已发布的 Models/Settings 界面配置中继；`OPENAI_*` 仍可供无头、SDK、CI 和托管部署使用。

### 远程访问

远程访问 **disabled by default**，Rust 监听器保持 **loopback-only**。正常启动 Tessivum，打开 **Settings → Remote access**（或 <http://127.0.0.1:3000/remote>），阅读公网隧道提示后点击 **Enable with Cloudflare**。Tessivum 会记住选择、重启 Web Host，并在之后的 `tessivum web` 启动时恢复隧道。同一页面可关闭远程访问并撤销已配对设备。不需要环境变量、域名、DNS 变更或 Cloudflare 账户。

Tessivum 会先使用绝对路径的 `TESSIVUM_CLOUDFLARED` 覆盖，其次使用 `PATH` 中的 `cloudflared`；否则会为受支持的 macOS/Linux 架构下载固定发布版本，验证其 SHA-256 摘要，并将其缓存于所选数据根的 `bin` 目录。启动后会打印临时 `https://*.trycloudflare.com/remote` URL。Rust Host 拥有配对和授权；`cloudflared` 仅传输流量。隧道退出时，Tessivum 立即移除旧权限，以有界指数退避重启，并原子安装替代权限。若在后续启动时无法启动已记住的隧道，Tessivum 会关闭远程访问，同时保持本地 Web Host 可用。

对于托管部署，`TESSIVUM_REMOTE_ACCESS=0|1` 会明确覆盖已记住的选择。`TESSIVUM_REMOTE_AUTO_TUNNEL=cloudflare` 会明确选择 Quick Tunnel 路径。

若要使用稳定的运营者自有域名或命名隧道，请省略 `TESSIVUM_REMOTE_AUTO_TUNNEL`，改为配置手动路径：

```bash
export TESSIVUM_REMOTE_ACCESS=1
export TESSIVUM_REMOTE_TRUSTED_TUNNEL=1
export TESSIVUM_WEB_TRUSTED_AUTHORITIES=app.example.test
tessivum web
```

手动隧道必须将 `https://app.example.test` 转发至 loopback 监听器，保留精确的 `Host` 和浏览器 `Origin`，并设置 `X-Forwarded-Proto: https`。多个精确 authority 以逗号分隔。不支持通配符 Host、普通远程 HTTP 或自动 LAN 绑定。Cloudflare 为 Quick Tunnel 终止 TLS，并能看到被代理的流量；Quick Tunnel URL 是临时的，不承诺可用性。

在本地打开 <http://127.0.0.1:3000/remote> 以生成短时 QR/link 并管理设备。链接只在 URL fragment 中携带一次性令牌。成功的远程交换会存储 `HttpOnly`、`Secure`、`SameSite=Strict` 设备 cookie，并重定向至现有 Tessivum Web shell；之后 API、SSE 和 WebSocket 请求会通过同一 Rust authority middleware。匿名远程访问只限于有界的公开姿态读取、配对交换和固定 Browser 静态资源。远程浏览器可读取渲染 shell 所需的已脱敏设置和凭据元数据，但 Legacy Node Web 路由，以及设置、凭据、工作区、文件系统、plugin-host 激活和 Host 关闭变更仍然仅限 loopback。撤销会立即关闭实时流连接并拒绝后续请求。

提示词准入具有持久性：已接受的 Agent 工作会在标签页重载、浏览器断开、会话过期或设备撤销后继续在本地 Host 中运行。撤销会移除观察和控制，不会悄悄取消工作。若必须取消正在运行的任务，请在断开前使用 Web 的 **Stop** 操作。

设备会话具有可配置的有界生命周期：`TESSIVUM_REMOTE_SESSION_TTL_SECONDS` 接受 300 秒至 7,776,000 秒（90 天），默认 2,592,000 秒（30 天）。启动会拒绝范围外的值。

只有哈希和已脱敏设备元数据会以 `0600` 模式持久化到 `${TESSIVUM_HOME:-$HOME/.tessivum}/remote-access.json`。删除该文件以重置全部配对和设备前，请先停止 Tessivum。原始 `@linxin666/dsh-remote-web-ui@0.3.6` 仍是已审计但不受支持的参考 fixture；其 Node gate、loopback proxy、隧道、更新和遥测权限均未安装。

### SDK 模式

```bash
cargo run --release -- sdk
```

SDK 模式从 stdin 读取以换行分隔的 JSON-RPC，并向 stdout 写入协议帧。客户端实现位于 [`sdk/typescript`](sdk/typescript) 和 [`sdk/python`](sdk/python)。

## 数据迁移、升级、回滚和卸载

数据根优先级依次为 `--data-dir`、绝对路径的 `TESSIVUM_HOME`、`$HOME/.tessivum`。若新默认位置不存在而 `./.tessivum` 存在，Tessivum 会停止并给出迁移诊断，而不是悄悄创建第二棵状态树。请备份两个目录，然后只在目标不存在时显式移动旧目录：

```bash
test ! -e "$HOME/.tessivum" && mv ./.tessivum "$HOME/.tessivum"
```

Alpha.11 使 pnpm 成为唯一的插件配置变更后端。Alpha.15 增加有序的 `dsh.profile.bundles` 权威，而不改变依赖、会话、设置或凭据；显式的空 bundle 数组会保留。迁移或恢复前请备份 `$TESSIVUM_HOME/plugins`（或 `$HOME/.tessivum/plugins`）。

Homebrew 升级会切换程序而不删除用户数据。无 sudo 安装器会保留带版本的程序目录；重新运行 `sh install.sh <older-version>` 会原子地重新指向启动器。二进制回滚不会重写 Alpha 数据或插件配置；若较新发布改变了它，请恢复匹配的备份。

```bash
brew upgrade tessivum                # 仅程序；保留用户数据
brew uninstall tessivum              # 仅程序
sh install.sh --uninstall            # 仅程序
rm -rf "${TESSIVUM_HOME:-$HOME/.tessivum}"  # 明确的破坏性数据删除
```

## 安全和来源

- 发布归档从带标签的 Tessivum 源码和 `Cargo.lock` 中精确的 `tessivum-core` Git 修订版构建；Browser 基线固定为 DeepSeek Harness 提交 `47f943859bef60e4160492346772ded9b24f765a`。
- Host 兼容 npm 输入在 `packaging/host-modules.json` 中有精确版本、registry URL、SHA-512 integrity、文件哈希和许可证；归档包含 `THIRD_PARTY_LICENSES.txt` 和 `release-metadata.json`。
- 安装器和 Homebrew formula 使用相同的四个发布归档及固定 SHA-256 值。发布装配中不存在浮动的 `latest` 包解析。
- HTTP 监听器为 loopback-only。Legacy Node 插件和 pnpm 子进程不是沙箱；安装前请检查包，并保持生命周期脚本禁用，除非明确需要。
- 远程请求需要明确受信任的 HTTPS authority、同源浏览器元数据、受信任的 tunnel marker 和存活的 Rust 拥有设备会话。配对签发和其他 Host 变更仍只限 loopback。
- 校验和可检测损坏，但不是签名。Alpha.23 二进制和第一方市场工件未进行代码签名或公证；执行前请验证发布标签、校验和资源和仓库来源。

## 可复现 Benchmark

固定 Ubuntu 24.04 arm64 路径会在测量前构建依赖，交错执行 process-cold Core A/B 和 Tessivum/DeepSeek Harness 产品样本，以真实 Chromium 驱动 Web UI，并从 Linux `smaps_rollup` 统计 Host 全部后代进程的 PSS。Alpha.23 公开结果在每个 runtime/manifest 单元中包含 30 个有效样本：**Scope 创建/销毁快 24.05×**、**Service 查找吞吐 20.53×**、**Event 吞吐 25.42×**、**1,000 个 Core Scope 存活时的进程 PSS 低 17.15×**，以及四个产品单元全部 **30/30** 成功。报告同时披露 Core `loader_update` **慢 40.05×** 及 Tessivum Compatibility 的 HTTP ready **慢 9.31×**。

```bash
SAMPLES=30 ./benchmarks/run-linux-container.sh
python3 scripts/check_release_facts.py
```

原始输出写入 `benchmarks/results/`。已检入的 [Core 原始 JSON](benchmarks/fixtures/phase9-alpha23/core-paired-30.json)、[配对产品原始 JSON](benchmarks/fixtures/phase9-alpha23/product-30.json)和[中文报告](docs/PHASE9_BENCHMARK_REPORT.zh-CN.md)是公开证据。产品对比使用相同的可见离线 Prompt/工具契约和插件启动图；两边的原生 replay 适配器与内部代码路径有意不同。

## 验证

```bash
python3 scripts/check_compat_baseline.py
python3 scripts/check_release_facts.py
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cd web && bun install --frozen-lockfile && bun run build && bun run test:source-client
cd ../plugins/market && bun install --frozen-lockfile && bun run check && bun run test
cd ../../web && bun test ./tests/migrated.test.ts --max-concurrency 1 --timeout 1200000
```

这些门槛覆盖 Rust 原生运行时、固定的 DeepSeek 客户端包、全部 69 个源码 Web Chromium 场景以及第一方 Market 和 Remote Access 场景、无头与 SDK 流程、Extism 权限、持久化、回滚、工作区隔离和关闭。

## 已知 Alpha 限制

- 只有已接入生产的 cwd-sensitive 能力按工作区作用域隔离；仅库中存在的潜在 Skills/LSP/Filesystem 集成仍无法从 Host 使用；
- Browser 配置只暴露已发布的 settings namespace allowlist；任意已注册 namespace 仍为 Host 内部；
- WASM 产品权限目前仅暴露 `logger@1.log`、`tools@1.schemas`、`settings@1.describe`、`credentials@1.describe` 和 `systemPrompt@1.assemble`；
- 原生 Responses 适配器需要标准的 API-key `/responses` 契约；未接入直接的 ChatGPT/Codex OAuth 或远程图像 URL；
- 含图像的 MCP/tool-result 序列化已由针对性适配器测试覆盖；真实 Browser E2E 目前仅演练文本工具续接和用户图像输入，因为没有配置生产可用的图像生成工具；
- Agent/LLM 兼容性在上面列出的 wire、retry-ledger、queue/steer 和 replay-consumer 边界仍不完整；
- API 监听器为 loopback-only；预构建归档经过校验和验证，但未进行代码签名或公证；
- **Tessivum 已验证** 当前只覆盖 `dsh-better-sidebar@0.16.1`；`dshmarket@1.29.2` 与 `dsh-dream-skin@8.30.1` 仍是固定运行时兼容样例，不是已验证社区发行版。其他包、其他版本和热激活在单独验证前均不受支持。

这些是产品后续工作，不是改变或弃用官方 DeepSeek Harness 项目的工作。

## 文档

- [运行时架构](docs/ARCHITECTURE.md)
- [开发和切换计划](docs/DEVELOPMENT_PLAN.md)
- [插件兼容性](docs/PLUGIN_COMPATIBILITY.md)
- [Phase 3 产品能力计划](docs/PHASE3_PRODUCT_PLAN.md)
- [Phase 4 品牌、分发和 dshmarket 计划](docs/PHASE4_BRAND_DISTRIBUTION_MARKET_PLAN.md)
- [Phase 6 DSH Profile 兼容性和 `tsv` 命令计划](docs/PHASE6_DSH_PROFILE_COMPATIBILITY_PLAN.md)
- [Phase 7 第一方市场和 Host 重启计划](docs/PHASE7_FIRST_PARTY_MARKET_PLAN.md)
- [Phase 8 远程访问和较新 Legacy Host 兼容性计划](docs/PHASE8_REMOTE_ACCESS_COMPATIBILITY_PLAN.md)
- [Phase 9 Benchmark 与社区验证计划](docs/PHASE9_BENCHMARK_ECOSYSTEM_PLAN.md)及[30 样本 Benchmark 报告](docs/PHASE9_BENCHMARK_REPORT.zh-CN.md)
- [社区插件投稿与验证](docs/PLUGIN_VERIFICATION.zh-CN.md)及 [DSH-better-sidebar 0.16.1 证据](docs/PLUGIN_VERIFICATION_REPORT.md)
- [DeepSeek Harness 兼容性基线](docs/COMPATIBILITY_BASELINE.md)
- [Web E2E 移植检查表（69 个上游文件）](docs/WEB_E2E_PORT_CHECKLIST.md)

## 许可证

Tessivum 源码采用 [MIT License](LICENSE) 授权。发布归档还包含 `THIRD_PARTY_LICENSES.txt`；兼容性不会对随附的 Cordis、源自 DeepSeek Harness 的 Browser 源码或 npm 依赖重新授权。


Wall time: 0.03 seconds
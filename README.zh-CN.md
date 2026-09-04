# Tessivum

[![CI](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml/badge.svg)](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml)

[English](README.md) | 简体中文

> 道器相成  
> Principle and implementation, in concert.

Tessivum 是独立的 Rust 原生智能体框架。Host、Agent、会话、工具、API 和 SDK 运行在 Rust 中；Legacy Node 与 Browser Cordis 作为现有插件和上游 React UI 的明确兼容边界保留。

> Tessivum 是社区项目，与 DeepSeek 没有关联，也不是官方 DeepSeek 项目。

## 当前状态

`v0.1.0-alpha.23` 是预发布版本。它使用 `tessivum-core v0.1.6` 修订版 `4a287c7caab9e715725c93ad4416660f51b77840`，兼容目标为 DeepSeek Harness `0.1.0-rc.5` 提交 `47f943859bef60e4160492346772ded9b24f765a`。

当前已有：

- Rust 原生 Host、Agent Loop、会话、工具、持久化、HTTP、SSE、WebSocket 与 SDK 运行时；
- Standard、PTC、Minimal、Composition 和严格的自定义 Agent Mode；
- Native、Extism/WASM、Browser 和有界 Legacy Node 插件路径；
- 无头 CLI、TypeScript/Python SDK 客户端和源码兼容的 React Web shell；
- OpenAI Responses 兼容中继、附件、多工作区权限、插件管理、第一方市场和由 Rust 掌权的远程访问。

完整的 DeepSeek Harness Agent/LLM 线路兼容尚未完成。准确边界见[兼容性基线](docs/COMPATIBILITY_BASELINE.md)和[插件兼容矩阵](docs/PLUGIN_COMPATIBILITY.md)。

## 安装

### 支持的安装包

| 平台 | 架构 | 归档 | SHA-256 |
| --- | --- | --- | --- |
| macOS | Apple Silicon | [`.tar.gz`](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-aarch64-apple-darwin.tar.gz) | [校验和](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-aarch64-apple-darwin.tar.gz.sha256) |
| macOS | Intel | [`.tar.gz`](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-x86_64-apple-darwin.tar.gz) | [校验和](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-x86_64-apple-darwin.tar.gz.sha256) |
| Linux（glibc） | ARM64 | [`.tar.gz`](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-aarch64-unknown-linux-gnu.tar.gz) | [校验和](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-aarch64-unknown-linux-gnu.tar.gz.sha256) |
| Linux（glibc） | x86_64 | [`.tar.gz`](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-x86_64-unknown-linux-gnu.tar.gz) | [校验和](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| Windows 原生 | x86_64/ARM64 | 暂未发布 | — |

现有安装器**并非只能用于 macOS**：它会自动识别 macOS/Linux 的 x86_64 与 ARM64。Windows 原生版本尚未通过发布构建、归档、启动器、插件、Browser、升级和进程清理门槛。Windows 用户可以暂时在 WSL2 内运行 Linux 包，但这条路径尚未经过发布验证；Linux `.tar.gz` 不能直接作为 Windows 原生程序运行。

### Homebrew——macOS 或 Linux

Homebrew 是最短路径，并会安装 Web 与 Legacy 插件所需的 Bun 和 pnpm：

```bash
brew tap wavetao2010/tap
brew install tessivum
tessivum --version
tsv --version
```

### 无 sudo 安装器——macOS 或 Linux

安装器会选择正确的发布归档、验证 SHA-256、安装到 `~/.local/lib/tessivum`，并更新 `~/.local/bin` 下的启动器：

```bash
curl -fsSLO https://raw.githubusercontent.com/wavetao2010/tessivum/v0.1.0-alpha.23/install.sh
sh install.sh 0.1.0-alpha.23
export PATH="$HOME/.local/bin:$PATH"
tessivum --version
```

它不使用 `sudo`，也不修改 shell 启动文件。使用 `tessivum web` 或 Legacy Node 插件前，需另行安装 Bun 1.3.14+ 和 pnpm 10+。

### 手动下载归档——macOS 或 Linux

从 [Alpha.23 发布页](https://github.com/wavetao2010/tessivum/releases/tag/v0.1.0-alpha.23)下载归档及相邻的 `.sha256` 文件，并从上面的四个 target 中选择一个。

Linux 示例：

```bash
version=0.1.0-alpha.23
target=x86_64-unknown-linux-gnu # ARM64 使用 aarch64-unknown-linux-gnu
base="https://github.com/wavetao2010/tessivum/releases/download/v$version"
curl -fLO "$base/tessivum-$version-$target.tar.gz"
curl -fLO "$base/tessivum-$version-$target.tar.gz.sha256"
sha256sum -c "tessivum-$version-$target.tar.gz.sha256"
tar -xzf "tessivum-$version-$target.tar.gz"
"./tessivum-$version-$target/bin/tessivum" --version
```

macOS 请选择 `apple-darwin` target，并将 `sha256sum -c` 替换为 `shasum -a 256 -c`。

### 从源码运行

源码构建需要带 `rustfmt`、`clippy` 的 Rust stable、Bun 1.3.14+、pnpm 10+、Git，以及访问固定依赖的网络。

```bash
git clone https://github.com/wavetao2010/tessivum.git
cd tessivum
cargo run --release -- web
```

Windows 原生源码构建目前没有经过发布测试，因此不属于受支持安装路径。

## 启动

```bash
tessivum web
```

打开 <http://127.0.0.1:3000>，然后在 **Models/Settings** 中配置模型中继。

标准 OpenAI Responses 兼容端点也可使用环境变量：

```bash
export OPENAI_API_KEY='relay-key'
export OPENAI_BASE_URL='https://relay.example/v1'
export OPENAI_MODEL='model-name'

tessivum --provider openai-responses --model "$OPENAI_MODEL" "inspect this repository"
```

`OPENAI_BASE_URL` 是前缀；Tessivum 会附加 `/responses`。自定义提供商路由还支持 `/chat/completions` 与 `/messages`。尚未实现直接 ChatGPT/Codex OAuth。

## 插件

第一方市场会在 `tessivum web` 时安装或升级。其他包变更使用 `${TESSIVUM_HOME:-$HOME/.tessivum}/plugins` 下由 pnpm 管理的 Profile：

```bash
tessivum plugin add @scope/package
tessivum plugin remove @scope/package

# 固定兼容样例
tessivum plugin add dsh-better-sidebar@0.16.1
tessivum plugin add dsh-dream-skin@8.30.1
```

Legacy Node 插件和生命周期脚本以用户权限执行，不属于 WASM 沙箱。只有[插件兼容矩阵](docs/PLUGIN_COMPATIBILITY.md)明确列出的版本具有兼容证据。

## 架构

```text
CLI / HTTP / SDK / React Web
              |
      Rust Host + Agent Runtime
              |
         Tessivum Core
       /        |         \
   Native   Extism/WASM   Legacy Node + Browser Cordis
```

Rust Host 始终是权限、持久状态、工具与远程访问的权威。详细设计见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)。

## 远程访问

远程访问默认关闭（**disabled by default**），Rust listener 保持仅回环（**loopback-only**）。启动 `tessivum web`，打开 **Settings → Remote access** 或 <http://127.0.0.1:3000/remote>，阅读公网隧道提示后，才能显式启用 Cloudflare Quick Tunnel。

Tessivum 掌管配对、设备会话、撤销以及 HTTP/WebSocket 授权。Cloudflare 仅传输流量且能够观察流量；Quick Tunnel URL 是临时的。详见[远程访问安全边界](docs/PHASE8_REMOTE_ACCESS_COMPATIBILITY_PLAN.md)。

## 数据、升级与卸载

数据根优先级为 `--data-dir`、绝对路径 `TESSIVUM_HOME`、`$HOME/.tessivum`。程序升级和卸载不会删除用户数据。

```bash
brew upgrade tessivum
brew uninstall tessivum

# 无 sudo 安装
sh install.sh 0.1.0-alpha.23
sh install.sh --uninstall

# 显式且具有破坏性的数据删除
rm -rf "${TESSIVUM_HOME:-$HOME/.tessivum}"
```

切换 Alpha 版本前请备份数据根。二进制回滚不会降级数据或插件 schema。

## 安全与发布来源

- 发布归档与第一方市场工件包含 SHA-256 校验和及来源元数据；
- 校验和可以检测损坏，但不是签名；Alpha.23 二进制未进行代码签名或公证；
- 除非显式启用带独立权限检查的远程访问，否则 HTTP listener 保持 loopback-only；
- Legacy Node 插件和 pnpm 子进程是受信任的本地代码，不是沙箱扩展。

## 可复现 Benchmark

公开的 Linux 30 样本结果显示，相比 TypeScript Cordis 4.0.1，tessivum-core 的 Scope 创建/销毁**快 24.05×**、Service 查找吞吐为 **20.53×**、Event 吞吐为 **25.42×**，1,000 个 Scope 存活时 PSS **低 17.15×**；同时也披露 Loader 更新路径**慢 40.05×**。

四个真实 Chromium 产品单元均通过 **30/30**。与 DeepSeek Harness 相比，Tessivum Base 的 HTTP ready **快 5.83×**、空闲 Host 进程树 PSS **低 4.52×**；Compatibility 的空闲 PSS **低 1.63×**，但 HTTP ready **慢 9.31×**。

证据见[中文 Benchmark 报告](docs/PHASE9_BENCHMARK_REPORT.zh-CN.md)、[Core 原始 JSON](benchmarks/fixtures/phase9-alpha23/core-paired-30.json)与[产品原始 JSON](benchmarks/fixtures/phase9-alpha23/product-30.json)。这些是固定的离线工作量，不是生产 LLM 或并发用户 Benchmark。

## 开发验证

```bash
python3 scripts/check_compat_baseline.py
python3 scripts/check_release_facts.py
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## 文档

- [架构](docs/ARCHITECTURE.md)
- [兼容性基线](docs/COMPATIBILITY_BASELINE.md)
- [插件兼容性](docs/PLUGIN_COMPATIBILITY.md)
- [社区插件投稿与验证](docs/PLUGIN_VERIFICATION.zh-CN.md)
- [远程访问兼容性与安全](docs/PHASE8_REMOTE_ACCESS_COMPATIBILITY_PLAN.md)
- [Benchmark 与生态计划](docs/PHASE9_BENCHMARK_ECOSYSTEM_PLAN.md)
- [30 样本 Benchmark 报告](docs/PHASE9_BENCHMARK_REPORT.zh-CN.md)
- [开发历史](docs/DEVELOPMENT_PLAN.md)

## 许可证

Tessivum 源码采用 [MIT License](LICENSE) 授权。发布归档还包含 `THIRD_PARTY_LICENSES.txt`；兼容性不会对随附的 Cordis、源自 DeepSeek Harness 的 Browser 源码或 npm 依赖重新授权。
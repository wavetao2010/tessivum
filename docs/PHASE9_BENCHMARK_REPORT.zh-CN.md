# Tessivum 0.1.0-alpha.23 Benchmark 报告

[English](PHASE9_BENCHMARK_REPORT.md)

日期：2026-09-04  
状态：公开级运行通过  
样本：每个 runtime/manifest 单元各 30 次冷进程重复

## 结论

在冻结的 Core 工作量上，tessivum-core 相比 `@deepseek-ai/cordis` 4.0.1，**Scope 创建/销毁快 24.05×**、**Service 查找吞吐为 20.53×**、**Event 吞吐为 25.42×**，1,000 个 Scope 存活时的进程 PSS **低 17.15×**。两种 runtime 在 root dispose 后的存活注册数均为零。

产品对比的四个 runtime/manifest 单元均通过 **30/30**：Tessivum Base、DeepSeek Harness Base、Tessivum Compatibility 和 DeepSeek Harness Compatibility。全部 120 个样本都使用全新的 Host 和数据目录，以真实 Chromium 驱动 Web UI，按相同的可见 Prompt/工具 marker 契约完成十个常驻 Session，并在关闭后留下零 Host、Browser 或子进程残留。

在 Base 中，Tessivum 的 Headless replay **快 13.78×**、HTTP ready **快 5.83×**、空闲 Host 进程树 PSS **低 4.52×**、十 Session 增量 PSS **低 2.50×**。在 Compatibility 中，Tessivum 的空闲 PSS 仍**低 1.63×**、十 Session 增量 PSS **低 2.42×**，但 Legacy Node 插件桥使 HTTP ready 比 DeepSeek Harness **慢 9.31×**（`5,002.07` 对 `537.34` ms）。DeepSeek Harness 在两个 manifest 中完成十 Session 也约快 7%。tessivum-core 的 Core `loader_update` 路径仍有显著的 **40.05× 回归**。

## 冻结环境

| 项目 | 值 |
|---|---|
| 宿主机 | MacBook Pro `Mac17,2`，Apple M5，10 CPU 核，16 GB RAM |
| 执行环境 | Docker Desktop Linux arm64 VM |
| 容器基础镜像 | Ubuntu 24.04，`ubuntu@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517` |
| 容器内核 | Linux `6.12.76-linuxkit` |
| 容器配额 | 10 CPU，7.75 GiB（`8,321,515,520` bytes） |
| Rust | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| Bun | `1.4.0` |
| Node.js | `v22.19.0` |
| pnpm | `11.7.0` |
| Tessivum | `0.1.0-alpha.23`，Benchmark 源码提交 `d455d99270673be208aecc3182cbf47b9b17989e` |
| tessivum-core Benchmark 源码 | `0.1.6`，提交 `4674aeda870989fede1fc79fb07afbe764d3a1eb` |
| Tessivum runtime 的 Core 依赖 | 提交 `bafb893f182d64b7b464b6cf827676f7ac368168` |
| DeepSeek Harness | `0.1.0-rc.5`，干净上游提交 `47f943859bef60e4160492346772ded9b24f765a` |
| TypeScript Cordis | 固定 Harness 源码树中的 `@deepseek-ai/cordis` 4.0.1 |
| 测量 Profile | Rust release 构建；每个样本使用全新进程与数据目录；每轮交错运行各单元 |

这是 Docker Desktop Linux VM 结果，不是裸机 Linux 结果。直接比较必须使用已检入容器和相同宿主机资源配额。

## 测量等价性

- 两个产品 runtime 接收相同的 manifest Prompt，必须渲染完全匹配的 `CLI_TOOL_ROUND_TRIP` marker，并保留十个 Session；不调用外部模型，也不需要 API Key。
- 两边使用各自原生的离线 replay 适配器：Tessivum 消费 `fixtures/headless/recorded-replay.jsonl`；DeepSeek Harness 使用固定的 `llm-replay` 插件和上游 snapshot。Replay 字节不同，但用户可见的 Prompt、工具结果、marker、Session 数、Browser driver、checkpoint 和清理检查相同。
- Base 在两个 runtime 中均不加载 Benchmark Browser 插件。Compatibility 加载 `tessivum-market`、`dsh-better-sidebar@0.16.1` 和 `dsh-dream-skin@8.30.1`；60 次 Compatibility Browser probe 均在 `window.__DSH_BOOT__` 中验证了三个 entry。
- 全新的 DeepSeek Harness Web Profile 必须先选择工作区才能解锁 Composer。共享 Browser driver 真实执行该 UI 流程。因此“Composer 可用”测量各产品在 HTTP ready 后实际需要的 Browser 初始化，而不是隐藏 API 捷径。
- DeepSeek Harness Headless Profile 缺少 `webServer` 与 `webRuntime`，无法激活两个 Browser-only 社区插件，因此 Compatibility seed 只将它们安装到 Web Profile。Compatibility Headless 数字描述各自支持的 Headless 模式，仅为完整性列出，不用于直接声称插件开销。

## Core 配对 Benchmark

两种 runtime 消费同一个带指纹的 JSON 工作量：1,000 个子 Scope、256 次 Service 查找、256 次 Event emit、16 个 Loader entry 和 32 个 root child。时间和内存单元格为中位数 / p95；吞吐量越高越好。PSS 覆盖完整 runtime 进程。

| 工作量 | tessivum-core 中位数 / p95 | TypeScript Cordis 中位数 / p95 | 中位数结果 |
|---|---:|---:|---:|
| 1,000 次 Scope 创建/销毁 | 0.839 / 0.987 ms | 20.171 / 29.676 ms | **快 24.05×** |
| Service 查找 | 23.184 / 24.187 M ops/s | 1.129 / 1.188 M ops/s | **吞吐 20.53×** |
| Event emit | 10.122 / 12.047 M ops/s | 0.398 / 0.421 M ops/s | **吞吐 25.42×** |
| 加载 16 个 entry | 0.451 / 0.756 ms | 2.184 / 2.588 ms | **快 4.85×** |
| 更新 16 个已加载 entry 中的 1 个 | 20.499 / 33.560 ms | 0.512 / 0.575 ms | **慢 40.05×** |
| 销毁包含 32 个 child 的 root | 0.070 / 0.083 ms | 0.217 / 0.251 ms | **快 3.11×** |
| 1,000 个 Scope 存活时的进程 PSS | 4.64 / 4.65 MiB | 79.61 / 80.25 MiB | **低 17.15×** |
| root dispose 后进程 PSS | 4.69 / 4.70 MiB | 91.05 / 93.18 MiB | **低 19.40×** |
| dispose 后存活注册数 | 0 / 0 | 0 / 0 | 相同；无残留 |

“root dispose 后进程 PSS”是逻辑 root 销毁后的进程内存，不是泄漏计数；“存活注册数”才是语义残留检查。

## 产品对比

所有单元格均为 30 个成功冷进程样本的中位数 / p95。Host 进程树 PSS 不含 Chromium，但包含 Host 的全部后代进程，包括 Tessivum Compatibility 中的 Legacy Node Host。“十 Session 增量”是稳定的边际内存信号；“Composer 可用”从 HTTP ready 后 Browser worker 启动时开始计时。

| 指标 | Tessivum Base | DeepSeek Harness Base | Tessivum Compatibility | DeepSeek Harness Compatibility |
|---|---:|---:|---:|---:|
| Headless replay 完成 | 36.50 / 65.64 ms | 503.02 / 709.20 ms | 69.41 / 114.36 ms | 645.22 / 990.36 ms |
| HTTP ready | 63.23 / 72.71 ms | 368.54 / 612.65 ms | 5,002.07 / 7,645.59 ms | 537.34 / 699.83 ms |
| Chromium Composer 可用 | 1,809.5 / 2,215.0 ms | 2,145.5 / 2,402.0 ms | 1,892.5 / 2,318.0 ms | 2,248.5 / 2,605.0 ms |
| 首个 Prompt 到 marker 往返 | 72.0 / 168.0 ms | 76.0 / 92.0 ms | 79.0 / 146.0 ms | 87.5 / 209.0 ms |
| 从首次提交到十个 Prompt/工具 Session 完成 | 807.5 / 1,055.0 ms | 755.5 / 843.0 ms | 882.5 / 1,064.0 ms | 823.0 / 1,069.0 ms |
| Host 进程树空闲 PSS | 38.33 / 38.34 MiB | 173.17 / 177.33 MiB | 115.03 / 116.26 MiB | 187.66 / 194.14 MiB |
| 单 Session 相对空闲 PSS 增量 | 4.62 / 5.17 MiB | 16.63 / 28.74 MiB | -10.07 / -9.24 MiB | 24.42 / 38.00 MiB |
| 十 Session 相对空闲 PSS 增量 | 10.70 / 11.18 MiB | 26.77 / 37.49 MiB | 12.19 / 14.07 MiB | 29.53 / 42.89 MiB |
| 十 Session PSS 平均增量 | 1.070 / 1.118 MiB | 2.677 / 3.749 MiB | 1.219 / 1.407 MiB | 2.953 / 4.289 MiB |
| 完整 Host 进程树关闭 | 43.75 / 68.81 ms | 46.56 / 71.97 ms | 64.72 / 86.60 ms | 44.88 / 69.48 ms |
| 关闭后存活进程 | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| 成功 Host + Chromium 样本 | 30/30 | 30/30 | 30/30 | 30/30 |

Tessivum Compatibility 的单 Session 增量为负是采样伪影：共享 Host 页面在空闲与单 Session checkpoint 之间被释放。它不表示 Session 使用负内存，应采用十 Session 汇总值。

## 稳定性与暴露的缺陷

- 没有丢弃预热样本。全部 120 个产品样本和两组各 30 个 Core 样本均被保留。
- 第一次 30 样本运行暴露了 Tessivum Compatibility 有 11/30 次冷启动超过 Legacy bridge 的五秒请求截止时间。产品现在为冷 Legacy 插件激活提供与 Web ready 相同的 30 秒上限；重复公开运行通过 30/30。其 `5,002.07` ms HTTP ready 中位数仍作为真实 Compatibility 回归公开，没有被当作预热样本删除。
- 空闲和十 Session 绝对 PSS 稳定，最差 p95/中位数为 1.08。Prompt 延迟波动更大，因此同时报告中位数与 p95。
- 全部 120 次 Browser probe 均无错误，提交了 replay Prompt，看到了精确 marker，完成全部十个 Session，并留下零进程残留。

## 复现

从包含同级 `tessivum-core` 与 `upstream/deepseek-harness`、且处于上述提交的 checkout 执行：

```bash
cd tessivum
SAMPLES=30 ./benchmarks/run-linux-container.sh

python3 scripts/check_benchmark_snapshot.py \
  benchmarks/results/core-paired.json \
  benchmarks/fixtures/phase9-alpha23/core-paired-30.json
python3 scripts/check_benchmark_snapshot.py \
  benchmarks/results/product-comparison.json \
  benchmarks/fixtures/phase9-alpha23/product-30.json
python3 scripts/check_release_facts.py
```

Runner 在测量前构建两个产品及其依赖，先执行 Core 配对工作量，再交错运行四个产品单元。`SAMPLES >= 30` 会启用公开级校验，缺失或失败样本都会被拒绝。

## 证据

- [Core 原始 JSON](../benchmarks/fixtures/phase9-alpha23/core-paired-30.json) — SHA-256 `a2b0b468f85c021e0943aa24fee77b7d26fd46e954a4bcaf24ebcf48e4f151f9`
- [产品原始 JSON](../benchmarks/fixtures/phase9-alpha23/product-30.json) — SHA-256 `a3ba246f394e91175ae4a51ca766afd2a2bc7796d3a5ac1f2f85a6ec0e7d9bf5`
- [Base manifest](../benchmarks/manifests/base.json) — SHA-256 `78725ce072b261de65a98d3ac236cf2876c948b393faa31e4a022e0b485b583b`
- [Compatibility manifest](../benchmarks/manifests/compatibility.json) — SHA-256 `24f55c3326edd9691495cdebe4eb5dd7121a0b029bd1768128bf3387710830db`
- Core 工作量 SHA-256 `82ca294d4fd1042e4d5558b42fef82b7ed03fbdabab29efa14dd3bcac5b6f292`；Core 环境 SHA-256 `a2f35dcb4819b94d4782d2cc84040f9b4f0cb004a79cd7a96d8cf98e5f22030b`
- 产品环境 SHA-256 `76db2dd87235a8cd334eaa0ead8b02347c83da567cb50c5bb42f9020d54ee8a0`
- 产品 driver SHA-256 `bf8ada5e886d20f958b2fb16b166fd30e2b9ec9f5d4efb598d6ff109fab50051`；Browser driver SHA-256 `6a30de03baab8d3789c58dcf1cf5690cb12686fc227eaba5d4ff8bd91cc3cfe0`
- Tessivum 二进制 SHA-256 `54c16d8d350922df7892967168b20fe513e66127362f8f683e4e09325f1cf0a3`；DeepSeek Harness 适配器 SHA-256 `f0ae9e1a63c20239669c8fa7395e385dde04e95aa9f6ec6d7a8a74bd8dcd1faa`
- 上游 DeepSeek Harness CLI SHA-256 `c0226687bb20f45c603ec6fe50f3de16d1c3510c3a803304ec575ef9bc366c62`；replay 插件 SHA-256 `66e714b1307167cc621748571b88f407df646706f7ff8d179ec8748c8de81814`
- Tessivum replay SHA-256 `c06e6e82a2e85e1c44659863429db396620a3c5f75722778a566f76cb228c789`；DeepSeek Harness replay SHA-256 `a8549d7586c1221b90df019a10eb56b81c971568bcb358ab5446a6465b86a0b1`；渲染后的 DeepSeek Harness replay patch SHA-256 `9ee9beca9834030f5420109844b265ce467cd71cbabca84b5858c56ea3abd484`
- DeepSeek Harness Compatibility seed SHA-256 `b76909d19d58cf988e021a378320d286a277a9ff10688c80810852365554b3d2`；Tessivum 已检入兼容源码补丁 SHA-256 `9e914d5998ccb2ca1faf8315a9d9a7235407c7830a8939255cd5838acd149ccd`

## 结论边界

这些结果证明冻结的 Alpha.23 Core 和离线产品契约在所记录 Linux VM 上的表现；不证明生产 LLM 延迟、Token 吞吐、模型质量、多用户饱和能力、裸机 Linux 性能或完整功能对等。Core 比较只覆盖两个 runtime 从同一 fixture 执行的操作；产品比较覆盖相同的可见 replay 契约和插件启动图，而不是相同内部代码路径或字节完全相同的 replay 文件。

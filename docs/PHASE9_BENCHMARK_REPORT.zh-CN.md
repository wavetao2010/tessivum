# Tessivum 0.1.0-alpha.23 Benchmark 报告

[English](PHASE9_BENCHMARK_REPORT.md)

日期：2026-09-03  
状态：公开级运行通过  
样本：每个 runtime 与 case 各 30 次冷进程重复

## 结论

在冻结的 Core 工作量上，tessivum-core 相比 `@deepseek-ai/cordis` 4.0.1，**Scope 创建/销毁快 24.02×**、**Service 查找吞吐为 21.03×**、**Event 吞吐为 26.54×**，峰值进程 PSS **低 17.43×**。两种 runtime 在 root dispose 后的存活注册数都为零。

产品矩阵通过 **30/30 Base** 与 **30/30 Compatibility** 样本。每个样本都创建全新的 Host，使用真实 Chromium 完成 Prompt/工具往返，保留十个 Session，并在销毁后留下零存活进程。Compatibility Profile 加载固定的 `dsh-better-sidebar@0.16.1` 与 `dsh-dream-skin@8.30.1`。

一项 Core 回归不可忽略：tessivum-core 的 `loader_update` **慢 39.49×**。Compatibility 也有明确产品成本：HTTP ready **慢 20.81×**，Host 进程树空闲 PSS 比 Base **高 76.82 MiB**。这些成本没有被标题隐藏。

## 冻结环境

| 项目 | 值 |
|---|---|
| 宿主机 | MacBook Pro `Mac17,2`，Apple M5，10 CPU 核，16 GB RAM |
| 执行环境 | Docker Desktop 4.84.0，Linux arm64 VM |
| 容器基础镜像 | Ubuntu 24.04，`ubuntu@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517` |
| 容器内核 | Linux `6.12.76-linuxkit` |
| 容器配额 | 10 CPU，7.75 GiB（`8,321,515,520` bytes） |
| Rust | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| Bun | `1.4.0` |
| Node.js | `v22.19.0` |
| pnpm | `11.7.0` |
| Tessivum | `0.1.0-alpha.23`，commit `4d2bd09573ff9f9b027cee4c0d14a4784309e164` |
| tessivum-core | `0.1.6`，commit `cedbeb9e1607056845b69e09b825eb7f5be67a69` |
| Tessivum runtime 的 Core 依赖 | commit `bafb893f182d64b7b464b6cf827676f7ac368168` |
| DeepSeek Harness 源码 | commit `47f943859bef60e4160492346772ded9b24f765a` |
| TypeScript Cordis | 固定 Harness 源码树中的 `@deepseek-ai/cordis` 4.0.1 |
| Profile | Rust release 构建；每个样本使用全新进程与数据目录 |

这是 Docker Desktop Linux VM 结果，不是裸机 Linux 结果。直接复现必须使用已检入容器与同样的宿主机资源配额；不同机器上的数字不能被当作同一次实验比较。

## Core 配对 Benchmark

两种 runtime 消费同一个带指纹的 JSON 工作量：1,000 个子 Scope、256 次 Service 查找、256 次 Event emit、16 个 Loader entry 和 32 个 root child。时间指标为中位数 / p95；吞吐指标为越高越好的每秒操作数。PSS 覆盖完整 runtime 进程。

| 工作量 | tessivum-core 中位数 / p95 | TypeScript Cordis 中位数 / p95 | 中位数结果 |
|---|---:|---:|---:|
| 1,000 次 Scope 创建/销毁 | 0.834 / 0.882 ms | 20.021 / 28.363 ms | **快 24.02×** |
| Service 查找 | 23.362 / 24.002 M ops/s | 1.111 / 1.186 M ops/s | **吞吐 21.03×** |
| Event emit | 10.723 / 12.118 M ops/s | 0.404 / 0.432 M ops/s | **吞吐 26.54×** |
| 加载 16 个 entry | 0.445 / 0.556 ms | 2.154 / 2.431 ms | **快 4.85×** |
| 更新 16 个 entry | 21.085 / 33.537 ms | 0.534 / 0.590 ms | **慢 39.49×** |
| 销毁包含 32 个 child 的 root | 0.069 / 0.081 ms | 0.217 / 0.250 ms | **快 3.15×** |
| 峰值进程 PSS | 4.59 / 4.59 MiB | 79.98 / 80.57 MiB | **低 17.43×** |
| root dispose 后进程 PSS | 4.64 / 4.64 MiB | 91.23 / 93.01 MiB | **低 19.66×** |
| dispose 后存活注册数 | 0 / 0 | 0 / 0 | 相同；无残留 |

“root dispose 后进程 PSS”是逻辑 root 销毁后的进程内存，不是泄漏计数；“存活注册数”才是语义残留检查。

## 产品 Benchmark

两个 manifest 使用同一个离线录制 replay，不调用外部模型，也不需要 API Key。Base 只运行 Rust 产品；Compatibility 额外启动固定版本的 Legacy Node Host 和两个 Browser 插件。Browser 总时间包含对可选首次启动对话框的有界探测；Prompt 到 marker 行单独给出提交后的真实本地 replay 往返时间。

| 指标 | Base 中位数 / p95 | Compatibility 中位数 / p95 |
|---|---:|---:|
| Headless replay 完成 | 41.83 / 53.71 ms | 54.87 / 76.42 ms |
| HTTP ready | 60.96 / 89.60 ms | 1,268.77 / 1,446.11 ms |
| Chromium Composer 可用 | 1.821 / 2.081 s | 1.865 / 1.961 s |
| 首个 Prompt 到 marker 往返 | 59 / 64 ms | 74 / 87 ms |
| 从首次提交到十个 Prompt/工具 Session 完成 | 1.089 / 1.161 s | 1.182 / 1.238 s |
| Host 进程树空闲 PSS | 38.27 / 38.27 MiB | 115.09 / 116.26 MiB |
| 单 Session 相对空闲 PSS 增量 | 4.65 / 5.30 MiB | -9.57 / -8.76 MiB |
| 十 Session 相对空闲 PSS 增量 | 10.76 / 11.49 MiB | 12.48 / 13.49 MiB |
| 十 Session PSS 平均增量 | 1.076 / 1.149 MiB | 1.248 / 1.349 MiB |
| 完整进程树关闭 | 43.61 / 68.60 ms | 44.02 / 51.53 ms |
| 关闭后存活进程 | 0 / 0 | 0 / 0 |
| 成功的 Host + Chromium 样本 | 30/30 | 30/30 |

Compatibility 的单 Session 增量为负是采样伪影：共享 Host 页面在空闲与单 Session checkpoint 之间被释放。它不表示 Session 使用负内存。稳定的边际信号应采用十 Session 汇总值。

## 稳定性与预热结论

- 没有丢弃预热样本。每个测量样本都启动全新进程并使用全新数据目录。
- 第一个 Compatibility headless 样本为 151.80 ms，是 54.87 ms 中位数的 2.77×。这暴露了新进程之外的文件系统/模块缓存预热，因此报告使用中位数与 p95，而不是最好成绩。
- Core 的 p95/中位数在 tessivum-core 中最高为 1.59，在 TypeScript Cordis 中最高为 1.42。Rust 最大波动来自已经明确披露的 `loader_update` 回归。
- 产品空闲及十 Session 绝对 PSS 稳定，p95/中位数最高为 1.02。HTTP ready 的 p95/中位数在 Base 中为 1.47，在 Compatibility 中为 1.14。
- Headless 进程完成时间短于 100 ms PSS 采样周期，其峰值 PSS 的 p95/中位数达到 2.31。因此原始证据保留该指标，但公开性能结论不引用它。
- 全部 60 次 Browser probe 均无 page error，提交了 replay Prompt，看到了精确工具 marker，完成全部十个 Session，并留下零 Host、Browser 或子进程残留。

## 复现

从包含同级 `tessivum-core` 与 `upstream/deepseek-harness`、且处于上述 commit 的 checkout 执行：

```bash
cd tessivum
SAMPLES=30 ./benchmarks/run-linux-container.sh

python3 scripts/check_benchmark_snapshot.py \
  benchmarks/results/core-paired.json \
  benchmarks/fixtures/phase9-alpha23/core-paired-30.json
python3 scripts/check_benchmark_snapshot.py \
  benchmarks/results/product.json \
  benchmarks/fixtures/phase9-alpha23/product-30.json
python3 scripts/check_release_facts.py
```

Runner 在固定镜像内构建 release 二进制，先执行 Core 配对测试，再交错执行 Base 与 Compatibility 产品 case。`SAMPLES >= 30` 会开启公开级校验；样本不足或任意 case 失败都会被拒绝。

## 证据

- [Core 原始 JSON](../benchmarks/fixtures/phase9-alpha23/core-paired-30.json) — SHA-256 `325f9b16352263f17d0b04b629cc22a1c6ec73adbde0eacb6882caf51485d69c`
- [产品原始 JSON](../benchmarks/fixtures/phase9-alpha23/product-30.json) — SHA-256 `6ae6f1b7a897ff7395e63121926a7e61378a251df3a411a37d48e202eae0cf80`
- [Base manifest](../benchmarks/manifests/base.json) — SHA-256 `0dd1b1c72f1ed8ad7c984a7f818c4cb211b6a2101600a7a75631e59ac733ad54`
- [Compatibility manifest](../benchmarks/manifests/compatibility.json) — SHA-256 `23c1831326d0fba09ca6aa34c8ae7cce74247f0fc6c811713fb70ffc474721d4`
- Core workload SHA-256 `82ca294d4fd1042e4d5558b42fef82b7ed03fbdabab29efa14dd3bcac5b6f292`
- 产品工作量 SHA-256 `e829d759cbbfca4d4adf907fb80d7e8e592a3f456636c23a513669428252e557`

## 结论边界

这些数字只证明 Alpha.23 冻结工作量在已记录 Linux 环境中的表现；不证明生产 LLM 延迟、Token 吞吐、质量、多用户饱和能力、裸机 Linux 表现或完整 DeepSeek Harness 产品对比。TypeScript 横向比较仅覆盖两个 runtime 从同一 fixture 执行的 Core 操作。上游产品仍为 `unmeasured`，因为它尚不能通过等价 driver 消费完全相同的产品 replay。

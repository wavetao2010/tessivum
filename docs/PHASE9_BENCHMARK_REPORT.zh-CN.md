# Tessivum 0.1.0-alpha.23 Benchmark 报告

[English](PHASE9_BENCHMARK_REPORT.md)

日期：2026-09-03  
状态：公开级运行通过  
样本：每个 runtime 与 case 各 30 次冷进程重复

## 结论

在冻结的 Core 工作量上，tessivum-core 相比 `@deepseek-ai/cordis` 4.0.1，**Scope 创建/销毁快 23.64×**、**Service 查找吞吐为 20.73×**、**Event 吞吐为 26.92×**，1,000 个 Scope 存活时的进程 PSS **低 17.43×**。两种 runtime 在 root dispose 后的存活注册数都为零。

产品矩阵通过 **30/30 Base** 与 **30/30 Compatibility** 样本。每个样本都创建全新的 Host，使用真实 Chromium 完成 Prompt/工具往返，保留十个 Session，并在销毁后留下零存活进程。Compatibility Profile 加载固定的 `dsh-better-sidebar@0.16.1` 与 `dsh-dream-skin@8.30.1`。

一项 Core 回归不可忽略：tessivum-core 的 `loader_update` **慢 37.03×**。Compatibility 也有明确产品成本：HTTP ready **慢 18.82×**，Host 进程树空闲 PSS 比 Base **高 76.51 MiB**。这些成本没有被标题隐藏。

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
| Tessivum | `0.1.0-alpha.23`，commit `d21f0a423076acf50334af5056943205d677ea1c` |
| tessivum-core Benchmark 源码 | `0.1.6`，commit `4674aeda870989fede1fc79fb07afbe764d3a1eb` |
| Tessivum runtime 的 Core 依赖 | commit `bafb893f182d64b7b464b6cf827676f7ac368168` |
| DeepSeek Harness 源码 | commit `47f943859bef60e4160492346772ded9b24f765a` |
| TypeScript Cordis | 固定 Harness 源码树中的 `@deepseek-ai/cordis` 4.0.1 |
| Profile | Rust release 构建；每个样本使用全新进程与数据目录 |

这是 Docker Desktop Linux VM 结果，不是裸机 Linux 结果。直接复现必须使用已检入容器与同样的宿主机资源配额；不同机器上的数字不能被当作同一次实验比较。

## Core 配对 Benchmark

两种 runtime 消费同一个带指纹的 JSON 工作量：1,000 个子 Scope、256 次 Service 查找、256 次 Event emit、16 个 Loader entry 和 32 个 root child。时间指标为中位数 / p95；吞吐指标为越高越好的每秒操作数。PSS 覆盖完整 runtime 进程。

| 工作量 | tessivum-core 中位数 / p95 | TypeScript Cordis 中位数 / p95 | 中位数结果 |
|---|---:|---:|---:|
| 1,000 次 Scope 创建/销毁 | 0.877 / 0.995 ms | 20.727 / 25.246 ms | **快 23.64×** |
| Service 查找 | 23.011 / 23.814 M ops/s | 1.110 / 1.181 M ops/s | **吞吐 20.73×** |
| Event emit | 10.667 / 12.047 M ops/s | 0.396 / 0.428 M ops/s | **吞吐 26.92×** |
| 加载 16 个 entry | 0.448 / 0.542 ms | 2.156 / 2.328 ms | **快 4.81×** |
| 更新 16 个已加载 entry 中的 1 个 | 19.854 / 30.167 ms | 0.536 / 0.592 ms | **慢 37.03×** |
| 销毁包含 32 个 child 的 root | 0.069 / 0.079 ms | 0.218 / 0.253 ms | **快 3.15×** |
| 1,000 个 Scope 存活时的进程 PSS | 4.59 / 4.59 MiB | 80.03 / 81.13 MiB | **低 17.43×** |
| root dispose 后进程 PSS | 4.64 / 4.65 MiB | 91.20 / 92.77 MiB | **低 19.65×** |
| dispose 后存活注册数 | 0 / 0 | 0 / 0 | 相同；无残留 |

“root dispose 后进程 PSS”是逻辑 root 销毁后的进程内存，不是泄漏计数；“存活注册数”才是语义残留检查。

## Tessivum Base 与 Compatibility Profile 成本对比

本节不是 Tessivum 与 DeepSeek Harness 的产品对比；两列均运行同一个 Tessivum `0.1.0-alpha.23`。两个 manifest 使用同一个离线录制 replay，不调用外部模型，也不需要 API Key。Base 只运行 Rust 产品；Compatibility 额外启动固定版本的 Legacy Node Host，并加载第一方 `tessivum-market` 以及固定版本的 `dsh-better-sidebar@0.16.1`、`dsh-dream-skin@8.30.1` Browser 插件。Browser 总时间包含对可选首次启动对话框的有界探测；Prompt 到 marker 行单独给出提交后的真实本地 replay 往返时间。

| 指标 | Tessivum Base 中位数 / p95 | Tessivum Compatibility 中位数 / p95 |
|---|---:|---:|
| Headless replay 完成 | 44.75 / 54.24 ms | 65.21 / 90.46 ms |
| HTTP ready | 71.06 / 89.97 ms | 1,337.41 / 1,686.20 ms |
| Chromium Composer 可用 | 1.903 / 2.063 s | 1.911 / 2.016 s |
| 首个 Prompt 到 marker 往返 | 65.5 / 81.0 ms | 76.0 / 113.0 ms |
| 从首次提交到十个 Prompt/工具 Session 完成 | 0.788 / 0.848 s | 0.869 / 0.950 s |
| Host 进程树空闲 PSS | 38.27 / 38.28 MiB | 114.78 / 115.56 MiB |
| 单 Session 相对空闲 PSS 增量 | 4.65 / 5.48 MiB | -9.66 / -8.48 MiB |
| 十 Session 相对空闲 PSS 增量 | 10.74 / 11.37 MiB | 12.55 / 13.16 MiB |
| 十 Session PSS 平均增量 | 1.074 / 1.137 MiB | 1.255 / 1.316 MiB |
| 完整进程树关闭 | 44.67 / 66.37 ms | 65.86 / 71.41 ms |
| 关闭后存活进程 | 0 / 0 | 0 / 0 |
| 成功的 Host + Chromium 样本 | 30/30 | 30/30 |

Compatibility 的单 Session 增量为负是采样伪影：共享 Host 页面在空闲与单 Session checkpoint 之间被释放。它不表示 Session 使用负内存。稳定的边际信号应采用十 Session 汇总值。

## 稳定性与预热结论

- 没有丢弃预热样本。每个测量样本都启动全新进程并使用全新数据目录。
- 第一个 Compatibility headless 样本为 147.14 ms，是 65.21 ms 中位数的 2.26×。这暴露了新进程之外的文件系统/模块缓存预热，因此报告使用中位数与 p95，而不是最好成绩。
- Core 的 p95/中位数在 tessivum-core 中最高为 1.52，在 TypeScript Cordis 中最高为 1.22。Rust 最大波动来自已经明确披露的 `loader_update` 回归。
- 产品空闲及十 Session 绝对 PSS 稳定，p95/中位数最高为 1.01。HTTP ready 的 p95/中位数在 Base 中为 1.27，在 Compatibility 中为 1.26。
- Headless 进程完成时间短于 100 ms PSS 采样周期，其峰值 PSS 的 p95/中位数达到 1.44。因此原始证据保留该指标，但公开性能结论不引用它。
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

- [Core 原始 JSON](../benchmarks/fixtures/phase9-alpha23/core-paired-30.json) — SHA-256 `4ac31357ab07f5280e57ec510d970cbcd8653e9ed62e9c67daee2f2f3a5263b3`
- [产品原始 JSON](../benchmarks/fixtures/phase9-alpha23/product-30.json) — SHA-256 `89f4bfb7169d6074e1d846643041bfc19ad8d8a0579a60a4dab86134684bf52c`
- [Base manifest](../benchmarks/manifests/base.json) — SHA-256 `867692853beccc6735533c547b8892179bd253b17f4aca271ad0be393b5b3a90`
- [Compatibility manifest](../benchmarks/manifests/compatibility.json) — SHA-256 `852952f6e6b206c241259ba1c57beb4e2c0e423d5d868b2e2fede4880b42b947`
- Core 工作量 SHA-256 `82ca294d4fd1042e4d5558b42fef82b7ed03fbdabab29efa14dd3bcac5b6f292`；Core 环境 SHA-256 `c2ddb47aadef65eac05b306ec4f9f1c0c5e266d56b8e9b77a98579a89c2d5b4a`
- 产品环境 SHA-256 `76db2dd87235a8cd334eaa0ead8b02347c83da567cb50c5bb42f9020d54ee8a0`；replay SHA-256 `c06e6e82a2e85e1c44659863429db396620a3c5f75722778a566f76cb228c789`
- 产品 driver SHA-256 `c35c0b4f9944e53ab66b73ffb6026c1d1ef8510b6ba57a59de63bb63fa4d72fd`；Browser driver SHA-256 `59b3d0498848b7b9cec29058557945226c5619bf5c834e1df5fcc14dbfe84ae1`
- DeepSeek Harness 兼容补丁 SHA-256 `9e914d5998ccb2ca1faf8315a9d9a7235407c7830a8939255cd5838acd149ccd`

## 结论边界

这些数字只证明 Alpha.23 冻结工作量在已记录 Linux 环境中的表现；不证明生产 LLM 延迟、Token 吞吐、质量、多用户饱和能力、裸机 Linux 表现或完整 DeepSeek Harness 产品对比。TypeScript 横向比较仅覆盖两个 runtime 从同一 fixture 执行的 Core 操作。上游产品仍为 `unmeasured`，因为它尚不能通过等价 driver 消费完全相同的产品 replay。

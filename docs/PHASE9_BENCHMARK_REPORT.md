# Tessivum 0.1.0-alpha.23 benchmark report

[简体中文](PHASE9_BENCHMARK_REPORT.zh-CN.md)

Date: 2026-09-04  
Status: publication run passed  
Samples: 30 process-cold repetitions per runtime and manifest

## Result

On the frozen Core workload, tessivum-core was **24.05× faster for scope create/dispose**, delivered **20.53× service-lookup throughput** and **25.42× event throughput**, and used **17.15× less process PSS while 1,000 scopes were live** than `@deepseek-ai/cordis` 4.0.1. Both runtimes reported zero live registrations after root disposal.

The product comparison passed **30/30 samples in all four runtime/manifest cells**: Tessivum Base, DeepSeek Harness Base, Tessivum Compatibility, and DeepSeek Harness Compatibility. All 120 samples used a fresh Host and data directory, drove the real Web UI with Chromium, completed the same visible prompt/tool-marker contract across ten resident Sessions, and left zero Host, Browser, or child-process residue.

In Base, Tessivum completed headless replay **13.78× faster**, reached HTTP readiness **5.83× faster**, used **4.52× less idle Host-tree PSS**, and used **2.50× less ten-Session incremental PSS** than DeepSeek Harness. In Compatibility, Tessivum still used **1.63× less idle PSS** and **2.42× less ten-Session incremental PSS**, but its Legacy Node plugin bridge made HTTP readiness **9.31× slower** than DeepSeek Harness (`5,002.07` vs `537.34` ms). DeepSeek Harness also completed ten Sessions about 7% faster in both manifests. The Core `loader_update` path remains a material **40.05× regression** in tessivum-core.

## Frozen environment

| Item | Value |
|---|---|
| Host | MacBook Pro `Mac17,2`, Apple M5, 10 CPU cores, 16 GB RAM |
| Execution environment | Docker Desktop Linux arm64 VM |
| Container base | Ubuntu 24.04, `ubuntu@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517` |
| Container kernel | Linux `6.12.76-linuxkit` |
| Container allocation | 10 CPUs, 7.75 GiB (`8,321,515,520` bytes) |
| Rust | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| Bun | `1.4.0` |
| Node.js | `v22.19.0` |
| pnpm | `11.7.0` |
| Tessivum | `0.1.0-alpha.23`, benchmark source commit `d455d99270673be208aecc3182cbf47b9b17989e` |
| tessivum-core benchmark source | `0.1.6`, commit `4674aeda870989fede1fc79fb07afbe764d3a1eb` |
| Tessivum runtime Core dependency | commit `bafb893f182d64b7b464b6cf827676f7ac368168` |
| DeepSeek Harness | `0.1.0-rc.5`, clean upstream commit `47f943859bef60e4160492346772ded9b24f765a` |
| TypeScript Cordis | `@deepseek-ai/cordis` 4.0.1 from the pinned Harness tree |
| Measurement profile | Rust release builds; fresh process and data directory per sample; cells interleaved within each repetition |

This is a Docker Desktop Linux VM result, not bare-metal Linux. Direct comparisons require the checked-in container and the same host allocation.

## Measurement equivalence

- Both product runtimes receive the same manifest prompt, must render the exact `CLI_TOOL_ROUND_TRIP` marker, and retain ten Sessions. No external model or API key is used.
- Each runtime uses its native offline replay adapter: Tessivum consumes `fixtures/headless/recorded-replay.jsonl`; DeepSeek Harness consumes its pinned `llm-replay` plugin and upstream snapshot. The replay bytes differ, but the user-visible prompt, tool result, marker, Session count, Browser driver, checkpoints, and cleanup checks are identical.
- Base loads no benchmark Browser plugin in either runtime. Compatibility loads `tessivum-market`, `dsh-better-sidebar@0.16.1`, and `dsh-dream-skin@8.30.1`; every one of the 60 Compatibility Browser probes verified all three entries in `window.__DSH_BOOT__`.
- A fresh DeepSeek Harness Web profile requires selecting a workspace before the composer unlocks. The shared Browser driver performs that real UI flow. `composer enabled` therefore measures each product's actual post-HTTP Browser initialization, not a hidden API shortcut.
- DeepSeek Harness cannot activate the two Browser-only community plugins in a headless profile because `webServer` and `webRuntime` are absent. Its Compatibility seed therefore installs them only in the Web profile. Compatibility headless numbers describe each supported headless mode and are shown for completeness, but are not used as a direct plugin-overhead claim.

## Core paired benchmark

Both runtimes consume the same fingerprinted JSON workload: 1,000 child scopes, 256 service lookups, 256 event emits, 16 loader entries, and 32 root children. Time and memory cells show median / p95; throughput is higher-is-better. PSS covers the complete runtime process.

| Workload | tessivum-core median / p95 | TypeScript Cordis median / p95 | Median result |
|---|---:|---:|---:|
| 1,000 scope create/dispose cycles | 0.839 / 0.987 ms | 20.171 / 29.676 ms | **24.05× faster** |
| Service lookup | 23.184 / 24.187 M ops/s | 1.129 / 1.188 M ops/s | **20.53× throughput** |
| Event emit | 10.122 / 12.047 M ops/s | 0.398 / 0.421 M ops/s | **25.42× throughput** |
| Load 16 entries | 0.451 / 0.756 ms | 2.184 / 2.588 ms | **4.85× faster** |
| Update 1 of 16 loaded entries | 20.499 / 33.560 ms | 0.512 / 0.575 ms | **40.05× slower** |
| Dispose root with 32 children | 0.070 / 0.083 ms | 0.217 / 0.251 ms | **3.11× faster** |
| Process PSS with 1,000 live scopes | 4.64 / 4.65 MiB | 79.61 / 80.25 MiB | **17.15× lower** |
| Process PSS after root disposal | 4.69 / 4.70 MiB | 91.05 / 93.18 MiB | **19.40× lower** |
| Live registrations after disposal | 0 / 0 | 0 / 0 | equal; no residue |

Post-disposal PSS is process memory after logical root disposal, not a leak count. The registration-residue row is the semantic leak check.

## Product comparison

All cells show median / p95 from 30 successful process-cold samples. Host-tree PSS excludes Chromium but includes every Host descendant, including Tessivum's Legacy Node host in Compatibility. `Ten-Session delta` is the stable marginal memory signal; `composer enabled` starts when the Browser worker launches after HTTP readiness.

| Metric | Tessivum Base | DeepSeek Harness Base | Tessivum Compatibility | DeepSeek Harness Compatibility |
|---|---:|---:|---:|---:|
| Headless replay completion | 36.50 / 65.64 ms | 503.02 / 709.20 ms | 69.41 / 114.36 ms | 645.22 / 990.36 ms |
| HTTP ready | 63.23 / 72.71 ms | 368.54 / 612.65 ms | 5,002.07 / 7,645.59 ms | 537.34 / 699.83 ms |
| Chromium composer enabled | 1,809.5 / 2,215.0 ms | 2,145.5 / 2,402.0 ms | 1,892.5 / 2,318.0 ms | 2,248.5 / 2,605.0 ms |
| First prompt-to-marker round trip | 72.0 / 168.0 ms | 76.0 / 92.0 ms | 79.0 / 146.0 ms | 87.5 / 209.0 ms |
| Ten prompt/tool Sessions from first submit | 807.5 / 1,055.0 ms | 755.5 / 843.0 ms | 882.5 / 1,064.0 ms | 823.0 / 1,069.0 ms |
| Idle Host-tree PSS | 38.33 / 38.34 MiB | 173.17 / 177.33 MiB | 115.03 / 116.26 MiB | 187.66 / 194.14 MiB |
| One-Session PSS delta from idle | 4.62 / 5.17 MiB | 16.63 / 28.74 MiB | -10.07 / -9.24 MiB | 24.42 / 38.00 MiB |
| Ten-Session PSS delta from idle | 10.70 / 11.18 MiB | 26.77 / 37.49 MiB | 12.19 / 14.07 MiB | 29.53 / 42.89 MiB |
| Ten-Session PSS delta per Session | 1.070 / 1.118 MiB | 2.677 / 3.749 MiB | 1.219 / 1.407 MiB | 2.953 / 4.289 MiB |
| Full Host-tree shutdown | 43.75 / 68.81 ms | 46.56 / 71.97 ms | 64.72 / 86.60 ms | 44.88 / 69.48 ms |
| Live processes after shutdown | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
| Successful Host + Chromium samples | 30/30 | 30/30 | 30/30 | 30/30 |

The negative Tessivum Compatibility one-Session delta is a sampling artifact: shared Host pages were released between the idle and one-Session checkpoints. It is not negative Session memory; use the ten-Session aggregate.

## Stability and defects exposed

- No warm-up sample was discarded. All 120 product samples and both 30-sample Core series are retained.
- The first 30-sample attempt exposed 11/30 Tessivum Compatibility cold starts exceeding the Legacy bridge's five-second request deadline. The product now gives cold Legacy plugin activation the same 30-second ceiling as Web readiness; the repeated publication run passed 30/30. Its `5,002.07` ms median HTTP readiness remains reported as a real Compatibility regression, not removed as warm-up.
- Absolute idle and ten-Session PSS were stable: the worst p95/median ratio was 1.08. Prompt latency was noisier, so both median and p95 are shown.
- All 120 Browser probes reported zero errors, submitted the replay prompt, observed the exact marker, completed all ten Sessions, and left zero process residue.

## Reproduce

From a checkout containing sibling `tessivum-core` and `upstream/deepseek-harness` repositories at the commits above:

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

The runner builds both products and their dependencies before measurement, executes the paired Core workload, and then interleaves the four product cells. `SAMPLES >= 30` enables publication validation, which rejects missing or failed samples.

## Evidence

- [Core raw JSON](../benchmarks/fixtures/phase9-alpha23/core-paired-30.json) — SHA-256 `a2b0b468f85c021e0943aa24fee77b7d26fd46e954a4bcaf24ebcf48e4f151f9`
- [Product raw JSON](../benchmarks/fixtures/phase9-alpha23/product-30.json) — SHA-256 `a3ba246f394e91175ae4a51ca766afd2a2bc7796d3a5ac1f2f85a6ec0e7d9bf5`
- [Base manifest](../benchmarks/manifests/base.json) — SHA-256 `78725ce072b261de65a98d3ac236cf2876c948b393faa31e4a022e0b485b583b`
- [Compatibility manifest](../benchmarks/manifests/compatibility.json) — SHA-256 `24f55c3326edd9691495cdebe4eb5dd7121a0b029bd1768128bf3387710830db`
- Core workload SHA-256 `82ca294d4fd1042e4d5558b42fef82b7ed03fbdabab29efa14dd3bcac5b6f292`; Core environment SHA-256 `a2f35dcb4819b94d4782d2cc84040f9b4f0cb004a79cd7a96d8cf98e5f22030b`
- Product environment SHA-256 `76db2dd87235a8cd334eaa0ead8b02347c83da567cb50c5bb42f9020d54ee8a0`
- Product driver SHA-256 `bf8ada5e886d20f958b2fb16b166fd30e2b9ec9f5d4efb598d6ff109fab50051`; Browser driver SHA-256 `6a30de03baab8d3789c58dcf1cf5690cb12686fc227eaba5d4ff8bd91cc3cfe0`
- Tessivum binary SHA-256 `54c16d8d350922df7892967168b20fe513e66127362f8f683e4e09325f1cf0a3`; DeepSeek Harness adapter SHA-256 `f0ae9e1a63c20239669c8fa7395e385dde04e95aa9f6ec6d7a8a74bd8dcd1faa`
- Upstream DeepSeek Harness CLI SHA-256 `c0226687bb20f45c603ec6fe50f3de16d1c3510c3a803304ec575ef9bc366c62`; replay plugin SHA-256 `66e714b1307167cc621748571b88f407df646706f7ff8d179ec8748c8de81814`
- Tessivum replay SHA-256 `c06e6e82a2e85e1c44659863429db396620a3c5f75722778a566f76cb228c789`; DeepSeek Harness replay SHA-256 `a8549d7586c1221b90df019a10eb56b81c971568bcb358ab5446a6465b86a0b1`; rendered DeepSeek Harness replay patch SHA-256 `9ee9beca9834030f5420109844b265ce467cd71cbabca84b5858c56ea3abd484`
- DeepSeek Harness Compatibility seed SHA-256 `b76909d19d58cf988e021a378320d286a277a9ff10688c80810852365554b3d2`; Tessivum's checked compatibility source patch SHA-256 `9e914d5998ccb2ca1faf8315a9d9a7235407c7830a8939255cd5838acd149ccd`

## Claim boundary

These results establish the frozen Alpha.23 Core and offline product contracts on the recorded Linux VM. They do not establish production LLM latency, token throughput, model quality, multi-user saturation, bare-metal Linux performance, or full feature parity. The Core comparison covers only operations both runtimes execute from the same fixture. The product comparison covers the same visible replay contract and plugin boot graph, not identical internal code paths or byte-identical replay files.

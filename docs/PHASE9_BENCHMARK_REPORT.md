# Tessivum 0.1.0-alpha.23 benchmark report

[简体中文](PHASE9_BENCHMARK_REPORT.zh-CN.md)

Date: 2026-09-03  
Status: publication run passed  
Samples: 30 process-cold repetitions per runtime and case

## Result

On the frozen Core workload, tessivum-core was **24.02× faster for scope create/dispose**, delivered **21.03× service-lookup throughput** and **26.54× event throughput**, and used **17.43× less peak process PSS** than `@deepseek-ai/cordis` 4.0.1. Both runtimes reported zero live registrations after root disposal.

The product matrix passed **30/30 Base** and **30/30 Compatibility** samples. Every sample started a fresh Host, exercised a real Chromium prompt/tool round trip, retained ten Sessions, and left zero live processes after disposal. The Compatibility profile loaded the pinned `dsh-better-sidebar@0.16.1` and `dsh-dream-skin@8.30.1` packages.

One Core regression is material: `loader_update` took **39.49× longer** in tessivum-core. Compatibility also has a visible product cost: HTTP readiness was **20.81× slower** and idle Host-tree PSS was **76.82 MiB higher** than Base. These costs are not hidden by the headline.

## Frozen environment

| Item | Value |
|---|---|
| Host | MacBook Pro `Mac17,2`, Apple M5, 10 CPU cores, 16 GB RAM |
| Execution environment | Docker Desktop 4.84.0, Linux arm64 VM |
| Container base | Ubuntu 24.04, `ubuntu@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517` |
| Container kernel | Linux `6.12.76-linuxkit` |
| Container allocation | 10 CPUs, 7.75 GiB (`8,321,515,520` bytes) |
| Rust | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| Bun | `1.4.0` |
| Node.js | `v22.19.0` |
| pnpm | `11.7.0` |
| Tessivum | `0.1.0-alpha.23`, commit `4d2bd09573ff9f9b027cee4c0d14a4784309e164` |
| tessivum-core | `0.1.6`, commit `cedbeb9e1607056845b69e09b825eb7f5be67a69` |
| Tessivum runtime Core dependency | commit `bafb893f182d64b7b464b6cf827676f7ac368168` |
| DeepSeek Harness source | commit `47f943859bef60e4160492346772ded9b24f765a` |
| TypeScript Cordis | `@deepseek-ai/cordis` 4.0.1 from the pinned Harness tree |
| Profile | Rust release builds; fresh process and data directory per sample |

This is a Docker Desktop Linux VM result, not a bare-metal Linux result. Use the checked-in container and the same host allocation for direct reproduction; do not compare numbers from a different machine as though they were the same experiment.

## Core paired benchmark

Each runtime consumes the same fingerprinted JSON workload: 1,000 child scopes, 256 service lookups, 256 event emits, 16 loader entries and 32 root children. Time metrics show median / p95; throughput metrics show higher-is-better operations per second. PSS covers the complete runtime process.

| Workload | tessivum-core median / p95 | TypeScript Cordis median / p95 | Median result |
|---|---:|---:|---:|
| 1,000 scope create/dispose cycles | 0.834 / 0.882 ms | 20.021 / 28.363 ms | **24.02× faster** |
| Service lookup | 23.362 / 24.002 M ops/s | 1.111 / 1.186 M ops/s | **21.03× throughput** |
| Event emit | 10.723 / 12.118 M ops/s | 0.404 / 0.432 M ops/s | **26.54× throughput** |
| Load 16 entries | 0.445 / 0.556 ms | 2.154 / 2.431 ms | **4.85× faster** |
| Update 16 entries | 21.085 / 33.537 ms | 0.534 / 0.590 ms | **39.49× slower** |
| Dispose root with 32 children | 0.069 / 0.081 ms | 0.217 / 0.250 ms | **3.15× faster** |
| Peak process PSS | 4.59 / 4.59 MiB | 79.98 / 80.57 MiB | **17.43× lower** |
| Process PSS after root disposal | 4.64 / 4.64 MiB | 91.23 / 93.01 MiB | **19.66× lower** |
| Live registrations after disposal | 0 / 0 | 0 / 0 | equal; no residue |

The post-disposal PSS row is process memory after logical root disposal, not a leak count. The registration-residue row is the semantic leak check.

## Product benchmark

Both manifests use the same offline recorded replay and no external model or API key. Base runs the Rust product alone. Compatibility additionally boots the pinned Legacy Node Host and the two fixed Browser plugins. Browser timings include a bounded probe for optional onboarding dialogs; the prompt-to-marker rows isolate the actual local replay round trip after submission.

| Metric | Base median / p95 | Compatibility median / p95 |
|---|---:|---:|
| Headless replay completion | 41.83 / 53.71 ms | 54.87 / 76.42 ms |
| HTTP ready | 60.96 / 89.60 ms | 1,268.77 / 1,446.11 ms |
| Chromium composer enabled | 1.821 / 2.081 s | 1.865 / 1.961 s |
| First prompt-to-marker round trip | 59 / 64 ms | 74 / 87 ms |
| Ten prompt/tool Sessions from first submit | 1.089 / 1.161 s | 1.182 / 1.238 s |
| Idle Host-tree PSS | 38.27 / 38.27 MiB | 115.09 / 116.26 MiB |
| One-Session PSS delta from idle | 4.65 / 5.30 MiB | -9.57 / -8.76 MiB |
| Ten-Session PSS delta from idle | 10.76 / 11.49 MiB | 12.48 / 13.49 MiB |
| Ten-Session PSS delta per Session | 1.076 / 1.149 MiB | 1.248 / 1.349 MiB |
| Full process-tree shutdown | 43.61 / 68.60 ms | 44.02 / 51.53 ms |
| Live processes after shutdown | 0 / 0 | 0 / 0 |
| Successful Host + Chromium samples | 30/30 | 30/30 |

The negative one-Session Compatibility delta is a sampling artifact: shared Host pages were released between the idle and one-Session checkpoints. It is not negative Session memory. Use the ten-Session aggregate for the stable marginal signal.

## Stability and warm-up findings

- No warm-up sample was discarded. Every measured sample launches a fresh process and uses a fresh data directory.
- The first Compatibility headless sample was 151.80 ms, 2.77× its 54.87 ms median. This exposes filesystem/module-cache warming outside the fresh process; median and p95 are reported instead of a best run.
- Core p95/median was at most 1.59 for tessivum-core and 1.42 for TypeScript Cordis. The largest Rust spread was `loader_update`, already reported as the principal regression.
- Product idle and ten-Session absolute PSS were stable: p95/median was at most 1.02. HTTP-ready p95/median was 1.47 for Base and 1.14 for Compatibility.
- Headless processes complete faster than the 100 ms PSS sampling interval. Their peak-PSS p95/median reached 2.31, so headless PSS is retained in raw evidence but excluded from performance claims.
- All 60 Browser probes reported zero page errors, submitted the replay prompt, observed the exact tool marker, completed all ten Sessions, and left zero Host, Browser or child-process residue.

## Reproduce

From a checkout containing sibling `tessivum-core` and `upstream/deepseek-harness` repositories at the commits above:

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

The runner builds release binaries inside the pinned image, executes the Core pair, then interleaves Base and Compatibility product cases. `SAMPLES >= 30` enables publication validation, which rejects missing samples or any failed case.

## Evidence

- [Core raw JSON](../benchmarks/fixtures/phase9-alpha23/core-paired-30.json) — SHA-256 `325f9b16352263f17d0b04b629cc22a1c6ec73adbde0eacb6882caf51485d69c`
- [Product raw JSON](../benchmarks/fixtures/phase9-alpha23/product-30.json) — SHA-256 `6ae6f1b7a897ff7395e63121926a7e61378a251df3a411a37d48e202eae0cf80`
- [Base manifest](../benchmarks/manifests/base.json) — SHA-256 `0dd1b1c72f1ed8ad7c984a7f818c4cb211b6a2101600a7a75631e59ac733ad54`
- [Compatibility manifest](../benchmarks/manifests/compatibility.json) — SHA-256 `23c1831326d0fba09ca6aa34c8ae7cce74247f0fc6c811713fb70ffc474721d4`
- Core workload SHA-256 `82ca294d4fd1042e4d5558b42fef82b7ed03fbdabab29efa14dd3bcac5b6f292`
- Product workload SHA-256 `e829d759cbbfca4d4adf907fb80d7e8e592a3f456636c23a513669428252e557`

## Claim boundary

These numbers establish the frozen Alpha.23 workloads on the recorded Linux environment. They do not establish production LLM latency, token throughput, quality, multi-user saturation, bare-metal Linux performance, or a full DeepSeek Harness product comparison. The TypeScript comparison is limited to the Core operations that both runtimes execute from the same fixture. The upstream product remains `unmeasured` because it does not consume the identical product replay through an equivalent driver.

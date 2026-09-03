# Tessivum 0.1.0-alpha.23 benchmark report

[简体中文](PHASE9_BENCHMARK_REPORT.zh-CN.md)

Date: 2026-09-03  
Status: publication run passed  
Samples: 30 process-cold repetitions per runtime and case

## Result

On the frozen Core workload, tessivum-core was **23.64× faster for scope create/dispose**, delivered **20.73× service-lookup throughput** and **26.92× event throughput**, and used **17.43× less peak process PSS** than `@deepseek-ai/cordis` 4.0.1. Both runtimes reported zero live registrations after root disposal.

The product matrix passed **30/30 Base** and **30/30 Compatibility** samples. Every sample started a fresh Host, exercised a real Chromium prompt/tool round trip, retained ten Sessions, and left zero live processes after disposal. The Compatibility profile loaded the pinned `dsh-better-sidebar@0.16.1` and `dsh-dream-skin@8.30.1` packages.

One Core regression is material: `loader_update` took **37.03× longer** in tessivum-core. Compatibility also has a visible product cost: HTTP readiness was **18.82× slower** and idle Host-tree PSS was **76.51 MiB higher** than Base. These costs are not hidden by the headline.

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
| Tessivum | `0.1.0-alpha.23`, commit `d21f0a423076acf50334af5056943205d677ea1c` |
| tessivum-core benchmark source | `0.1.6`, commit `4674aeda870989fede1fc79fb07afbe764d3a1eb` |
| Tessivum runtime Core dependency | commit `bafb893f182d64b7b464b6cf827676f7ac368168` |
| DeepSeek Harness source | commit `47f943859bef60e4160492346772ded9b24f765a` |
| TypeScript Cordis | `@deepseek-ai/cordis` 4.0.1 from the pinned Harness tree |
| Profile | Rust release builds; fresh process and data directory per sample |

This is a Docker Desktop Linux VM result, not a bare-metal Linux result. Use the checked-in container and the same host allocation for direct reproduction; do not compare numbers from a different machine as though they were the same experiment.

## Core paired benchmark

Each runtime consumes the same fingerprinted JSON workload: 1,000 child scopes, 256 service lookups, 256 event emits, 16 loader entries and 32 root children. Time metrics show median / p95; throughput metrics show higher-is-better operations per second. PSS covers the complete runtime process.

| Workload | tessivum-core median / p95 | TypeScript Cordis median / p95 | Median result |
|---|---:|---:|---:|
| 1,000 scope create/dispose cycles | 0.877 / 0.995 ms | 20.727 / 25.246 ms | **23.64× faster** |
| Service lookup | 23.011 / 23.814 M ops/s | 1.110 / 1.181 M ops/s | **20.73× throughput** |
| Event emit | 10.667 / 12.047 M ops/s | 0.396 / 0.428 M ops/s | **26.92× throughput** |
| Load 16 entries | 0.448 / 0.542 ms | 2.156 / 2.328 ms | **4.81× faster** |
| Update 16 entries | 19.854 / 30.167 ms | 0.536 / 0.592 ms | **37.03× slower** |
| Dispose root with 32 children | 0.069 / 0.079 ms | 0.218 / 0.253 ms | **3.15× faster** |
| Peak process PSS | 4.59 / 4.59 MiB | 80.03 / 81.13 MiB | **17.43× lower** |
| Process PSS after root disposal | 4.64 / 4.65 MiB | 91.20 / 92.77 MiB | **19.65× lower** |
| Live registrations after disposal | 0 / 0 | 0 / 0 | equal; no residue |

The post-disposal PSS row is process memory after logical root disposal, not a leak count. The registration-residue row is the semantic leak check.

## Product benchmark

Both manifests use the same offline recorded replay and no external model or API key. Base runs the Rust product alone. Compatibility additionally boots the pinned Legacy Node Host and the two fixed Browser plugins. Browser timings include a bounded probe for optional onboarding dialogs; the prompt-to-marker rows isolate the actual local replay round trip after submission.

| Metric | Base median / p95 | Compatibility median / p95 |
|---|---:|---:|
| Headless replay completion | 44.75 / 54.24 ms | 65.21 / 90.46 ms |
| HTTP ready | 71.06 / 89.97 ms | 1,337.41 / 1,686.20 ms |
| Chromium composer enabled | 1.903 / 2.063 s | 1.911 / 2.016 s |
| First prompt-to-marker round trip | 65.5 / 81.0 ms | 76.0 / 113.0 ms |
| Ten prompt/tool Sessions from first submit | 0.788 / 0.848 s | 0.869 / 0.950 s |
| Idle Host-tree PSS | 38.27 / 38.28 MiB | 114.78 / 115.56 MiB |
| One-Session PSS delta from idle | 4.65 / 5.48 MiB | -9.66 / -8.48 MiB |
| Ten-Session PSS delta from idle | 10.74 / 11.37 MiB | 12.55 / 13.16 MiB |
| Ten-Session PSS delta per Session | 1.074 / 1.137 MiB | 1.255 / 1.316 MiB |
| Full process-tree shutdown | 44.67 / 66.37 ms | 65.86 / 71.41 ms |
| Live processes after shutdown | 0 / 0 | 0 / 0 |
| Successful Host + Chromium samples | 30/30 | 30/30 |

The negative one-Session Compatibility delta is a sampling artifact: shared Host pages were released between the idle and one-Session checkpoints. It is not negative Session memory. Use the ten-Session aggregate for the stable marginal signal.

## Stability and warm-up findings

- No warm-up sample was discarded. Every measured sample launches a fresh process and uses a fresh data directory.
- The first Compatibility headless sample was 147.14 ms, 2.26× its 65.21 ms median. This exposes filesystem/module-cache warming outside the fresh process; median and p95 are reported instead of a best run.
- Core p95/median was at most 1.52 for tessivum-core and 1.22 for TypeScript Cordis. The largest Rust spread was `loader_update`, already reported as the principal regression.
- Product idle and ten-Session absolute PSS were stable: p95/median was at most 1.01. HTTP-ready p95/median was 1.27 for Base and 1.26 for Compatibility.
- Headless processes complete faster than the 100 ms PSS sampling interval. Their peak-PSS p95/median reached 1.44, so headless PSS is retained in raw evidence but excluded from performance claims.
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

- [Core raw JSON](../benchmarks/fixtures/phase9-alpha23/core-paired-30.json) — SHA-256 `4ac31357ab07f5280e57ec510d970cbcd8653e9ed62e9c67daee2f2f3a5263b3`
- [Product raw JSON](../benchmarks/fixtures/phase9-alpha23/product-30.json) — SHA-256 `89f4bfb7169d6074e1d846643041bfc19ad8d8a0579a60a4dab86134684bf52c`
- [Base manifest](../benchmarks/manifests/base.json) — SHA-256 `867692853beccc6735533c547b8892179bd253b17f4aca271ad0be393b5b3a90`
- [Compatibility manifest](../benchmarks/manifests/compatibility.json) — SHA-256 `852952f6e6b206c241259ba1c57beb4e2c0e423d5d868b2e2fede4880b42b947`
- Core workload SHA-256 `82ca294d4fd1042e4d5558b42fef82b7ed03fbdabab29efa14dd3bcac5b6f292`; Core environment SHA-256 `c2ddb47aadef65eac05b306ec4f9f1c0c5e266d56b8e9b77a98579a89c2d5b4a`
- Product environment SHA-256 `76db2dd87235a8cd334eaa0ead8b02347c83da567cb50c5bb42f9020d54ee8a0`; replay SHA-256 `c06e6e82a2e85e1c44659863429db396620a3c5f75722778a566f76cb228c789`
- Product driver SHA-256 `c35c0b4f9944e53ab66b73ffb6026c1d1ef8510b6ba57a59de63bb63fa4d72fd`; Browser driver SHA-256 `59b3d0498848b7b9cec29058557945226c5619bf5c834e1df5fcc14dbfe84ae1`
- DeepSeek Harness compatibility patch SHA-256 `9e914d5998ccb2ca1faf8315a9d9a7235407c7830a8939255cd5838acd149ccd`

## Claim boundary

These numbers establish the frozen Alpha.23 workloads on the recorded Linux environment. They do not establish production LLM latency, token throughput, quality, multi-user saturation, bare-metal Linux performance, or a full DeepSeek Harness product comparison. The TypeScript comparison is limited to the Core operations that both runtimes execute from the same fixture. The upstream product remains `unmeasured` because it does not consume the identical product replay through an equivalent driver.

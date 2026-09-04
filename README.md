# Tessivum

[![CI](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml/badge.svg)](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml)

English | [简体中文](README.zh-CN.md)

> 道器相成  
> Principle and implementation, in concert.

Tessivum is an independent, Rust-native agent harness. The Host, Agent, sessions, tools, API, and SDK run in Rust; Legacy Node and Browser Cordis remain as explicit compatibility boundaries for existing plugins and the upstream React UI.

> Tessivum is a community project. It is not affiliated with DeepSeek and is not an official DeepSeek project.

## Status

`v0.1.0-alpha.23` is a prerelease. It uses `tessivum-core v0.1.6` at revision `81a1803d5f376615ccce80a247fc9cd3ab4fe96e` and targets DeepSeek Harness `0.1.0-rc.5` at commit `47f943859bef60e4160492346772ded9b24f765a`.

Available today:

- Rust-native Host, Agent Loop, sessions, tools, persistence, HTTP, SSE, WebSocket, and SDK runtimes;
- Standard, PTC, Minimal, Composition, and strict custom Agent Modes;
- Native, Extism/WASM, Browser, and bounded Legacy Node plugin paths;
- Headless CLI, TypeScript/Python SDK clients, and the source-compatible React Web shell;
- OpenAI Responses-compatible relays, attachments, multi-workspace authority, plugin management, the first-party Market, and Rust-owned Remote Access.

Full DeepSeek Harness Agent/LLM wire compatibility is not complete. See the exact [compatibility baseline](docs/COMPATIBILITY_BASELINE.md) and [known plugin matrix](docs/PLUGIN_COMPATIBILITY.md).

## Install

### Supported packages

| Platform | Architecture | Archive | SHA-256 |
| --- | --- | --- | --- |
| macOS | Apple Silicon | [`.tar.gz`](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-aarch64-apple-darwin.tar.gz) | [checksum](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-aarch64-apple-darwin.tar.gz.sha256) |
| macOS | Intel | [`.tar.gz`](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-x86_64-apple-darwin.tar.gz) | [checksum](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-x86_64-apple-darwin.tar.gz.sha256) |
| Linux (glibc) | ARM64 | [`.tar.gz`](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-aarch64-unknown-linux-gnu.tar.gz) | [checksum](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-aarch64-unknown-linux-gnu.tar.gz.sha256) |
| Linux (glibc) | x86_64 | [`.tar.gz`](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-x86_64-unknown-linux-gnu.tar.gz) | [checksum](https://github.com/wavetao2010/tessivum/releases/download/v0.1.0-alpha.23/tessivum-0.1.0-alpha.23-x86_64-unknown-linux-gnu.tar.gz.sha256) |
| Windows native | x86_64/ARM64 | Not published | — |

The existing installer is **not macOS-only**: it detects both macOS and Linux on x86_64/ARM64. Native Windows has not passed the release build, archive, launcher, plugin, Browser, upgrade, or process-cleanup gates. Windows users can run the Linux package inside WSL2 as an unverified workaround; a Linux archive is not a native Windows executable.

### Homebrew — macOS or Linux

Homebrew is the shortest path and installs Bun and pnpm for Web and Legacy plugins:

```bash
brew tap wavetao2010/tap
brew install tessivum
tessivum --version
tsv --version
```

### No-sudo installer — macOS or Linux

The installer selects the correct release archive, verifies its SHA-256, installs under `~/.local/lib/tessivum`, and updates launchers in `~/.local/bin`:

```bash
curl -fsSLO https://raw.githubusercontent.com/wavetao2010/tessivum/v0.1.0-alpha.23/install.sh
sh install.sh 0.1.0-alpha.23
export PATH="$HOME/.local/bin:$PATH"
tessivum --version
```

It does not use `sudo` or edit shell startup files. Install Bun 1.3.14+ and pnpm 10+ separately before using `tessivum web` or Legacy Node plugins.

### Manual archive — macOS or Linux

Download the archive and adjacent `.sha256` file from the [Alpha.23 release](https://github.com/wavetao2010/tessivum/releases/tag/v0.1.0-alpha.23). Choose one of the four target names listed above.

Linux example:

```bash
version=0.1.0-alpha.23
target=x86_64-unknown-linux-gnu # use aarch64-unknown-linux-gnu on ARM64
base="https://github.com/wavetao2010/tessivum/releases/download/v$version"
curl -fLO "$base/tessivum-$version-$target.tar.gz"
curl -fLO "$base/tessivum-$version-$target.tar.gz.sha256"
sha256sum -c "tessivum-$version-$target.tar.gz.sha256"
tar -xzf "tessivum-$version-$target.tar.gz"
"./tessivum-$version-$target/bin/tessivum" --version
```

On macOS, select an `apple-darwin` target and replace `sha256sum -c` with `shasum -a 256 -c`.

### Build from source

Source builds require Rust stable with `rustfmt` and `clippy`, Bun 1.3.14+, pnpm 10+, Git, and network access to the pinned dependencies.

```bash
git clone https://github.com/wavetao2010/tessivum.git
cd tessivum
cargo run --release -- web
```

Native Windows source builds are not currently release-tested or supported.

## Start

```bash
tessivum web
```

Open <http://127.0.0.1:3000>, then configure a model relay from **Models/Settings**.

For a standard OpenAI Responses-compatible endpoint:

```bash
export OPENAI_API_KEY='relay-key'
export OPENAI_BASE_URL='https://relay.example/v1'
export OPENAI_MODEL='model-name'

tessivum --provider openai-responses --model "$OPENAI_MODEL" "inspect this repository"
```

`OPENAI_BASE_URL` is a prefix; Tessivum appends `/responses`. Custom provider routes also support `/chat/completions` and `/messages`. Direct ChatGPT/Codex OAuth is not implemented.

## Plugins

The first-party Market is installed or upgraded on `tessivum web`. Other package changes use the pnpm-owned profile under `${TESSIVUM_HOME:-$HOME/.tessivum}/plugins`:

```bash
tessivum plugin add @scope/package
tessivum plugin remove @scope/package

# Frozen compatibility samples
tessivum plugin add dsh-better-sidebar@0.16.1
tessivum plugin add dsh-dream-skin@8.30.1
```

Legacy Node plugins and lifecycle scripts execute with the user's permissions; they are not a WASM sandbox. Only the versions named in the [plugin compatibility matrix](docs/PLUGIN_COMPATIBILITY.md) carry compatibility evidence.

## Architecture

```text
CLI / HTTP / SDK / React Web
              |
      Rust Host + Agent Runtime
              |
         Tessivum Core
       /        |         \
   Native   Extism/WASM   Legacy Node + Browser Cordis
```

The Rust Host remains authoritative for permissions, durable state, tools, and remote access. Architecture details live in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Remote Access

Remote Access is **disabled by default** and the Rust listener remains **loopback-only**. Start `tessivum web`, open **Settings → Remote access** or <http://127.0.0.1:3000/remote>, review the public-tunnel notice, then explicitly enable Cloudflare Quick Tunnel.

Tessivum owns pairing, device sessions, revocation, and HTTP/WebSocket authorization. Cloudflare transports traffic and can observe it; Quick Tunnel URLs are temporary. See the [Remote Access security boundary](docs/PHASE8_REMOTE_ACCESS_COMPATIBILITY_PLAN.md).

## Data, upgrade, and uninstall

The data root is `--data-dir`, then absolute `TESSIVUM_HOME`, then `$HOME/.tessivum`. Program upgrades and uninstall do not remove user data.

```bash
brew upgrade tessivum
brew uninstall tessivum

# No-sudo installation
sh install.sh 0.1.0-alpha.23
sh install.sh --uninstall

# Explicit and destructive data removal
rm -rf "${TESSIVUM_HOME:-$HOME/.tessivum}"
```

Back up the data root before changing Alpha versions. Binary rollback does not downgrade data or plugin schemas.

## Security and release provenance

- Release archives and first-party Market artifacts include SHA-256 checksums and source metadata.
- Checksums detect corruption; they are not signatures. Alpha.23 binaries are not code-signed or notarized.
- HTTP listeners remain loopback-only unless Remote Access is explicitly enabled with its separate authority checks.
- Legacy Node plugins and pnpm subprocesses are trusted local code, not sandboxed extensions.

## Reproducible benchmark

The published 30-sample Linux run reports tessivum-core at **24.05× faster** scope create/dispose, **20.53×** service-lookup throughput, **25.42×** event throughput, and **17.15× lower** PSS with 1,000 live scopes than TypeScript Cordis 4.0.1. It also discloses a **40.05× slower** Loader update path.

All four real-Chromium product cells passed **30/30**. Against DeepSeek Harness, Tessivum Base reached HTTP readiness **5.83× faster** with **4.52× less** idle Host-tree PSS; Compatibility used **1.63× less** idle PSS but reached HTTP readiness **9.31× slower**.

See the [benchmark report](docs/PHASE9_BENCHMARK_REPORT.md), [Core raw JSON](benchmarks/fixtures/phase9-alpha23/core-paired-30.json), and [product raw JSON](benchmarks/fixtures/phase9-alpha23/product-30.json). These are frozen offline workloads, not production LLM or concurrent-user benchmarks.

## Development verification

```bash
python3 scripts/check_compat_baseline.py
python3 scripts/check_release_facts.py
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## Documentation

- [Architecture](docs/ARCHITECTURE.md)
- [Compatibility baseline](docs/COMPATIBILITY_BASELINE.md)
- [Plugin compatibility](docs/PLUGIN_COMPATIBILITY.md)
- [Community plugin submission and verification](docs/PLUGIN_VERIFICATION.md)
- [Remote Access compatibility and security](docs/PHASE8_REMOTE_ACCESS_COMPATIBILITY_PLAN.md)
- [Benchmark and ecosystem plan](docs/PHASE9_BENCHMARK_ECOSYSTEM_PLAN.md)
- [30-sample benchmark report](docs/PHASE9_BENCHMARK_REPORT.md)
- [Development history](docs/DEVELOPMENT_PLAN.md)

## License

Tessivum source is licensed under the [MIT License](LICENSE). Release archives also contain `THIRD_PARTY_LICENSES.txt`; compatibility does not relicense bundled Cordis, DeepSeek Harness-derived Browser sources, or npm dependencies.

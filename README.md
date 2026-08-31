# Tessivum

[![CI](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml/badge.svg)](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml)

Tessivum is an independent, Rust-native agent harness inspired by DeepSeek Harness and Cordis. It targets two explicit compatibility planes—Legacy Node for existing npm/Cordis plugins and Browser Cordis for the upstream React UI—while moving the Host, Agent, session, tool, API, and SDK runtime to Rust.

> Tessivum is a community project. It is not affiliated with DeepSeek, is not an official DeepSeek project, and does not aim to replace the official DeepSeek Harness repository or its publication process.

## Alpha status

`v0.1.0-alpha.16` is a prerelease, not a production-stable API or data-format promise. It adds fixed `dsh-dream-skin@8.30.1` Host/Browser compatibility while keeping Legacy web routes confined to the supported plugin namespaces.

Current implementation foundation:

- Rust Host, Agent, Agent Loop, sessions, tools, system prompt, and provider-neutral LLM runtime;
- session-owned Native Agent Modes: Standard, PTC, Minimal, Composition, and strict custom `mode.toml` bundles;
- durable JSONL/SQLite sessions, cold resume, rollback, and bounded transports;
- Headless CLI plus NDJSON JSON-RPC/ACP SDK with TypeScript and Python clients;
- HTTP full-form RPC, durable SSE, and Browser WebSocket downlinks;
- Native/WASM/Browser routing plus a real Legacy Node compat-host over the bounded `cordis.node/v1` bridge and DomainBridge services;
- Extism service permissions, settings/credentials, multi-workspace authority, attachments, an OpenAI Responses adapter, and the frozen upstream `AppWebEntry` source shell;
- a pnpm-owned plugin profile with ordered Host Bundle authority, exact Loader/Fiber inventory, bounded HTTP/WebSocket plugin routes, packaged Host compatibility modules, and verified `dshmarket@1.29.2` plus `dsh-better-sidebar@0.16.1` lifecycle behavior.

The frozen DeepSeek Harness `0.1.0-rc.5` compatibility baseline remains complete:

- the source Web shell and all 38 composed client packages build from commit `47f943859bef60e4160492346772ded9b24f765a`; Tessivum applies its checked-in compatibility patch before auditing and building the source tree;
- the Rust `/api` dispatcher implements all 52 frozen Core RPC method names and both Browser WebSocket downlinks;
- provider-neutral streaming, retry, cancellation, atomic queue/steer, durable sessions, Native Agent Modes, Subagents, Workflow, Native/WASM tools, and JSONL replay are covered by focused Rust contracts;
- all 69 source-Web Chromium scenarios pass as the behavioral schema/event parity gate.

The authoritative compatibility contract is [`docs/COMPATIBILITY_BASELINE.md`](docs/COMPATIBILITY_BASELINE.md). Alpha.10–12 architecture and distribution gates are recorded in [`docs/PHASE4_BRAND_DISTRIBUTION_MARKET_PLAN.md`](docs/PHASE4_BRAND_DISTRIBUTION_MARKET_PLAN.md); Alpha.13 Native Mode release gates are recorded in [`docs/PHASE5_NATIVE_AGENT_MODES_PLAN.md`](docs/PHASE5_NATIVE_AGENT_MODES_PLAN.md).

Alpha.15 DSH Profile authority, market activation, upgrade, rollback, and distribution-alias gates are recorded in [`docs/PHASE6_DSH_PROFILE_COMPATIBILITY_PLAN.md`](docs/PHASE6_DSH_PROFILE_COMPATIBILITY_PLAN.md).

## Architecture

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

The Host remains authoritative for permissions and durable facts. Legacy Node and Browser Cordis are intentional compatibility planes, not temporary TypeScript Host/Agent implementations.

### Legacy Node boundary

Tessivum does not export the legacy Host modules `agentCore`, `llm`, `systemPrompt`, `sessionStore`, or `toolRuntime` into Node Cordis contexts. A plugin that requires one of those module names is unsupported: direct lookup stays absent, and a named bridge call must return an explicit `SERVICE_UNAVAILABLE`/`UNKNOWN_SERVICE` error—never a successful no-op, empty model result, or fabricated service.

Supported cross-runtime operations use the bounded, versioned DomainBridge contracts (`agents@1`, `llm@1`, `systemPrompt@1`, `sessions@1`, and `tools@1`). These contracts are not aliases for the omitted legacy modules.

### Native Agent Modes

An Agent Mode is the immutable, session-owned bundle of prompt policy, model-facing tool presentation, native tool allowlist, Skills, planning, compaction, and Native/WASM/Legacy plugin entries. The Rust Host owns resolution and lifecycle; Browser `agentPreset.*` names are frozen wire compatibility only. Tessivum does not execute upstream `agent.cordis.yml` or arbitrary JavaScript mode definitions.

Built-ins:

- **Standard** exposes the Host's available native tool catalog directly.
- **PTC** exposes one `run_code` tool backed by Bun and a restricted nested native-tool SDK.
- **Minimal** exposes persistent `bash` plus `str_replace_editor` with a complete replacement prompt.
- **Composition** adds typed `composition_inspect`, `composition_define`, `composition_validate`, `composition_run`, and `composition_stop` tools; descriptors can reference Native, WASM, or Legacy entries but cannot contain executable source.

Custom modes live at `${TESSIVUM_HOME:-$HOME/.tessivum}/modes/<id>/mode.toml`. For an isolated data root:

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

Select it through an ordered CLI patch:

```yaml
# /tmp/review-mode.yml
agent-presets:
  default: review
```

```bash
tessivum --data-dir /tmp/tessivum-data web --patch /tmp/review-mode.yml
```

Later `--patch` files override earlier files recursively. Unknown fields, unknown tool capabilities, duplicate IDs, path escapes, missing required native tools, missing Bun, and unavailable plugin runtimes fail with structured errors; Tessivum does not silently expand or downgrade a mode.

## Requirements

Source builds require Rust stable with `rustfmt` and `clippy`, Bun 1.3.14 or newer, pnpm 10 or newer, Git, and network access to the pinned `tessivum-core` revision and frozen npm inputs.

Prebuilt archives do not require Rust, Git, or a system Node.js. Core Headless/Web operation does not require Bun or pnpm. Bun is required when Legacy Node plugins run; pnpm is required for `tessivum plugin add/remove` and dshmarket mutations. The Homebrew formula installs both runtime dependencies.

## Install

### Homebrew Tap

```bash
brew tap wavetao2010/tap
brew install tessivum
tessivum --version
tsv --version
```

### No-sudo installer

Download the installer before running it; it installs versioned releases under `~/.local/lib/tessivum` and atomically updates the `tessivum` and `tsv` launchers:

```bash
curl -fsSLO https://raw.githubusercontent.com/wavetao2010/tessivum/v0.1.0-alpha.16/install.sh
sh install.sh 0.1.0-alpha.16
```

The script verifies the adjacent SHA-256 file, rejects unsafe archive paths, does not use `sudo`, and does not modify shell startup files.

### Prebuilt archives

Download the archive and adjacent `.sha256` file for your platform from the [Alpha.16 release](https://github.com/wavetao2010/tessivum/releases/tag/v0.1.0-alpha.16), verify the checksum, then run either packaged launcher:

```bash
target=x86_64-unknown-linux-gnu  # or aarch64-unknown-linux-gnu, x86_64-apple-darwin, aarch64-apple-darwin
sha256sum -c "tessivum-0.1.0-alpha.16-$target.tar.gz.sha256"
tar -xzf "tessivum-0.1.0-alpha.16-$target.tar.gz"
"./tessivum-0.1.0-alpha.16-$target/bin/tessivum" --version
"./tessivum-0.1.0-alpha.16-$target/bin/tsv" --version
```

On macOS, use `shasum -a 256 -c` instead of `sha256sum -c`. Archives are checksum-verified but are not code-signed or notarized.

## Source quick start

### Deterministic Headless smoke

This runs the checked-in recorded model stream and explicitly enables the trusted Bash fixture:

```bash
cargo run --release -- \
  --session alpha-smoke \
  --data-dir /tmp/tessivum-alpha \
  --replay fixtures/headless/recorded-replay.jsonl \
  --trusted-bash \
  "prove the CLI tool round trip"
```

Expected final output:

```text
CLI tool round trip complete: CLI_TOOL_ROUND_TRIP
```

`--trusted-bash` grants the process native shell permissions. Do not enable it for untrusted prompts.

### OpenAI Responses relay

The native adapter targets the standard Responses protocol with bearer authentication. `OPENAI_BASE_URL` is a prefix; Tessivum appends `/responses`.

```bash
export OPENAI_API_KEY='relay-key'
export OPENAI_BASE_URL='https://relay.example/v1'
export OPENAI_MODEL='codex-model-name'

# Browser or SDK
cargo run --release -- web

# Headless
cargo run --release -- \
  --provider openai-responses \
  --model "$OPENAI_MODEL" \
  "inspect this repository"
```

The Responses adapter sends `store: false`, streams text/reasoning/function calls, persists encrypted reasoning items for stateless tool-call continuation, and materializes validated AttachmentRef images as Responses data URLs. Custom provider routes also dispatch `openai-completions` to `/chat/completions` and `anthropic-messages` to `/messages`; the selected protocol is no longer treated as Responses. `openai-codex-responses` remains out of scope because it is an OAuth transport rather than the API-key Responses contract.

### Community plugins and dshmarket

All package mutations target `${TESSIVUM_HOME:-$HOME/.tessivum}/plugins` and use pnpm. Installs ignore lifecycle scripts unless the profile contains a non-empty, explicit `pnpm.onlyBuiltDependencies` allowlist.

`package.json.dependencies` is the installed-package inventory. `dsh.profile.bundles` is the only Host Bundle enablement and ordering authority. Alpha.15 atomically creates the field once for older profiles; an explicit empty array remains empty. Generic client-only packages stay outside the Host Bundle stack.

```bash
tessivum plugin add @scope/package
tessivum plugin remove @scope/package

# Frozen compatibility targets
tessivum plugin add dshmarket@1.29.2
tessivum plugin add dsh-better-sidebar@0.16.1
tessivum plugin add dsh-dream-skin@8.30.1
tessivum web

`tessivum` and `tsv` are the same launcher and resolve the same data root. After a CLI mutation—or whenever dshmarket reports “重启后生效”—restart the Web process. dshmarket reads the current Profile plus settled Loader/Fiber inventory; Tessivum does not expose a global `dsh` shim or maintain a second Node-side Loader state.

Legacy plugins and their lifecycle scripts are trusted code running with the user's permissions, not a sandbox. `web.route/v1` registrations remain Rust-owned, same-origin, prefix-restricted, size-bounded, deadline-bounded, cancellable, and generation-scoped. Packaged deployments locate the compatibility host, Cordis vendor, Host modules, and Agent Presets relative to the launcher; source checkouts use their pinned development paths.

### Browser shell

```bash
cd web
bun install --frozen-lockfile
bun run build
cd ..
cargo run --release -- web
```

Open <http://127.0.0.1:3000>. Web can configure a relay from the published Models/Settings surface; `OPENAI_*` remains available for Headless, SDK, CI, and managed deployments.

### SDK mode

```bash
cargo run --release -- sdk
```

SDK mode reads newline-delimited JSON-RPC from stdin and writes protocol frames to stdout. Client implementations live in [`sdk/typescript`](sdk/typescript) and [`sdk/python`](sdk/python).

## Data migration, upgrade, rollback, and uninstall

The data-root precedence is `--data-dir`, then absolute `TESSIVUM_HOME`, then `$HOME/.tessivum`. If the new default does not exist but `./.tessivum` does, Tessivum stops with a migration diagnostic instead of silently creating a second state tree. Back up both directories, then move the old tree explicitly only when the destination is absent:

```bash
test ! -e "$HOME/.tessivum" && mv ./.tessivum "$HOME/.tessivum"
```

Alpha.11 made pnpm the only plugin-profile mutation backend. Alpha.15 adds the ordered `dsh.profile.bundles` authority without changing dependencies, sessions, settings, or credentials; explicit empty bundle arrays are preserved. Back up `$TESSIVUM_HOME/plugins` (or `$HOME/.tessivum/plugins`) before migration or restore.

Homebrew upgrades switch the program without deleting user data. The no-sudo installer retains versioned program directories; rerunning `sh install.sh <older-version>` atomically repoints the launcher. Binary rollback does not rewrite an Alpha data or plugin profile, so restore the matching backup if the newer release changed it.

```bash
brew uninstall tessivum              # program only
sh install.sh --uninstall            # program only
rm -rf "${TESSIVUM_HOME:-$HOME/.tessivum}"  # explicit, destructive data removal
```

## Security and provenance

- Release archives are built from the tagged Tessivum source and the exact `tessivum-core` Git revision in `Cargo.lock`; the Browser baseline is pinned to DeepSeek Harness commit `47f943859bef60e4160492346772ded9b24f765a`.
- Host compatibility npm inputs are exact versions with registry URLs, SHA-512 integrities, file hashes, and licenses in `packaging/host-modules.json`; archives include `THIRD_PARTY_LICENSES.txt` and `release-metadata.json`.
- The installer and Homebrew formula consume the same four release archives and fixed SHA-256 values. There is no floating `latest` package resolution in release assembly.
- HTTP listeners are loopback-only. Legacy Node plugins and pnpm subprocesses are not sandboxed; inspect packages before installation and keep lifecycle scripts disabled unless explicitly required.
- Checksums detect corruption but are not signatures. Alpha.15 binaries are not code-signed or notarized; verify the release tag, checksum asset, and repository origin before execution.

## Verification

```bash
python3 scripts/check_compat_baseline.py
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cd web && bun install --frozen-lockfile && bun run build && bun run test:source-client
bun test ./tests/migrated.test.ts --max-concurrency 1 --timeout 1200000
```

These gates exercise the Rust-native runtime, the pinned DeepSeek client packages, all 69 source-Web Chromium scenarios, Headless and SDK journeys, Extism permissions, persistence, rollback, workspace isolation, and shutdown.

## Known Alpha limits

- only production-wired cwd-sensitive capabilities are workspace-scoped; latent library-only Skills/LSP/Filesystem integrations remain unavailable from the Host;
- Browser configuration exposes only the published settings namespace allowlist; arbitrary registered namespaces remain Host-internal;
- WASM product permissions currently expose only `logger@1.log`, `tools@1.schemas`, `settings@1.describe`, `credentials@1.describe`, and `systemPrompt@1.assemble`;
- the native Responses adapter requires the standard API-key `/responses` contract; direct ChatGPT/Codex OAuth and remote image URLs are not wired;
- image-bearing MCP/tool-result serialization is covered by focused adapter tests; the real Browser E2E currently exercises text tool continuation plus user image input, because no production-configured image-producing tool is exposed;
- API listeners are loopback-only; prebuilt archives are checksum-verified but are not code-signed or notarized.
- community-plugin compatibility is verified only for `dshmarket@1.29.2` and `dsh-better-sidebar@0.16.1`; other versions, other packages, and hot activation remain unsupported until separately verified.

These are product follow-ups, not work to change or deprecate the official DeepSeek Harness project.

## Documentation

- [Runtime architecture](docs/ARCHITECTURE.md)
- [Development and cutover plan](docs/DEVELOPMENT_PLAN.md)
- [Plugin compatibility](docs/PLUGIN_COMPATIBILITY.md)
- [Phase 3 product capability plan](docs/PHASE3_PRODUCT_PLAN.md)
- [Phase 4 branding, distribution, and dshmarket plan](docs/PHASE4_BRAND_DISTRIBUTION_MARKET_PLAN.md)
- [DeepSeek Harness compatibility baseline](docs/COMPATIBILITY_BASELINE.md)
- [Web E2E port checklist (69 upstream files)](docs/WEB_E2E_PORT_CHECKLIST.md)

## License

Tessivum source is licensed under the [MIT License](LICENSE). Release archives also contain `THIRD_PARTY_LICENSES.txt`; compatibility does not relicense bundled Cordis, DeepSeek Harness-derived Browser sources, or npm dependencies.

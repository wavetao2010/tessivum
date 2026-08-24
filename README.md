# Tessivum

[![CI](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml/badge.svg)](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml)

Tessivum is an independent, Rust-native agent harness inspired by DeepSeek Harness and Cordis. It targets two explicit compatibility planes—Legacy Node for existing npm/Cordis plugins and Browser Cordis for the upstream React UI—while moving the Host, Agent, session, tool, API, and SDK runtime to Rust.

> Tessivum is a community project. It is not affiliated with DeepSeek, is not an official DeepSeek project, and does not aim to replace the official DeepSeek Harness repository or its publication process.

## Alpha status

`v0.1.0-alpha.6` is a source release and reproducible baseline, not a production-stable API promise.

Current implementation foundation:

- Rust Host, Agent, Agent Loop, sessions, tools, system prompt, and provider-neutral LLM runtime;
- durable JSONL/SQLite sessions, cold resume, rollback, and bounded transports;
- Headless CLI plus NDJSON JSON-RPC/ACP SDK with TypeScript and Python clients;
- HTTP full-form RPC, durable SSE, and Browser WebSocket downlinks;
- Native/WASM/Browser routing plus a real Legacy Node compat-host over the bounded `cordis.node/v1` bridge and DomainBridge services;
- Extism service permissions, settings/credentials, multi-workspace authority, attachments, an OpenAI Responses adapter, and the upstream `AppWebEntry` source shell with a 38-package `dsh.client` graph.

The frozen DeepSeek Harness `0.1.0-rc.5` compatibility baseline is complete:

- the source Web shell and all 38 composed client packages build from commit `47f943859bef60e4160492346772ded9b24f765a`; Tessivum applies its checked-in compatibility patch before auditing and building the source tree;
- the Rust `/api` dispatcher implements all 52 frozen Core RPC method names and both Browser WebSocket downlinks;
- provider-neutral streaming, retry, cancellation, atomic queue/steer, durable sessions, presets, Subagents, Workflow, Native/WASM tools, and JSONL replay are covered by focused Rust contracts;
- all 69 source-Web Chromium scenarios pass as the behavioral schema/event parity gate.

The authoritative contract and completion gates are in [`docs/COMPATIBILITY_BASELINE.md`](docs/COMPATIBILITY_BASELINE.md).

## Architecture

```text
Rust CLI / HTTP / SDK
        |
Rust Host + Agent Runtime
        |
Rust Cordis Core
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

## Requirements

- Rust stable toolchain with `rustfmt` and `clippy`;
- Bun 1.3.14 or newer for the Browser shell and Legacy Node compatibility host;
- npm for `tessivum plugin add/remove`;
- Git and network access for the pinned `tessivum-core` dependencies.

## Quick start

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

### Community plugins

Install npm, Git, tarball, or local Cordis packages into the deployment-owned profile, then start Web normally:

```bash
cargo run --release -- plugin add @scope/package
cargo run --release -- plugin remove @scope/package
cargo run --release -- web
```

The default profile is `.tessivum/plugins`. `--data-dir <dir>` selects another profile for the management command. Installs disable npm lifecycle scripts and copy local packages into the confined profile. On Web/SDK startup, ordinary Cordis packages run in the Legacy Node host, Extism declarations run as WASM, `dsh.bundle.patch` insertions are composed into the Host entry tree, and published `dsh.client` bundles join the Browser graph. The plugin inventory reports both Host and Browser entries.

Legacy plugins are trusted code, not a sandbox. The source checkout resolves the compatibility host from `../tessivum-core/node/compat-host` and the pinned Cordis vendor from `../upstream/deepseek-harness/vendor`; packaged deployments can set `TESSIVUM_COMPAT_HOST` and `CORDIS_VENDOR_ROOT` explicitly.

### Browser shell

```bash
cd web
bun install --frozen-lockfile
bun run build
cd ..
cargo run --release -- web
```

Open <http://127.0.0.1:3000>. Alpha6 can configure a relay from the published Models/Settings surface; `OPENAI_*` remains available for Headless, SDK, CI, and managed deployments.

### SDK mode

```bash
cargo run --release -- sdk
```

SDK mode reads newline-delimited JSON-RPC from stdin and writes protocol frames to stdout. Client implementations live in [`sdk/typescript`](sdk/typescript) and [`sdk/python`](sdk/python).

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
- API listeners are loopback-only and this release does not ship prebuilt binaries.

These are product follow-ups, not work to change or deprecate the official DeepSeek Harness project.

## Documentation

- [Runtime architecture](docs/ARCHITECTURE.md)
- [Development and cutover plan](docs/DEVELOPMENT_PLAN.md)
- [Plugin compatibility](docs/PLUGIN_COMPATIBILITY.md)
- [Phase 3 product capability plan](docs/PHASE3_PRODUCT_PLAN.md)
- [DeepSeek Harness compatibility baseline](docs/COMPATIBILITY_BASELINE.md)
- [Web E2E port checklist (69 upstream files)](docs/WEB_E2E_PORT_CHECKLIST.md)

## License

No open-source license has been selected for this Alpha baseline. Copyright remains with the repository owner until a license is added explicitly.

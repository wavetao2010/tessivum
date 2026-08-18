# Tessivum

[![CI](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml/badge.svg)](https://github.com/wavetao2010/tessivum/actions/workflows/ci.yml)

Tessivum is an independent, Rust-native agent harness inspired by the architecture of DeepSeek Harness and Cordis. It keeps the useful compatibility boundaries—Legacy Node for existing npm/Cordis plugins and Browser Cordis for the published React UI—while moving the Host, Agent, session, tool, API, and SDK runtime to Rust.

> Tessivum is a community project. It is not affiliated with DeepSeek, is not an official DeepSeek project, and does not aim to replace the official DeepSeek Harness repository or its publication process.

## Alpha status

`v0.1.0-alpha.4` is a source release and reproducible baseline, not a production-stable API promise.

Implemented and verified:

- Rust Host, Agent, Agent Loop, sessions, tools, system prompt, and recorded LLM runtime;
- durable JSONL/SQLite sessions, cold resume, rollback, and bounded transports;
- Headless CLI plus NDJSON JSON-RPC/ACP SDK with TypeScript and Python clients;
- HTTP API, durable SSE, published full-form RPC, and Browser WebSocket downlinks;
- published Browser Cordis/React shell with workspace/session recovery and tool cards;
- Legacy Node compatibility host with generation cleanup and real Cordis community samples;
- Native/WASM/Legacy/Browser plugin routing and actionable compatibility reports;
- exact per-plugin WASM service permissions plus a pinned real Rust/Extism Guest;
- Browser stop/resume, durable approval request/response/reconnect, and writable redacted settings/credentials;
- one HostRuntime with durable opaque multi-workspace/session authority, restart migration, workspace-scoped Bash, and subagent inheritance.

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

## Requirements

- Rust stable toolchain with `rustfmt` and `clippy`;
- Bun 1.3.14 or newer for the Browser shell and Legacy Node compatibility tests;
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

### Browser shell

```bash
cd web
bun install --frozen-lockfile
bun run build
cd ..
cargo run --release -- web
```

Open <http://127.0.0.1:3000>. The Alpha baseline boots with an unconfigured model adapter unless the embedding application supplies one; the recorded integration tests exercise complete model and tool flows.

### SDK mode

```bash
cargo run --release -- sdk
```

SDK mode reads newline-delimited JSON-RPC from stdin and writes protocol frames to stdout. Client implementations live in [`sdk/typescript`](sdk/typescript) and [`sdk/python`](sdk/python).

## Verification

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cd web && bun install --frozen-lockfile && bun run build
```

The Alpha cutover baseline passes 280 Rust tests across 38 suites, the Browser typecheck/build, real Headless and SDK process journeys, real Chromium stop/approval/settings/credential and multi-workspace/restart interaction, community plugin loading, real Extism allow/deny/trap/update/unload flows, rollback drills, workspace-scoped Bash/subagent inheritance, and graceful shutdown checks.

## Known Alpha limits

- only production-wired cwd-sensitive capabilities are workspace-scoped; latent library-only Skills/LSP/Filesystem integrations remain unavailable from the Host;
- Browser configuration exposes only the published settings namespace allowlist; arbitrary registered namespaces remain Host-internal;
- WASM product permissions currently expose only `logger@1.log`, `tools@1.schemas`, `settings@1.describe`, and `credentials@1.describe`;
- several unpublished upstream Browser packages require explicit compatibility overrides;
- API listeners are loopback-only and this release does not ship prebuilt binaries.

These are product follow-ups, not work to change or deprecate the official DeepSeek Harness project.

## Documentation

- [Runtime architecture](docs/ARCHITECTURE.md)
- [Development and cutover plan](docs/DEVELOPMENT_PLAN.md)
- [Plugin compatibility](docs/PLUGIN_COMPATIBILITY.md)
- [Phase 3 product capability plan](docs/PHASE3_PRODUCT_PLAN.md)

## License

No open-source license has been selected for this Alpha baseline. Copyright remains with the repository owner until a license is added explicitly.

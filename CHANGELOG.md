# Changelog

## Unreleased

### Added

- Pair the full product benchmark against a clean DeepSeek Harness `0.1.0-rc.5`
  build under the same Base and Compatibility Browser contracts, with 30-sample
  Linux evidence and bilingual comparison tables.
- Add frozen Windows Server 2025 CI gates for the product and Core, covering
  MSVC compile/Clippy/tests, Web and plugin dependencies, Extism, and a real
  Legacy Node lifecycle.

### Changed

- Pin `tessivum-core v0.1.6` to revision
  `2a7f39006f7bde5b680405b2827f32d85ab20c26` for Windows executable resolution,
  platform-correct supervision, isolated Bun transpilation, retained host stderr diagnostics, and the required Windows `SystemRoot`.

### Fixed

- Give cold Legacy Node plugin activation the Web readiness deadline instead of
  failing after five seconds under process-cold Compatibility runs.

## 0.1.0-alpha.23 - 2026-09-03

### Added

- Add frozen Base and Compatibility benchmark manifests, a process-cold
  product driver, real Chromium replay checks, full Host-descendant Linux PSS,
  one/ten loaded resident-Session measurements, and strict residue reporting.
- Add the pinned Ubuntu 24.04 container path and the paired
  TypeScript Cordis/tessivum-core benchmark protocol.
- Add fail-closed benchmark snapshot regression checks; changed environments,
  revisions, versions, workloads, or metric sets reject comparison explicitly.
- Add the Simplified Chinese README and the independent Tessivum slogan
  “道器相成” / “Principle and implementation, in concert.”
- Add the live community-catalog trust overlay, exact-release verification ledger,
  public request form, no-secret CI workflow, revocation path, and bilingual
  reproduction/risk documentation.
- Verify `dsh-better-sidebar@0.16.1` through Profile preflight, exact install,
  Legacy Host and real Chromium startup, update, full removal, rollback, and graceful process cleanup.
- Publish the bilingual 30-sample Core/Base/Compatibility report, checked raw
  JSON, README claims, and a release-fact drift gate.

### Changed

- Mark the old three-sample, Rust-only Core result as historical and
  non-comparative. The Alpha.23 publication result uses 30 valid process-cold
  samples per runtime and case and reports median, p95, regressions, full-tree
  PSS, compatibility success, and measurement limits together.

### Fixed

- Accept the `sessions` and `tools` services that the Legacy Host actually
  provides, allowing the pinned Better Sidebar compatibility profile to pass
  preflight instead of being rejected as unavailable.
- Separate package-entry and bundle-patch byte limits so the pinned
  Better Sidebar client entry is accepted while bundle patches remain capped
  at 1 MiB.
- Restrict static runtime-import captures to package-specifier characters so
  diagnostic strings such as `require('${spec}')` are not treated as missing
  dependencies.
- Exclude terminated Linux zombie processes from shutdown residue and bound
  optional onboarding-dialog probes, removing two benchmark timing artifacts.

## 0.1.0-alpha.22 - 2026-09-03

### Changed

- Local users can enable or disable the built-in Cloudflare Remote Access flow from `/remote`; the remembered choice is applied automatically on later Web Host launches without environment variables.

### Fixed

- The Remote Access header now uses the shipped Tessivum mark instead of an empty decorative square.
- A remembered Quick Tunnel that cannot start now falls back to the local Web Host and disables Remote Access instead of trapping later launches in a startup failure.

## 0.1.0-alpha.21 - 2026-09-03

### Fixed

- First-party market migration now removes the incompatible `@linxin666/dsh-remote-web-ui` package and bundle before Legacy Node activation, leaving the built-in Rust-owned Remote Access implementation as the sole owner.

## 0.1.0-alpha.20 - 2026-09-02

### Added

- Rust-supervised Cloudflare Quick Tunnels for account-free Remote Access, including explicit executable/PATH discovery and checksum-verified pinned downloads on supported macOS and Linux targets.

### Security

- Quick Tunnel authority rotation is atomic and fail-closed: an exited tunnel immediately loses Host/Origin admission before bounded exponential restart installs its replacement URL.

### Changed

- `TESSIVUM_REMOTE_AUTO_TUNNEL=cloudflare` now supplies the trusted-TLS posture and generated authority; the existing operator-owned tunnel configuration remains available for stable domains.

## 0.1.0-alpha.19 - 2026-09-01

### Added

- Rust-owned Remote Access with one-time pairing, persistent device sessions, local device administration, exact `remoteAccess@1` Browser/Legacy APIs, and the built-in `/remote` QR pairing surface.
- An immutable Remote Access entry in loopback Web settings; paired devices reuse the existing Tessivum Web shell instead of a second Session/Workspace implementation.
- Editable connected-provider cards plus custom OpenAI-compatible provider creation, draft endpoint/key model discovery, capacity editing, and persisted model selection in Web settings.

### Security

- The HTTP, SSE, and WebSocket boundary now requires exact trusted Host and Origin, same-origin Fetch metadata, an explicit trusted-TLS tunnel posture, and a live Rust device session for every protected remote request.
- Remote Access remains disabled by default, the Rust listener remains loopback-only, pairing and administrative mutations remain local-only, raw secrets are never persisted or returned in state, and revocation promptly closes active SSE and WebSocket streams.
- Anonymous remote access is limited to a bounded public-posture read, the built-in pairing flow, and fixed Browser assets; Legacy Node Web routes remain loopback-only.

### Changed

- Remote browsers may read redacted settings and credential state needed by the existing Web shell, while settings, credentials, workspace, plugin-host activation, filesystem, and Host shutdown mutations remain loopback-only.
- Tessivum pins `tessivum-core v0.1.6` revision `bafb893f182d64b7b464b6cf827676f7ac368168` for correlated Legacy cancellation and Browser `AbortSignal` propagation.
- Accepted Agent work remains in the local Host across browser reloads, disconnects, remote session expiry, and device revocation; only explicit Stop/cancel terminates the run.

### Fixed

- Profile reconciliation prunes stale bundle names after package removal before validating the remaining bundle graph.
- Native/WASM settings access rejects Node-only mutation methods at the read-only DomainBridge boundary.
- The deep package-interoperability fixture uses the canonical `service.call.params` envelope.

## 0.1.0-alpha.18 - 2026-08-31

### Fixed

- Packaged Web startup uses pnpm 11's supported config form when removing a legacy market package, so the first-party market migration no longer rolls back after installation.

## 0.1.0-alpha.17 - 2026-08-31

### Added

- `tessivum-market`, derived from the MIT-licensed `dshmarket@1.38.1` source, as a first-party Host + Browser plugin with retained upstream provenance and license files.
- Checksum-verified market tarballs in release archives and GitHub release assets, with immutable copies under the user data root.
- Native Host + Bun client + Chromium coverage for first install, category and theme discovery, and profile snapshot restoration.

### Changed

- Packaged Web startup transactionally installs or upgrades the matching market release and replaces legacy `dshmarket`/`dsh-market` dependency and bundle entries while preserving market state.
- Market update mutations consume the exact version and registry selected by the latest check; first-party self-update and self-removal are rejected.
- Tessivum pins `tessivum-core v0.1.6` revision `4c3d7b7769e43e2eb228ebf43d46bef6119c4574` for the market integration release.

## 0.1.0-alpha.16 - 2026-08-31

### Added

- Frozen compatibility coverage for `dsh-dream-skin@8.30.1`, including its Host persistence route and Browser client bundle.

### Changed

- Legacy web routes are restricted to the supported `/dsh-market`, `/sidebar`, and `/dream-skin` namespaces.
- Tessivum pins the `tessivum-core v0.1.6` route-policy revision at `7150b20df296e52403de00f36fdc1dd9bf93edde`.
- Process shutdown keeps a bounded ten-second drain window so real Legacy plugin disposers can finish before forced exit; packaged smoke rejects forced or failed shutdown.

## 0.1.0-alpha.15 - 2026-08-28

### Added

- Ordered `dsh.profile.bundles` Host Bundle authority with one-time atomic migration for older profiles and preservation of explicit empty bundle lists.
- Collision-safe `tsv` aliases in release archives, Homebrew, and the no-sudo installer; `tessivum` remains the canonical command and no `dsh` shim is installed.
- Native-backed Legacy `agents` compatibility for Side Chat create, resume, messaging, cancellation, and generation-owned disposal.

### Changed

- CLI and dshmarket mutations share one Rust profile reconciliation boundary; installed dependencies, enabled Host Bundles, generic client-only packages, and market-disabled packages remain distinct.
- The Legacy Host loads only declared bundles in manifest order and exposes settled Loader entry/Fiber inventory, so dshmarket reports `live`, restart-required, disabled, and absent states from runtime facts.
- Tessivum pins the published `tessivum-core v0.1.6` seed-preserving compatibility release at `3571b75dd79bdcf658d8ad6b86da63005431b21e`.

### Fixed

- Side Chat persists the plugin's exact closed seed, cold-loads durable child snapshots without making them resident, and completes generation-owned child disposal before a replacement generation starts.
- Every `SessionPersistence` implementation must atomically commit seeded headers and events; JSONL, SQLite, and memory backends satisfy the contract without partial sessions.
- Long dshmarket pnpm operations use the bounded market bridge limit without weakening other bridge payload limits.
- Legacy heartbeat, cancellation, crash, restart, and shutdown cleanup leave no stale generation registrations, routes, Loader entries, or Fibers.

## 0.1.0-alpha.14 - 2026-08-28

### Fixed

- Legacy Node heartbeats bypass serialized plugin operations, preventing long plugin loads from being mistaken for a disconnected compatibility host.
- Generation-owned HTTP routes are removed before process reaping, preventing stale Browser requests from repeatedly reaching a closed bridge.

## 0.1.0-alpha.13 - 2026-08-27

### Added

- Native Agent Mode registry with built-in Standard, PTC, Minimal, and Composition modes plus strict user `mode.toml` bundles under the Tessivum data root.
- Session-owned mode selection persisted as `agentMode`, with restore-time migration from legacy built-in `agentPreset` values.
- Programmatic Tool Calling mode backed by a bounded Bun subprocess and one model-facing `run_code` tool.
- Typed, session-isolated `composition_inspect`, `composition_define`, `composition_validate`, `composition_run`, and `composition_stop` tools for Native, WASM, and Legacy entries.
- Browser Agent Mode picker, settings, authoring, and session-label surfaces over the frozen `agentPreset.*` compatibility wire.
- Ordered `web --patch <file>` profile overlays; `agent-presets.default` selects the Host default mode after custom modes are loaded.

### Changed

- Standard, PTC, Minimal, Composition, and custom modes now resolve immutable prompt, tool, Skill, planning, compaction, and plugin policies per session.
- Browser slots and settings section IDs use `conversation.hero.agentMode` and `agent-modes`; `agentPreset.*`, `agent-preset/selected`, and the `agent-presets` settings namespace remain wire-only compatibility names.
- Built-in Standard and PTC modes expose only native tools present in the Host; strict custom modes and Minimal still fail when a required tool or runtime is unavailable.
- Legacy sessions without a persisted mode now materialize the boot default exactly once before runtime resolution; later Settings changes cannot retroactively change them.
- Composition runtime IDs are session-namespaced, and failed plugin or scope cleanup retains ownership for an explicit retry instead of reporting a false stop.

### Removed

- `TESSIVUM_TOOLS_MODE`, `TESSIVUM_CORDIS_TOOLS`, `with_cordis_tools`, and the model-visible `cordis_inspect_self`, `cordis_define`, `cordis_run`, and `cordis_stop` path.
- Arbitrary Host/Client JavaScript execution from the dynamic Cordis runtime. Frozen `dynamicCordisRunner/*` Browser routes now expose only bounded disabled compatibility responses.
- Duplicate Agent Preset runtime, UI, and obsolete Cordis tool-round test paths.

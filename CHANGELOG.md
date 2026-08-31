# Changelog

## Unreleased

### Added

- Frozen compatibility coverage for `dsh-dream-skin@8.30.1`, including its Host persistence route and Browser client bundle.

### Changed

- Legacy web routes are restricted to the supported `/dsh-market`, `/sidebar`, and `/dream-skin` namespaces.
- Tessivum pins the `tessivum-core v0.1.6` route-policy revision at `7150b20df296e52403de00f36fdc1dd9bf93edde`.

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

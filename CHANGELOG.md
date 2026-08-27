# Changelog

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

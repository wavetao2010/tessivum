# Windows Cargo resource bound design

This design limits Windows Cargo build concurrency in CI and native source
acceptance. It prevents the observed all-target test build from exhausting
the Windows commit limit while preserving the existing test surface.

## Context

The first fresh local-source run at revision `504ecb2` passed Groups 01-11.
Group 12, `cargo test --all-targets --locked`, then failed with Cargo exit 101.
Windows recorded multiple concurrent `rustc.exe` processes using about 2.1 to
2.6 GB each, two virtual-memory exhaustion events, and two matching
`rustc_driver` crashes with exception `0xc0000409`.

A post-merge experiment using `--jobs 1` did not reproduce the concurrent
rustc crash. Its first linker failure was separately traced to insufficient C:
free space. Moving the generated Cargo target to a spacious NTFS volume let
the same `compaction` integration-test binary link successfully with normal
test debug information.

The collaborator's Alpha.25 update does not change Windows CI commands,
`docs/WINDOWS_SOURCE_TEST.md`, Cargo concurrency, or rustc memory policy.

## Goals

Every Cargo command that can compile or link code in the Windows CI job and
native Windows source procedure must use one build job explicitly. Fetch-only
commands do not need a jobs argument.

The change must provide these outcomes:

- Windows Cargo check, Clippy, tests, WASM builds, targeted regression tests,
  Agent smoke runs, and the release Web run use `--jobs 1`.
- Linux, macOS, browser-only, and package release jobs remain unchanged.
- Existing test selection, locked dependency behavior, fail-fast semantics,
  and command output assertions remain unchanged.
- Independent Node and Python source contracts reject an unbounded Windows
  Cargo command or a misleading comment/decoy.

## Non-goals

This change does not reduce test coverage, disable debug information, change
Rust source logic, alter Cargo.lock, or configure a global Cargo target or
package cache. It does not hard-code a machine-specific drive or free-space
threshold. Local acceptance still requires an NTFS work root with enough free
space for generated dependencies and Rust artifacts.

## Command policy

Use Cargo's command-specific `--jobs 1` option so each command remains readable
and auditable. The Windows CI commands become forms such as:

```text
cargo check --jobs 1 --all-targets --locked
cargo clippy --jobs 1 --all-targets --locked -- -D warnings
cargo test --jobs 1 --all-targets --locked --no-fail-fast
```

Apply the same option to the Windows CI WASM build and both targeted test
commands. Keep `cargo fetch --locked` unchanged because it does not compile or
link targets.

In `docs/WINDOWS_SOURCE_TEST.md`, apply `--jobs 1` to Groups 10-12, both Agent
commands in Groups 14-15, and the Group 16 release Web command. This keeps the
full source and runtime path under test while bounding compiler concurrency.

## Source contracts

Extend `scripts/check-windows-node-unicode-fs.test.mjs` and
`scripts/check_compat_baseline.py` with equivalent fail-closed checks. Each
checker must isolate the Windows job and the executable PowerShell blocks in
the source guide, enumerate actual Cargo command lines, and require the exact
`--jobs 1` forms before any Cargo `--` argument separator.

Both the Node and Python checks must independently reject each of these
mutations:

- removing `--jobs 1` from one Windows CI Cargo command;
- changing the value to `--jobs 2` or a larger job count;
- replacing the exact form with a non-exact spelling such as `-j 1` or
  `--jobs=1`;
- leaving an unbounded Group 12 command in the source guide;
- placing the required command only in another workflow job;
- placing the required command only in a YAML or PowerShell comment;
- placing the required source-guide command only in a non-executable Markdown
  fence such as `text`.

A decoy cannot compensate for a missing, unbounded, or malformed command in the
actual Windows job or executable PowerShell block. Each mutation must assert
the expected failure from the checker under test, not merely any failure.

The source contract supplements execution evidence. It does not claim that a
text match proves memory behavior.

## TDD sequence

Implementation follows these red-green cycles:

1. Add a Node source-contract assertion for all compiling Windows CI Cargo
   commands and observe it fail on the first unbounded command.
2. Add the equivalent Python baseline assertion and observe the same RED.
3. Add source-guide assertions for Groups 10-12, 14-15, and 16 and observe
   their unbounded-command failures.
4. Add the full focused mutation matrix for a removed bound, larger value,
   non-exact spelling, other-job decoy, comment decoy, and non-executable-fence
   decoy in both contracts.
5. Update only the Windows workflow and source guide commands.
6. Run both Node runtimes, the exact-source Python baseline, and PowerShell AST
   validation.
7. Run the affected Rust tests and the complete Windows acceptance from a
   fresh Chinese-and-space NTFS path on a volume with adequate capacity.

## Files

The expected implementation surface is:

- Modify `.github/workflows/ci.yml` only within `jobs.windows`.
- Modify `docs/WINDOWS_SOURCE_TEST.md` only in Windows Cargo commands and the
  surrounding resource requirement text.
- Modify `scripts/check-windows-node-unicode-fs.test.mjs` for the independent
  Node source contract.
- Modify `scripts/check_compat_baseline.py` for the independent Python source
  contract.

No Rust source, lockfile, external dependency source, global configuration, or
preserved acceptance artifact is changed by this design.

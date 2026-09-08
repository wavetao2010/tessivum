# Windows Node Unicode filesystem guard design

This design prevents Tessivum's Windows source acceptance run from entering
pnpm with a Node runtime that cannot remove files below a non-ASCII path. It
also makes the Windows CI runtime explicit and preserves the required
ordinary-user, Developer Mode disabled, Chinese-and-space NTFS coverage.

## Context

The Windows source acceptance run for
`02c5987e716729edd235cbd90c834eba24065419` failed in Group 01 while pnpm
11.7.0 renamed an imported package directory:

```text
esbuild_tmp_13744_16 -> esbuild
EPERM: operation not permitted, rename
```

The failure happened in the pinned DeepSeek Harness workspace before any
Tessivum runtime, Web, Market, PowerShell executor, Job Object, or ACL sandbox
code ran. The retained failure state contains eight build-required packages
whose stage directories contain all files while their final directories retain
only empty directory skeletons.

Node issue `nodejs/node#61067` documents a Windows regression where Node 24
silently fails to remove files below a Chinese-named directory. Node PR
`nodejs/node#61108` fixes the defect and first appears in Node 24.13.1 LTS.

A controlled local comparison established the causal boundary:

- Node 24.11.1 removes and renames the test tree in an ASCII-and-space path.
- Node 24.11.1 returns from `fs.rmSync` without an error in the corresponding
  Chinese-and-space path, but leaves the target present; `fs.renameSync` then
  returns `EPERM`.
- Verified portable Node 24.20.0 completes both operations in both paths.

## Goals

The change must prevent the known runtime defect from being misreported as a
pnpm or esbuild package failure. It must make the Windows Node runtime
reproducible, verify the actual filesystem capability, and keep the frozen
DeepSeek commit, pnpm version, security settings, and Unicode path requirements
unchanged.

The change must provide the following outcomes:

- Accept supported Node LTS runtimes that implement correct non-ASCII Windows
  removal semantics.
- Reject an affected runtime before cloning or installing external
  dependencies.
- Emit an actionable failure that includes the Node version, platform, failed
  operation, and upstream Node issue.
- Leave no probe directory after either success or failure.
- Pin Windows CI to Node 24.20.0 and execute the same probe used locally.
- Preserve Group 01 fail-fast behavior and require a fresh clean install after
  the runtime prerequisite passes.

## Non-goals

This change does not patch pnpm, edit the pinned DeepSeek Harness checkout,
clean stale `node_modules`, retry failed installations, disable security
software, elevate privileges, or weaken the Chinese-path acceptance condition.
It does not claim to fix independent antivirus handle contention or concurrent
pnpm installs.

The change does not alter Node requirements for Linux or macOS jobs unless the
existing workflow needs an explicit action ordering adjustment. The defect and
the acceptance requirement are Windows-specific.

## Runtime policy

Windows source acceptance supports Node 22 LTS at version 22.19.0 or later and
Node 24 LTS at version 24.13.1 or later. The development and Windows CI
reference runtime is Node 24.20.0.

The behavioral probe is authoritative. A runtime inside a nominally supported
range still fails the prerequisite if it cannot complete the filesystem
operations. Conversely, the acceptance procedure does not use an undocumented
version bypass to skip the probe.

The local retest uses the official `node-v24.20.0-win-x64.zip` through
process-local `PATH` precedence. Its archive SHA-256 is:

```text
6CAC9FFBCA8F6A47091E4B5C772E0606049C3871CB67D900C0CEDDE630E545BA
```

This does not replace the installed Node runtime or modify global PATH,
registry, pnpm store, or proxy configuration.

## Components

The implementation introduces one dependency-free Node module and one
dependency-free test module. It also connects the probe to CI, the compatibility
baseline, and the Windows acceptance procedure.

### Runtime probe

Create `scripts/check-windows-node-unicode-fs.mjs`. The module exports a
function that accepts an optional parent directory for tests and implements a
CLI entry point for normal use.

The probe performs these operations:

1. Create a unique directory below a path component containing Chinese
   characters and a space.
2. Create a populated `target` directory and a populated sibling stage
   directory whose name follows pnpm's `<name>_tmp_<pid>_<id>` shape.
3. Call `fs.rmSync(target, { recursive: true, force: true })` once.
4. Check that `target` no longer exists. Treat a silent no-op as a failure.
5. Rename the stage directory to `target` once and check that the stage no
   longer exists.
6. Remove the full probe tree before returning.

The probe must not retry either operation. Retrying could hide the behavior the
acceptance gate exists to detect.

If recursive removal fails or silently leaves the target, cleanup uses a small
bottom-up fallback based on `readdirSync`, `unlinkSync`, and `rmdirSync`. The
fallback exists only to remove probe artifacts. It does not convert a failed
probe into a pass.

The CLI exits 0 and prints one concise success line when every assertion and
cleanup succeeds. It exits non-zero with a message containing the following
facts when the capability check fails:

- `process.version` and `process.platform`;
- the operation that failed or returned an invalid state;
- the tested path;
- the supported Windows Node LTS ranges;
- `https://github.com/nodejs/node/issues/61067`.

If fallback cleanup also fails, the error preserves both the primary probe
failure and the cleanup failure. The process never reports success when cleanup
is incomplete.

### Probe tests

Create `scripts/check-windows-node-unicode-fs.test.mjs` using `node:test` and
`node:assert/strict`. Tests use real temporary directories and the exported
probe function. They do not mock filesystem operations.

The test module covers these contracts:

- A capable runtime completes removal and rename below a Chinese-and-space
  path and leaves no probe directory.
- The CLI succeeds and prints the current Node version on a capable runtime.
- The source acceptance document names the supported LTS floors and invokes
  the probe before dependency installation.
- The Windows CI job pins Node 24.20.0, runs the tests, and runs the probe
  before the DeepSeek pnpm install.

The local TDD cycle also runs the CLI under Node 24.11.1 as an expected-failure
integration check. It must emit the targeted diagnostic and leave no probe
directory. That execution is evidence for the rejection path, not a passing
test-suite result.

### Windows CI

Modify `.github/workflows/ci.yml` only in the `windows` job. Add a commit-pinned
`actions/setup-node` step for Node 24.20.0 before `pnpm/action-setup`. Add a
runtime prerequisite step before the compatibility baselines and before the
DeepSeek frozen install:

```powershell
node --test scripts/check-windows-node-unicode-fs.test.mjs
node scripts/check-windows-node-unicode-fs.mjs
```

The job continues to pin pnpm 11.7.0 and Bun 1.4.0. The probe creates its own
Unicode descendant, so it exercises the relevant behavior even though the
GitHub workspace path is normally ASCII.

### Compatibility baseline

Extend `scripts/check_compat_baseline.py` to assert that the Windows workflow
contains the Node 24.20.0 pin and invokes both the Node test and runtime probe
before the pinned DeepSeek installation step. The check prevents a future CI
edit from silently dropping the prerequisite.

The baseline check remains a source invariant. It does not replace execution
of the Node test or the runtime probe.

### Windows acceptance procedure

Update `docs/WINDOWS_SOURCE_TEST.md` to state the supported Windows Node LTS
ranges and the Node 24.20.0 reference runtime. Record Node's resolved executable
path and version as before.

After the exact candidate checkout is verified and before external dependency
clones or pnpm installation, run:

```powershell
node scripts/check-windows-node-unicode-fs.mjs
```

A non-zero result is an environment prerequisite failure. Preserve its output,
mark Groups 01-16 `NOT RUN`, and do not reinterpret it as an esbuild package
failure. If the probe succeeds, continue to the existing clean Group 01
installation without cleanup, retries, or configuration changes.

The procedure also replaces the Developer Mode registry lookup with a read-only
query that represents a missing value as disabled without throwing inside the
global stop-on-error scope. This corrects the independently observed environment
gate failure without changing the registry.

## Data flow

The acceptance flow becomes:

```text
record tools and versions
        |
verify exact Tessivum candidate
        |
run Node Unicode filesystem probe
        |
        +-- failure --> preserve output; Groups 01-16 NOT RUN
        |
        +-- success --> clone pinned dependencies
                         |
                         --> Group 01 frozen pnpm install
                               |
                               --> execute installed esbuild binaries
                               |
                               --> second frozen install
                               |
                               --> Groups 02-16
```

CI follows the same runtime prerequisite before its external dependency build.
The compatibility baseline separately checks that this ordering remains in the
workflow source.

## TDD sequence

Implementation follows these red-green cycles:

1. Add the Node test importing the not-yet-created probe and observe
   `ERR_MODULE_NOT_FOUND` under portable Node 24.20.0.
2. Implement the minimal probe and observe the real-filesystem test pass under
   Node 24.20.0.
3. Add CI and documentation contract assertions and observe them fail because
   the runtime pin and invocations are absent.
4. Modify CI, the compatibility baseline, and the acceptance document until
   those assertions pass.
5. Run the CLI under Node 24.11.1 and verify the expected targeted rejection and
   complete cleanup.
6. Run the CLI and test module under Node 24.20.0 and verify success.
7. Run `python scripts/check_compat_baseline.py` with the exact pinned source
   checkouts and verify the extended invariant.

No production implementation is written before its failing test is observed.

## Acceptance and regression

After focused tests pass, create a fresh checkout in a Chinese-and-space NTFS
path and use only the verified portable Node 24.20.0 through process-local PATH.
Run the candidate procedure from the beginning with fresh evidence.

Group 01 passes only when all of these commands complete:

- the first clean `pnpm install --frozen-lockfile`;
- every installed Windows esbuild binary with `--version`;
- the second installed-state `pnpm install --frozen-lockfile`.

Only a passing Group 01 authorizes Groups 02-16. Every later group retains the
existing fail-fast semantics. The final report records exact commands, exit
codes, resolved tools, new evidence, process cleanup, and any environment
deviation. Previous results are not reused as proof.

## Files

The expected implementation surface is:

- Create `scripts/check-windows-node-unicode-fs.mjs`.
- Create `scripts/check-windows-node-unicode-fs.test.mjs`.
- Modify `.github/workflows/ci.yml`.
- Modify `scripts/check_compat_baseline.py`.
- Modify `docs/WINDOWS_SOURCE_TEST.md`.
- Update `docs/WINDOWS_SOURCE_TEST_RESULT_02C5987.md` only after a fresh run,
  while clearly separating the original candidate result from the development
  branch result.

No product runtime module, pinned dependency source, lockfile, global tool
configuration, or preserved failure artifact is modified by this design.

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

Windows source acceptance supports exactly these Node LTS ranges:

- `>=22.19.0 <23.0.0`;
- `>=24.13.1 <25.0.0`.

The development and Windows CI reference runtime is Node 24.20.0. Odd-numbered
and future major versions are rejected until they are explicitly validated and
the policy is updated. A numerically newer version does not gain implicit
support.

The version policy and behavioral probe are both mandatory gates. A runtime in
a supported range still fails if it cannot complete the filesystem operations.
A runtime outside the supported ranges fails even if the filesystem probe
would pass. There is no undocumented version or behavior bypass.

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

Create `scripts/check-windows-node-unicode-fs.mjs`. The module exports a pure
version-range predicate, a filesystem probe that accepts an optional parent
directory for tests, and a CLI entry point for normal use. The version parser
uses `process.versions.node` and no package dependency.

The probe performs these operations:

1. Reject a Node version outside `>=22.19.0 <23.0.0` and
   `>=24.13.1 <25.0.0`.
2. Create a unique directory below a path component containing Chinese
   characters and a space.
3. Create a populated `target` directory and a populated sibling stage
   directory whose name follows pnpm's `<name>_tmp_<pid>_<id>` shape.
4. Call `fs.rmSync(target, { recursive: true, force: true })` once.
5. Check that `target` no longer exists. Treat a silent no-op as a failure.
6. Rename the stage directory to `target` once.
7. Verify that the stage no longer exists and the target contains the expected
   root file, `bin` file, and `lib` file with their original contents.
8. Remove the full probe tree before returning.

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
- both supported Windows Node LTS ranges;
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
- The version predicate accepts the inclusive lower bounds, accepts later
  patch/minor releases within Node 22 and 24, and rejects versions immediately
  below each lower bound.
- The version predicate rejects Node 20, Node 23, Node 25, and an unlisted
  future major version even when their numeric versions exceed a lower bound.
- The rename result contains every expected file with its original contents.
- The source acceptance document names the supported LTS floors and invokes
  the probe before dependency installation.
- The Windows CI job orders primary checkout, pinned Node setup, tests, and the
  probe before external checkouts, pnpm setup, and the DeepSeek install.

The local TDD cycle also runs the CLI under Node 24.11.1 as an expected-failure
integration check. The CLI must reject its version before filesystem mutation,
emit the targeted diagnostic, and leave no probe directory. A separate direct
call to the exported filesystem probe under Node 24.11.1 must reproduce the
silent removal failure, report it, and complete fallback cleanup. Those
executions are rejection-path evidence, not passing test-suite results.

### Windows CI

Modify `.github/workflows/ci.yml` only in the `windows` job. Its relevant steps
must have this exact order:

1. Check out the primary Tessivum repository.
2. Run
   `actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38` with
   `node-version: 24.20.0`.
3. Run the Node tests and runtime probe.
4. Check out the three pinned external repositories.
5. Set up Rust, Bun, and pnpm 11.7.0.
6. Run compatibility gates and the existing build and test steps.

The runtime prerequisite step runs:

```powershell
node --test scripts/check-windows-node-unicode-fs.test.mjs
node scripts/check-windows-node-unicode-fs.mjs
```

The setup-node commit is the immutable commit currently referenced by the v6
tag. The job continues to pin pnpm 11.7.0 and Bun 1.4.0. The probe creates its
own Unicode descendant, so it exercises the relevant behavior even though the
GitHub workspace path is normally ASCII.

### Compatibility baseline

Extend `scripts/check_compat_baseline.py` to isolate the `windows` job and
assert the ordered occurrence of primary checkout, the immutable setup-node
commit, Node 24.20.0, the Node test, the runtime probe, the first external
checkout, pnpm setup, and the pinned DeepSeek installation. The check prevents
a future CI edit from moving pnpm or external dependency work ahead of the
runtime prerequisite.

The baseline check remains a source invariant. It does not replace execution
of the Node test or the runtime probe.

### Windows acceptance procedure

Update `docs/WINDOWS_SOURCE_TEST.md` to state both supported Windows Node LTS
ranges and the Node 24.20.0 reference runtime. The procedure must use this
order:

1. Record the environment and resolve the Node executable without invoking
   pnpm.
2. Clone and verify the exact primary Tessivum candidate.
3. Invoke `node --version`, enforce the supported range, and run the runtime
   probe from the primary checkout.
4. Clone and verify the three pinned external dependencies.
5. Invoke and verify pnpm 11.7.0, then continue to Group 01.

The runtime probe command is:

```powershell
node scripts/check-windows-node-unicode-fs.mjs
```

A version rejection or non-zero probe result is an environment prerequisite
failure. Preserve its output, mark Groups 01-16 `NOT RUN`, and do not invoke
pnpm or reinterpret it as an esbuild package failure. If both gates succeed,
continue to the existing clean Group 01 installation without cleanup, retries,
or configuration changes.

The procedure also replaces the Developer Mode registry lookup with a read-only
query that represents a missing value as disabled without throwing inside the
global stop-on-error scope. This corrects the independently observed environment
gate failure without changing the registry.

## Data flow

The acceptance flow becomes:

```text
record environment and resolve Node
        |
verify exact Tessivum candidate
        |
enforce Node LTS range
        |
run Node Unicode filesystem probe
        |
        +-- failure --> preserve output; Groups 01-16 NOT RUN
        |
        +-- success --> clone pinned dependencies
                         |
                         --> verify pnpm 11.7.0
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
2. Implement and test the version predicate, including exact range boundaries
   and rejection of unlisted major versions.
3. Implement the minimal filesystem probe and observe the real-filesystem test
   pass under Node 24.20.0.
4. Add CI and documentation contract assertions and observe them fail because
   the runtime pin and invocations are absent.
5. Modify CI, the compatibility baseline, and the acceptance document until
   those assertions pass.
6. Run the CLI under Node 24.11.1 and verify version rejection before mutation.
7. Call the filesystem probe directly under Node 24.11.1 and verify the known
   behavior failure plus fallback cleanup.
8. Run the CLI and test module under Node 24.20.0 and verify success.
9. Run `python scripts/check_compat_baseline.py` with the exact pinned source
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

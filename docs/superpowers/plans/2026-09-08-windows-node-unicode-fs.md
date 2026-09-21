# Windows Node Unicode filesystem guard implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reject unsupported Windows Node runtimes before external checkout or
pnpm work, verify real non-ASCII removal and rename behavior, and rerun all 16
Windows source acceptance groups with fresh evidence.

**Architecture:** A dependency-free Node module owns the explicit LTS version
policy and a real filesystem capability probe. Node tests exercise policy,
filesystem behavior, CLI output, and source ordering; the existing Python
compatibility baseline independently guards CI order. Windows CI and the local
acceptance procedure run both gates before external dependencies or pnpm.

**Tech Stack:** Node.js 24.20.0 `node:test`, Node core filesystem/process APIs,
Python 3, GitHub Actions YAML, PowerShell 7.4+, pnpm 11.7.0, Bun 1.4.0, Rust.

---

## File map

The implementation keeps the executable behavior in one small module and uses
existing repository verification and documentation surfaces for integration.

- Create `scripts/check-windows-node-unicode-fs.mjs`: version predicate,
  artifact-only fallback cleanup, filesystem probe, CLI gate, and diagnostics.
- Create `scripts/check-windows-node-unicode-fs.test.mjs`: version boundaries,
  real filesystem behavior, CLI behavior, and CI/document source contracts.
- Modify `.github/workflows/ci.yml:116`: reorder only the Windows job and pin
  Node 24.20.0 through the immutable `actions/setup-node` commit.
- Modify `scripts/check_compat_baseline.py:83`: assert the exact Windows CI
  prerequisite order.
- Modify `docs/WINDOWS_SOURCE_TEST.md:19`: document and execute the supported
  Node ranges before external checkout or pnpm, and make Developer Mode lookup
  safe when the registry value is absent.
- Modify `docs/WINDOWS_SOURCE_TEST_RESULT_02C5987.md`: append only fresh branch
  evidence after the rerun; retain the original candidate failure record.

## Fixed inputs

All tasks use these exact inputs. Do not update them while implementing the
guard.

```text
Baseline candidate: 02c5987e716729edd235cbd90c834eba24065419
Development base: 8a1986c1eabf7b317441da9f7467dd15e1907c1a
Development branch: codex/fix-windows-node-unicode-fs
Portable Node:
  C:\Users\Q\Documents\New project\.tools\node-v24.20.0-win-x64\node.exe
Affected system Node: D:\Program Files\nodejs\node.exe (24.11.1)
DeepSeek: 47f943859bef60e4160492346772ded9b24f765a
Cordis: 8cc9e33fab69e2d0476d126baaf2acb24e6a6ab4
tessivum-core: 86c7e1c71bd99a3c0fc70e7be6f251c89f2cc694
pnpm: 11.7.0
Bun: 1.4.0
```

Do not modify the preserved failure checkout at
`C:\Users\Q\Documents\New project\测试 项目\tessivum-02c5987`.

## Subagent execution gates

The controller executes Tasks 1-3 and the Task 6 report change with one fresh
implementation subagent per repository-changing task. After each implementation
commit, the controller dispatches a fresh specification-compliance reviewer,
resolves every finding, and re-reviews until approved. Only then does it
dispatch a fresh code-quality reviewer, resolve Critical and Important
findings, and re-review until approved.

The controller independently inspects the commit and reruns its verification
before advancing. Every completed implementation or review subagent is
immediately reclaimed. Any repository fix after review repeats both gates.
Implementation subagents run sequentially because they share this worktree; do
not dispatch them in parallel.

### Task 1: Add the explicit Windows Node support policy

**Files:**

- Create: `scripts/check-windows-node-unicode-fs.test.mjs`
- Create: `scripts/check-windows-node-unicode-fs.mjs`

- [ ] **Step 1: Write the import and version-policy tests first**

Create the test module with `node:test` and `node:assert/strict`. Import the
not-yet-created guard module once as an ESM namespace, `import * as guard`, so
later missing exports fail inside their individual tests instead of preventing
the complete test module from executing. Define table-driven tests with these
exact expectations:

```javascript
const versionCases = [
  ['22.18.99', false],
  ['22.19.0', true],
  ['22.19.1', true],
  ['22.99.99', true],
  ['23.0.0', false],
  ['24.13.0', false],
  ['24.13.1', true],
  ['24.20.0', true],
  ['24.99.99', true],
  ['25.0.0', false],
  ['20.99.99', false],
  ['26.0.0', false],
  ['99.0.0', false],
  ['24.13', false],
  ['not-a-version', false],
];
```

Each case calls `guard.isSupportedWindowsNodeVersion`. Include a separate
assertion that the
explicit reference version `24.20.0` is accepted. Do not assume the runtime
executing the test is supported because Task 2 also runs under Node 24.11.1.

- [ ] **Step 2: Run the test and verify the missing-module red state**

Run:

```powershell
$Node = 'C:\Users\Q\Documents\New project\.tools\node-v24.20.0-win-x64\node.exe'
& $Node --test scripts/check-windows-node-unicode-fs.test.mjs
```

Expected: exit 1 with `ERR_MODULE_NOT_FOUND` for
`scripts/check-windows-node-unicode-fs.mjs`. Record this exact output before
creating the implementation file.

- [ ] **Step 3: Implement only the version predicate**

Create `scripts/check-windows-node-unicode-fs.mjs` with these public constants
and function contract:

```javascript
export const SUPPORTED_NODE_RANGES = Object.freeze([
  '>=22.19.0 <23.0.0',
  '>=24.13.1 <25.0.0',
]);

export function isSupportedWindowsNodeVersion(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(version);
  if (match === null) return false;
  const [, majorText, minorText, patchText] = match;
  const major = Number(majorText);
  const minor = Number(minorText);
  const patch = Number(patchText);
  if (major === 22) return minor > 19 || (minor === 19 && patch >= 0);
  if (major === 24) return minor > 13 || (minor === 13 && patch >= 1);
  return false;
}
```

Do not accept a leading `v`, prerelease text, odd major, or unlisted future
major. The predicate is policy, not a generic semantic-version library.

- [ ] **Step 4: Run the version tests and verify green**

Run the same portable Node command. Expected: all current tests pass, with no
warnings and exit 0.

- [ ] **Step 5: Review and commit Task 1**

Run:

```powershell
git diff --check
git status --short
git add scripts/check-windows-node-unicode-fs.mjs `
  scripts/check-windows-node-unicode-fs.test.mjs
git commit -m "feat: define supported Windows Node runtimes"
```

Expected: only the two Node files are committed.

After the commit, complete the specification-compliance and code-quality gates
defined above before starting Task 2.

### Task 2: Add version diagnostics, the filesystem probe, and CLI

**Files:**

- Modify: `scripts/check-windows-node-unicode-fs.test.mjs`
- Modify: `scripts/check-windows-node-unicode-fs.mjs`

- [ ] **Step 1: Write failing rejection and stdin-import tests**

Use `guard.assertSupportedWindowsNodeVersion` from the existing namespace
import. Add a test that passes `24.11.1` and `win32`, catches the error, and
asserts its message
contains the version, platform, operation `version-policy`, both supported
ranges, and `https://github.com/nodejs/node/issues/61067`.

Add a child-process test that runs the system Node 24.11.1 executable only when
it exists and has that exact version. It executes the module as a CLI and
asserts non-zero exit plus the same diagnostic facts. Record a recursive
inventory of `tmpdir()` entries matching the probe's `测试 路径-*` ownership
prefix before and after and assert that version rejection changed no files. On
environments without exact Node 24.11.1, skip only this integration assertion;
the pure diagnostic test remains mandatory.

Add a stdin-import child test. It must exit 0 without executing the CLI or
producing output:

```powershell
'import "./scripts/check-windows-node-unicode-fs.mjs"' |
  & $Node --input-type=module
```

- [ ] **Step 2: Run both runtimes and verify rejection red**

Run the test module with portable Node and exact system Node 24.11.1. Expected:
both runs exit 1 because `assertSupportedWindowsNodeVersion` is not exported.
The failure must be missing behavior, not a syntax or path error.

- [ ] **Step 3: Implement rejection diagnostics and a guarded CLI shell**

Implement `assertSupportedWindowsNodeVersion(version, platform)` using the pure
predicate and required diagnostic facts. Add the CLI entry shell and guard
stdin imports exactly:

```javascript
const isDirectExecution = process.argv[1] !== undefined
  && pathToFileURL(process.argv[1]).href === import.meta.url;
```

The CLI calls the assertion before any filesystem mutation. On an unsupported
version, it writes one actionable diagnostic to stderr and sets non-zero exit
status. It must not invoke the filesystem probe on this path. Re-run the pure,
stdin-import, and Node 24.11.1 rejection tests. Expected: they pass. The
supported portable CLI can remain red until the probe exists.

- [ ] **Step 4: Write failing capable-runtime probe tests**

Use `guard.probeWindowsNodeUnicodeFilesystem` from the namespace import. Use
`mkdtempSync(join(tmpdir(), 'tessivum-node-probe-test-'))` as a test-owned
parent. Mark capable-runtime probe and CLI-success tests skipped when the pure
predicate rejects `process.versions.node`. Add tests that:

1. Call the probe with `{ parentDirectory }`.
2. Assert the result records `process.version`, `process.platform`, the exact
   tested path, the unique probe root, and its owned Unicode parent.
3. Assert the probe root and owned Unicode parent do not exist, while the
   caller-owned `parentDirectory` still exists.
4. Assert the probe reports validation of these exact contents:

```text
package.json: {"name":"probe-stage"}\n
bin/esbuild.exe: probe-binary\n
lib/main.js: export const probe = true;\n
```

5. Spawn portable Node with the module path and assert exit 0, empty stderr,
   and one stdout line containing `process.version`, `process.platform`, and
   `Windows Node Unicode filesystem probe passed`.
6. Remove the test-owned parent in `afterEach` and assert cleanup completed.

The tests use real files and directories. Do not mock `rmSync` or
`renameSync`.

- [ ] **Step 5: Run and verify the missing-probe red state**

Run the portable Node tests. Expected: exit 1 because
`probeWindowsNodeUnicodeFilesystem` is not exported. Confirm this exact red
state before implementation.

- [ ] **Step 6: Implement probe construction and one-shot operations**

Implement the exported function with this contract:

```javascript
export function probeWindowsNodeUnicodeFilesystem({
  parentDirectory = tmpdir(),
} = {})
```

Create a unique, probe-owned parent named `测试 路径-<uuid>` directly below the
caller-owned `parentDirectory`, then create the probe root below that owned
parent. Within the root, create a populated `esbuild` target and a sibling
named `esbuild_tmp_<pid>_<uuid>`. Populate the stage with the exact three files
used by the tests. Then:

1. Call `rmSync(target, { recursive: true, force: true })` once.
2. Fail if `existsSync(target)` is still true.
3. Call `renameSync(stage, target)` once.
4. Fail if the stage still exists or target is missing.
5. Read and compare all three target files byte-for-byte with expected text.
6. Remove the complete owned Unicode parent before returning a small facts
   object; never remove the caller-owned `parentDirectory`.

Do not retry removal or rename. First use only normal recursive cleanup for the
probe root; do not add fallback cleanup yet.

- [ ] **Step 7: Run the capable-runtime green state**

Run the complete tests and CLI with portable Node 24.20.0. Expected: all tests
pass, CLI exits 0, the probe root and owned Unicode parent are absent, and the
caller-owned parent remains.

- [ ] **Step 8: Add and run the affected-runtime cleanup red test**

Under exact Node 24.11.1, call the exported probe with a unique test-owned
parent. Mark this affected-runtime test skipped unless
`process.versions.node === '24.11.1'` and `process.platform === 'win32'`. Assert
that it throws a structured failure identifying operation
`remove-target`, the exact tested path, and the silent no-op state. Also assert
the error preserves `cleanupComplete: true`, that its exact probe root and
owned Unicode parent are absent, and that the caller-owned parent remains.

Run this test under Node 24.11.1 before fallback cleanup exists. Expected: exit
1 because recursive cleanup silently leaves the tree or because required
cleanup facts are absent. After recording red output, use a PowerShell
bottom-up deletion only on the exact test-owned parent if needed. A different
import, syntax, rename, or unrelated failure is not the expected red state.

- [ ] **Step 9: Implement artifact-only fallback cleanup**

Add a private bottom-up cleanup helper based on `readdirSync(..., {
withFileTypes: true })`, `unlinkSync`, and `rmdirSync`. Use it only if the
normal recursive cleanup fails or silently leaves the probe-owned Unicode
parent. Clean every directory the probe created, but never delete
`parentDirectory`.

Preserve the primary operation error. If fallback cleanup also fails, throw an
`AggregateError` containing both failures. A fallback cleanup success must not
turn a failed removal, rename, or content check into a passing probe. The
structured primary error records its operation, exact paths, and
`cleanupComplete`, so the affected-runtime test cannot accept another error.

- [ ] **Step 10: Verify affected cleanup green, then add formatter red**

Run the affected-runtime test under exact Node 24.11.1. Expected: it now passes
for operation `remove-target`, preserves owned paths and cleanup facts, removes
only the owned Unicode parent, and leaves the caller-owned parent.

Then add a separate test calling the not-yet-implemented
`guard.formatWindowsNodeUnicodeFilesystemError`. Pass it a structured
capability failure and assert the result includes `process.version`,
`process.platform`, operation, tested path, both supported ranges, Node issue
61067, primary error details, and cleanup or `AggregateError` details.

Run the formatter test under portable Node. Expected: the individual test
fails because the namespace property is not a function; all preceding tests,
including the real affected-runtime cleanup test in its Node 24.11.1 run, can
still execute. An ESM instantiation or import error is not the expected red.

- [ ] **Step 11: Implement capability-failure formatting**

Implement the tested formatter and route the CLI capability-failure catch path
through it without discarding primary or cleanup details. Re-run the formatter
test and verify green.

- [ ] **Step 12: Run both runtime suites and cleanup verification**

Run:

```powershell
& $Node --test scripts/check-windows-node-unicode-fs.test.mjs
& $Node scripts/check-windows-node-unicode-fs.mjs
& $OldNode --test scripts/check-windows-node-unicode-fs.test.mjs
```

Expected: portable tests and CLI exit 0. Under exact Node 24.11.1, capable-only
tests skip, rejection and affected-runtime cleanup tests pass, and the suite
exits 0. Each test asserts absence using its exact probe root and owned Unicode
parent, and asserts the caller-owned parent remains; do not substitute a
shallow `$env:TEMP` scan.

- [ ] **Step 13: Review and commit Task 2**

Run `git diff --check`, inspect the complete diff, and commit:

```powershell
git add scripts/check-windows-node-unicode-fs.mjs `
  scripts/check-windows-node-unicode-fs.test.mjs
git commit -m "feat: probe Windows Unicode filesystem behavior"
```

After the commit, complete the specification-compliance and code-quality gates
defined above before starting Task 3.

### Task 3: Enforce prerequisite ordering in CI and local acceptance

**Files:**

- Modify: `scripts/check-windows-node-unicode-fs.test.mjs`
- Modify: `.github/workflows/ci.yml:116`
- Modify: `scripts/check_compat_baseline.py:83`
- Modify: `docs/WINDOWS_SOURCE_TEST.md:19`

- [ ] **Step 1: Add source-contract tests before source changes**

Read source files relative to the repository root derived from
`import.meta.url`. Isolate the Windows job between `\n  windows:` and
`\n  browser-e2e:`. Assert strictly increasing positions for:

```text
actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09
actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38
node-version: 24.20.0
node --test scripts/check-windows-node-unicode-fs.test.mjs
node scripts/check-windows-node-unicode-fs.mjs
repository: deepseek-ai/deepseek-harness
pnpm/action-setup@b906affcce14559ad1aafd4ab0e942779e9f58b1
Install pinned DeepSeek build dependencies
```

Also assert the acceptance document contains both range strings and that its
main PowerShell block orders these tokens strictly:

```text
Get-Command node
git clone https://github.com/wavetao2010/tessivum.git
node --version
node scripts/check-windows-node-unicode-fs.mjs
git clone https://github.com/deepseek-ai/deepseek-harness.git
pnpm --version
pnpm install --frozen-lockfile
```

- [ ] **Step 2: Run and verify the source-contract red state**

Run the portable Node tests. Expected: the new CI/document ordering tests fail
because setup-node and the probe are absent and pnpm/external checkout currently
occur too early.

- [ ] **Step 3: Add the Python compatibility invariant before CI changes**

Add a helper that accepts a source string and ordered tokens, reports missing
tokens through the existing `check()` path, and compares token positions. In
`main()`, isolate only the Windows job and assert this sequence:

```python
WINDOWS_CI_ORDER = (
    "actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09",
    "actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
    "node-version: 24.20.0",
    "node --test scripts/check-windows-node-unicode-fs.test.mjs",
    "node scripts/check-windows-node-unicode-fs.mjs",
    "repository: deepseek-ai/deepseek-harness",
    "pnpm/action-setup@b906affcce14559ad1aafd4ab0e942779e9f58b1",
    "Install pinned DeepSeek build dependencies",
)
```

Run the baseline against the preserved pinned dependency checkouts:

```powershell
$env:TESSIVUM_DEEPSEEK_SOURCE = `
  'C:\Users\Q\Documents\New project\测试 项目\tessivum-02c5987\.ci\deepseek-harness'
$env:TESSIVUM_CORDIS_SOURCE = `
  'C:\Users\Q\Documents\New project\测试 项目\tessivum-02c5987\.ci\cordis'
$env:TESSIVUM_CORE_SOURCE = `
  'C:\Users\Q\Documents\New project\测试 项目\tessivum-02c5987\.ci\tessivum-core'
python scripts/check_compat_baseline.py
```

Expected: exit 1 with the new Windows Node prerequisite ordering failure. Do
not modify the preserved dependency checkouts.

- [ ] **Step 4: Reorder only the Windows CI job**

Keep the primary checkout first. Immediately add:

```yaml
      - uses: actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38 # v6
        with:
          node-version: 24.20.0

      - name: Verify Windows Node filesystem prerequisite
        run: |
          node --test scripts/check-windows-node-unicode-fs.test.mjs
          node scripts/check-windows-node-unicode-fs.mjs
```

Move the three external checkout steps after this prerequisite. Keep Rust,
cache, Bun 1.4.0, and pnpm 11.7.0 setup after external checkouts. Do not change
the `verify` or `browser-e2e` jobs.

- [ ] **Step 5: Correct and reorder the acceptance procedure**

Document exactly the two supported ranges and Node 24.20.0 reference runtime.
In the PowerShell block:

1. Resolve environment commands without resolving or invoking pnpm.
2. Query Developer Mode with `Get-ItemProperty` plus a property-presence check
   so a missing registry value becomes disabled and cannot terminate under
   `$ErrorActionPreference = 'Stop'`.
3. Clone and verify the exact Tessivum candidate.
4. Invoke `node --version`, enforce the approved ranges in PowerShell, and run
   `node scripts/check-windows-node-unicode-fs.mjs` from `$Repo`.
5. On either failure, let fail-fast preserve output and leave every group
   `NOT RUN`.
6. Only then clone the three external dependencies, resolve pnpm, verify
   pnpm 11.7.0, and continue Group 01.

Do not change Bun, Rust, dependency SHAs, security posture, Group 01 retry
rules, or Groups 02-16 semantics.

- [ ] **Step 6: Run the integrated green checks**

Run:

```powershell
& $Node --test scripts/check-windows-node-unicode-fs.test.mjs
& $Node scripts/check-windows-node-unicode-fs.mjs
python scripts/check_compat_baseline.py
git diff --check
```

Expected: every command exits 0. The Python command prints `compat baseline
OK` using the pinned environment variables from Step 3.

- [ ] **Step 7: Review documentation and commit Task 3**

Verify edited Markdown uses clear requirement language, every new heading has
introductory text, non-URL lines stay within 80 characters where surrounding
style permits, and commands match implementation. Commit:

```powershell
git add .github/workflows/ci.yml scripts/check_compat_baseline.py `
  scripts/check-windows-node-unicode-fs.test.mjs `
  docs/WINDOWS_SOURCE_TEST.md
git commit -m "ci: gate Windows dependencies on Node filesystem support"
```

After the commit, complete the specification-compliance and code-quality gates
defined above before starting Task 4.

### Task 4: Verify the affected and reference Node runtimes

**Files:**

- No repository file changes.
- Append results to external `progress.md` and fresh evidence logs only.

- [ ] **Step 1: Re-verify the portable archive before accepting evidence**

Run:

```powershell
$Archive = 'C:\Users\Q\Documents\New project\.tools\node-v24.20.0-win-x64.zip'
$ExpectedHash = `
  '6CAC9FFBCA8F6A47091E4B5C772E0606049C3871CB67D900C0CEDDE630E545BA'
$ActualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $Archive).Hash
if ($ActualHash -ne $ExpectedHash) { throw "Unexpected Node hash $ActualHash" }
```

Expected: exit 0 and an exact case-insensitive hash match. Do not accept later
Node 24.20.0 or Groups 01-16 evidence if this gate fails.

- [ ] **Step 2: Verify CLI rejection under system Node 24.11.1**

Run the system executable by absolute path:

```powershell
$OldNode = 'D:\Program Files\nodejs\node.exe'
& $OldNode scripts/check-windows-node-unicode-fs.mjs
```

Expected: exit non-zero before filesystem mutation. Output includes 24.11.1,
`win32`, `version-policy`, both supported ranges, and Node issue 61067.

- [ ] **Step 3: Verify the raw filesystem failure and fallback cleanup**

Invoke the exported filesystem probe directly, bypassing only the CLI version
policy for diagnostic evidence:

```powershell
@'
import { probeWindowsNodeUnicodeFilesystem } from
  './scripts/check-windows-node-unicode-fs.mjs';
try {
  probeWindowsNodeUnicodeFilesystem({ parentDirectory: process.env.TEMP });
  process.exitCode = 2;
} catch (error) {
  console.error(error);
  const expected = error.operation === 'remove-target'
    && error.cleanupComplete === true;
  process.exitCode = expected ? 0 : 3;
}
'@ | & $OldNode --input-type=module
```

Expected: the direct probe reports operation `remove-target`, the exact Chinese
test path, the silent no-op state, and `cleanupComplete: true`. The wrapper
exits 0 only for that structured failure. The affected-runtime test separately
verifies the exact probe root and test-owned Chinese parent are absent. Import,
syntax, cleanup, rename, or unrelated errors exit 3. Unexpected success exits
2 and requires investigation rather than rewriting expectations.

- [ ] **Step 4: Verify reference runtime success**

Run the complete Node test and CLI with portable Node 24.20.0. Expected: all
tests pass, CLI exits 0, and no probe artifacts remain.

- [ ] **Step 5: Re-run compatibility and diff checks**

Run the Python baseline with exact pinned source variables, `git diff --check`,
and `git status --short --branch`. Expected: clean tracked worktree after the
three implementation commits.

- [ ] **Step 6: Complete full implementation review before acceptance**

Dispatch a fresh final implementation reviewer over `3ebef67..HEAD`. Resolve
every Critical and Important implementation finding, repeat the per-task
specification-compliance then code-quality gates for each repository change,
and reclaim every reviewer immediately. Re-run Steps 1-5 after any fix.

Freeze the resulting implementation commit as `$TestedRevision`. Tasks 5 and 6
must test and report this exact commit. Do not begin native acceptance while an
implementation review finding remains open.

### Task 5: Run Groups 01-15 fail-fast in a fresh Chinese-and-space checkout

**Files:**

- Create outside repository: fresh checkout and timestamped evidence directory.
- No repository file changes; preserve result facts in the evidence directory.

- [ ] **Step 1: Establish process-local Node 24.20.0 precedence**

Start a new PowerShell process with only the portable Node directory prepended
to `PATH`. Do not modify machine or user PATH. Verify `Get-Command node` and
`node --version` resolve to the verified portable archive and `v24.20.0`.

- [ ] **Step 2: Run the updated source acceptance script from the beginning**

Use a new timestamped evidence directory and a fresh repository path below a
Chinese-and-space NTFS parent. Test the development branch commit, not the
preserved candidate checkout. Preserve every command, resolved tool, commit,
and exit code.

- [ ] **Step 3: Enforce Group 01 fail-fast semantics**

Group 01 passes only if the first frozen pnpm install, every installed Windows
esbuild executable, and the second frozen install all exit 0. Do not clean,
retry, change the pnpm store, or continue after any failure.

- [ ] **Step 4: Continue Groups 02-15 only after Group 01 passes**

Run the document exactly. On the first non-zero result, stop, preserve state,
mark every later group `NOT RUN`, and begin systematic diagnosis before any
fix. If all pass, retain the Agent round-trip and session-prefix evidence.

- [ ] **Step 5: Prepare an evidence-backed result manifest**

Write a plain evidence manifest outside the repository containing the exact
development commit, environment, portable Node archive hash, every group
status, raw evidence paths, and deviations. Do not edit the report in this
task, and do not report unrun groups as passing.

### Task 6: Run Group 16, finalize the report, and verify the branch

**Files:**

- Modify: `docs/WINDOWS_SOURCE_TEST_RESULT_02C5987.md`

- [ ] **Step 1: Gate Group 16 on Groups 01-15**

Read the same run's result manifest. Run Group 16 only if Groups 01-15 all
passed against the exact development commit. If an earlier group failed or was
`NOT RUN`, record Group 16 as `NOT RUN` and continue directly to Step 5 so the
failure report can still be finalized and committed.

- [ ] **Step 2: Run the release Web server from the same fresh checkout**

Use the same process-local Node environment, exact development commit, evidence
root, and ordinary-user session as Groups 01-15. Never substitute the original
`02c5987` candidate. Record the process baseline before starting
`cargo run --release -- web`.

- [ ] **Step 3: Verify HTTP and browser rendering before cancellation**

Require HTTP 200 at `http://127.0.0.1:3000/`, retain the response body, and save
a browser screenshot under the evidence root. A listening port without a
rendered page is not sufficient.

- [ ] **Step 4: Send one Ctrl+C and verify cleanup**

Record product exit 130, closed port 3000, and no new tessivum, Bun, Cargo,
PowerShell, or related child process relative to the baseline.

- [ ] **Step 5: Finish the Markdown report from evidence only**

Keep the original `02c5987` result intact. Add a clearly labeled development
branch rerun section using only the manifest and raw evidence. If Group 16 ran,
record HTTP, screenshot, Ctrl+C, port, and process results. If an earlier group
failed, record Group 16 as `NOT RUN` and explain the fail-fast boundary.
Reconcile summary counts with the per-group table. Keep every failure and
`NOT RUN` entry visible. Run documentation self-review and link/path checks.

- [ ] **Step 6: Run fresh final verification**

Run at minimum:

```powershell
& $Node --test scripts/check-windows-node-unicode-fs.test.mjs
& $Node scripts/check-windows-node-unicode-fs.mjs
python scripts/check_compat_baseline.py
git diff --check
git status --short --branch
```

Also rerun every focused or wider suite affected by any post-review fix. Read
the complete output and count failures before making a completion claim.

- [ ] **Step 7: Commit the evidence-backed report**

Commit only after its statements match fresh logs:

```powershell
git add docs/WINDOWS_SOURCE_TEST_RESULT_02C5987.md
git commit -m "docs: report Windows Node guard acceptance results"
```

- [ ] **Step 8: Complete report and final branch reviews**

Run the Task 6 report commit through the same specification-compliance then
code-quality gates used by Tasks 1-3. Re-review every report-only fix and
reclaim each reviewer immediately. Then dispatch one final branch reviewer to
verify that the report names `$TestedRevision`, all statements match evidence,
and commits after `$TestedRevision` change documentation only.

If any review proposes an implementation, CI, executable acceptance-procedure,
or test change after `$TestedRevision`, do not patch it while retaining old
evidence. Return to Task 4, create and review a new `$TestedRevision`, rerun
Tasks 5-6 from a fresh checkout, reconcile the report, and repeat both gates.

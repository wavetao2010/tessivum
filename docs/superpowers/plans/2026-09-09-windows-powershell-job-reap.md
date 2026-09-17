# Windows PowerShell escaped-tree cleanup implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reap PowerShell descendants that escape Tessivum's Windows Job before
one-shot and persistent shell operations publish completion.

**Architecture:** Keep Job Object termination for processes that remain in the
Job, and add a Windows-native Toolhelp ancestry tracker for descendants that
escape it. Own the root and each retained process as a handle plus
`(PID, creation time)` generation identity. Validate each parent edge before
retaining descendants, then consume one bounded fixed-point cleanup fence before
stream and lifecycle completion.

**Tech Stack:** Rust 2024, Tokio, `windows-sys` 0.61, Windows Job Objects,
Toolhelp process snapshots, and native Windows integration tests.

---

## Scope and invariants

This repair changes only the Windows PowerShell ownership boundary. Preserve
the following constraints throughout implementation and review.

- Keep Unix behavior byte-for-byte unchanged.
- Do not change `Cargo.lock`, dependency declarations, or fixed dependency
  sources. The required `windows-sys` features already exist.
- Do not extend the existing one-second `assert_reaped` observation window.
- Do not invoke `taskkill` or another external process-tree utility.
- Do not stage or commit `.ci/`.
- Do not use `git checkout --` to remove rejected experiments. Use
  `apply_patch`, inspect the resulting diff, and preserve unrelated work.
- Do not modify production code until all three behavior tests fail for the
  expected process-not-reaped reason on the restored production baseline.
- Do not modify the current Task 1 production code until both PID-reuse
  regressions and the deterministic termination-race regression produce the
  required RED evidence against `4d9f745` in merged source `da0a4d0`.
- Use `--jobs 1` for every compiling or linking Cargo command in this plan.
- Commit each implementation task separately. After each implementation
  commit, run specification review first and code-quality review second. Fix
  every Critical or Important finding and repeat the same review before moving
  on.
- Do not push this branch before the later fresh Groups 01-16 verification.

The current correction target is merged Alpha.27 source
`da0a4d09a60280a745910c9d4de4c85b5210c9d5`. It contains the behavior-test
commit `291348c` and the initial Task 1 production commit `4d9f745`. Task 0
below records the established behavior-first history. Do not repeat its
baseline restoration, editing, or commit steps against the current merged
source. Later tasks explicitly identify which behavior tests to rerun. The new
Task 1 RED steps target the current implementation.

## File map

The implementation stays within the current process-ownership boundary.

- Modify `tests/builtin_tools.rs` to finish the deterministic normal-exit RED
  and remove temporary Job-membership diagnostics from existing tests.
- Modify `src/subprocess.rs` to own exact Windows process generations, discover
  descendants, implement capture and fixed-point cleanup, and integrate the
  persistent reaper request path.
- Modify `src/builtin_tools.rs` to integrate capture and cleanup into one-shot
  PowerShell completion and cancellation.
- Modify lower-level Windows process tests only when a deterministic native or
  pure ancestry behavior cannot be covered through `tests/builtin_tools.rs`.

## Task 0: Preserve the established behavior RED baseline

This task records the RED work that landed in `291348c`. It removed rejected
production experiments and established valid behavior RED evidence before the
initial Task 1 implementation. Keep these steps as provenance; do not rerun
them against `da0a4d0` and expect the original process-not-reaped failures.

**Files:**

- Modify: `src/subprocess.rs`
- Modify: `src/builtin_tools.rs`
- Modify: `tests/builtin_tools.rs`
- Test: `tests/builtin_tools.rs`

- [ ] **Step 1: Audit the uncommitted experiment boundary**

Run:

```powershell
git status --short --branch
git diff --check
git diff -- src/subprocess.rs src/builtin_tools.rs tests/builtin_tools.rs
```

Expected: only rejected PowerShell experiments and the diagnostic test draft
appear in these files. `.ci/` remains untracked and unstaged.

- [ ] **Step 2: Remove rejected production experiments with `apply_patch`**

Patch only the experiment hunks:

1. Remove `CREATE_BREAKAWAY_FROM_JOB` from `WindowsJob::spawn` and restore the
   sole `CREATE_SUSPENDED` creation flag.
2. Remove the experimental `WindowsJob::terminate_and_wait` method.
3. Replace both one-shot `terminate_and_wait().await` branches with the
   upstream `job.terminate()` behavior.

Run:

```powershell
git diff -- src/subprocess.rs src/builtin_tools.rs
```

Expected: both production files have no uncommitted diff. Do not use a Git
restore or checkout command, because it could discard unrelated work.

- [ ] **Step 3: Restore the existing cancellation test**

Remove the temporary `OpenProcess` and `IsProcessInJob` diagnostic block from
`powershell_cancellation_reaps_its_descendant_tree`. Preserve its original
launch, cancellation, `CANCELLED` assertion, and unchanged `assert_reaped` call.

- [ ] **Step 4: Restore the existing persistent lifecycle test**

Inspect
`persistent_powershell_cancel_disable_and_shutdown_reap_process_trees` and
remove any temporary Job-membership or probe-only assertions. Preserve its
cancel, disable, shutdown, PID capture, and unchanged reap assertions.

- [ ] **Step 5: Finish the normal-completion P/Invoke regression**

Keep `powershell_normal_completion_reaps_its_descendant_tree`, but make the C#
helper and Rust assertions express the proven escaped-child contract:

1. Call `IsProcessInJob(GetCurrentProcess(), NULL, ...)` and require the root
   PowerShell process to be in a Job.
2. call `CreateProcessW` with `CREATE_SUSPENDED | CREATE_NO_WINDOW`, no
   breakaway flag, and `bInheritHandles = false`.
3. Query the child handle before it executes and require the child to be
   outside every Job.
4. Call `ResumeThread`, then return the PID and membership facts.
5. Keep the 30-second command and call the unchanged `assert_reaped(pid)` after
   normal tool completion.

Add a test-owned Rust RAII guard immediately after parsing the PID. Open the
process once with query, synchronize, and terminate rights, and own the handle
with `std::os::windows::io::OwnedHandle`. In `Drop`, if a zero-time wait reports
`WAIT_TIMEOUT`, call `TerminateProcess`, then call `WaitForSingleObject` with a
bounded wait. This guard exists only to clean the intentional leak after a
panic or failed assertion; it must not make the production assertion pass.

The guard must remain live through `assert_reaped`. Do not close the only test
handle before membership validation, `ResumeThread`, or cleanup ownership is
established.

- [ ] **Step 6: Verify the normal-completion test is a valid RED**

Run:

```powershell
$env:CARGO_TARGET_DIR = 'D:\Tessivum 验证缓存\02f061f-tree-cleanup'
cargo test --jobs 1 --locked --test builtin_tools `
  powershell_normal_completion_reaps_its_descendant_tree `
  -- --exact --nocapture --test-threads=1
```

Expected: C# compilation and `CreateProcessW` succeed, root membership is
`True`, child membership is `False`, the child is resumed, and Cargo exits 101
only because unchanged `assert_reaped` reports that PID still active. A helper
compile error, membership mismatch, parse error, timeout, or natural 30-second
exit is not valid RED evidence.

- [ ] **Step 7: Verify the existing one-shot cancellation test is RED**

Run:

```powershell
cargo test --jobs 1 --locked --test builtin_tools `
  powershell_cancellation_reaps_its_descendant_tree `
  -- --exact --nocapture --test-threads=1
```

Expected: Cargo exits 101 only at the unchanged process-not-reaped assertion.

- [ ] **Step 8: Verify the existing persistent lifecycle test is RED**

Run:

```powershell
cargo test --jobs 1 --locked --test builtin_tools `
  persistent_powershell_cancel_disable_and_shutdown_reap_process_trees `
  -- --exact --nocapture --test-threads=1
```

Expected: Cargo exits 101 only at an existing process-not-reaped assertion.
Record which lifecycle boundary failed. Do not implement until all three REDs
are valid.

- [ ] **Step 9: Commit only the regression-test cleanup**

Run:

```powershell
cargo fmt --all --check
git diff --check
git diff -- src/subprocess.rs src/builtin_tools.rs tests/builtin_tools.rs
git add -- tests/builtin_tools.rs
git diff --cached --check
git diff --cached --name-only
git commit -m "test: cover escaped PowerShell descendants"
```

Expected: the staged set contains only `tests/builtin_tools.rs`; the production
files are clean and `.ci/` is absent. Dispatch a specification reviewer, reclaim
it, then dispatch a quality reviewer and reclaim it. Resolve and re-review all
Critical or Important findings before Task 1.

## Task 1: Harden Windows process-generation ownership

This task corrects the native identity, discovery, capture, and consuming
cleanup primitives currently present in `4d9f745`. It does not change one-shot
or persistent caller ordering. Microsoft documents that a
[process handle remains valid after termination][process-handles], but the PID
is valid only until termination and can then be reused. The implementation must
therefore use [`GetProcessTimes`][get-process-times] creation and exit times to
distinguish process generations.

**Files:**

- Modify: `src/subprocess.rs`
- Test: `src/subprocess.rs` Windows-only unit tests or an existing lower-level
  Windows process test when integration coverage is required

- [ ] **Step 1: Add and run the retained-anchor reuse RED**

Before editing production code, add a pure synthetic ancestry regression for
the retained-anchor PID-reuse hole in `4d9f745`. Model an exited retained
anchor with one PID, creation time, and exit time; reuse the same PID for a
later process generation; and give that replacement a child created after the
historical anchor exited. The current PID-only helper accepts the child through
the reused anchor.

The test must assert that generation-aware evaluation rejects the child. It
must also assert that the replacement generation itself is absent from the
retained-generation result and the termination-target result. Neither the
replacement nor its child may become a termination target.

Run:

```powershell
cargo test --jobs 1 --locked --lib `
  subprocess::windows_process_tree_tests::rejects_reused_retained_anchor `
  -- --exact --nocapture --test-threads=1
```

Expected against `4d9f745` as merged in `da0a4d0`: Cargo exits 101 at the new
assertion because PID-only ancestry incorrectly accepts the replacement's
child or conflates the replacement with the retained anchor. A setup, unrelated
compile, or unrelated assertion failure is not valid RED evidence. Record the
output before changing production code.

- [ ] **Step 2: Add and run the root reuse RED**

Still without editing production code, add a second pure synthetic regression
for root PID reuse. Model an exited exact root generation, a later replacement
with the same PID, and a child created by that replacement after the original
root exited. The test must reject the replacement's child and must assert that
the replacement itself appears in neither the retained-generation result nor
the termination-target result.

Run:

```powershell
cargo test --jobs 1 --locked --lib `
  subprocess::windows_process_tree_tests::rejects_reused_root `
  -- --exact --nocapture --test-threads=1
```

Expected against the same current production source: Cargo exits 101 at the
new assertion because PID-only ancestry treats the reused root PID as the old
root and accepts the replacement's child. Record this RED separately. Do not
edit production code between the retained-anchor and root-reuse RED runs.

- [ ] **Step 3: Add and run the termination-normalization RED**

Still without editing production code, add a deterministic pure test for the
[`TerminateProcess`][terminate-process] failure decision. Cover these cases:

- Suppress `ERROR_ACCESS_DENIED` only when an immediate zero-time wait on the
  same handle reports signaled.
- Preserve the original termination error when that handle is unsignaled.
- Preserve the original termination error when the immediate wait fails.
- Preserve a non-`ERROR_ACCESS_DENIED` termination error.

Run the exact new test against the same current production source:

```powershell
cargo test --jobs 1 --locked --lib `
  subprocess::windows_process_tree_tests::normalizes_terminate_failure `
  -- --exact --nocapture --test-threads=1
```

Expected: Cargo exits 101 for the specific missing normalization helper or new
assertion. Because a compile failure can prevent other unit tests from running,
retain both PID-reuse assertion RED outputs separately. Do not modify production
until all three RED reasons have been reviewed and accepted.

- [ ] **Step 4: Introduce exact RAII generation state**

Replace the tuple `WindowsJob` representation with a private `Arc`-owned state
that holds:

- the Job as `OwnedHandle` or an equivalent standard RAII owner;
- the owned root generation: PID, `GetProcessTimes` creation timestamp, and a
  duplicated `OwnedHandle`;
- a mutex-protected collection of validated descendant generations, keyed by
  `(PID, creation time)` and owning one handle per generation; and
- the first capture error, which later errors cannot overwrite.

Do not add a blanket `unsafe impl Send` or `unsafe impl Sync`. The owner must be
movable into `spawn_blocking` through standard handle ownership.

- [ ] **Step 5: Own the exact root generation during assignment**

In `assign_raw`, synchronously duplicate the supplied process handle into an
owned handle before returning. Query the duplicate with `GetProcessId` and
`GetProcessTimes`, and store its PID and creation timestamp as the root
generation. Return the operating-system error if duplication, PID lookup, or
time lookup fails. `spawn` and `spawn_std` must use this same assignment path.
The root identity must not borrow a `Child` handle or rely on `Child::id()`.

- [ ] **Step 6: Implement parent-first generation validation**

Use `CreateToolhelp32Snapshot`, `Process32FirstW`, and `Process32NextW` to build
PID-to-parent discovery hints. A snapshot parent PID is never an identity or
termination authority. Validate candidates parent-first as follows:

1. Open it with only the query, synchronize, and terminate rights needed.
2. Keep the handle open.
3. Verify `GetProcessId` and read the candidate's creation time with
   `GetProcessTimes`.
4. Take a fresh snapshot after the open and require its immediate parent PID to
   name the exact root or a previously validated retained generation.
5. If the parent handle is live, require parent creation time to be no later
   than child creation time.
6. If the parent handle is signaled, query its exit time and also require child
   creation time to be no later than parent exit time.
7. Retain the exact candidate generation before considering its descendants.

If a current process reuses an anchor PID, its creation time differs. Do not
treat it as the old anchor, and do not terminate it. Missing timing data,
inconsistent ordering, disappearance during validation, or identity ambiguity
is a cleanup error. Retain validated Job members as anchors as well as escaped
descendants.

- [ ] **Step 7: Correct asynchronous early capture**

Add `capture_and_terminate(&self)`. It must clone the private `Arc` into
`tokio::task::spawn_blocking`, perform one parent-first
discovery/open/revalidation pass, store newly validated generations, preserve
the first error, and only then call fast `TerminateJobObject`. Repeated calls
must be idempotent and may add newly discovered generations.

The method performs no wait or fixed-point loop. It returns the capture error to
the caller, but callers must still wait for the root and run consuming cleanup
before surfacing that error.

- [ ] **Step 8: Correct the consuming fixed-point fence**

Add one consuming async cleanup method that moves the owner into
`spawn_blocking`. The blocking operation creates one 10-second `Instant`
deadline at entry and never restarts it. Until a complete fixed point, it must:

1. discover candidate descendants from the exact root and retained anchors;
2. open and fresh-snapshot validate every new generation parent-first;
3. call `TerminateJobObject` again;
4. terminate every active retained generation through its owned handle;
5. wait on retained handles only for the remaining shared deadline; and
6. repeat discovery until a full validation pass adds no generation.

Success requires all retained handles signaled, Job accounting reporting zero
active processes, and one final validation snapshot adding no generation.
Fresh passes can discover children created after an earlier snapshot. Retaining
each parent first lets its handle remain a historical anchor after exit;
creation and exit times then validate only children created during that exact
parent's lifetime.

Snapshot failure, a process that cannot be opened, missing or inconsistent
timing data, failed wait, a blocking-task panic, or deadline expiry returns
`std::io::Error`. When `TerminateProcess` fails, immediately perform a zero-time
wait on the same owned handle. Suppress only `ERROR_ACCESS_DENIED` when that
wait reports signaled. If the handle is unsignaled or the wait fails, preserve
the termination error. Return the stored first capture error after completing
the best available fence.

- [ ] **Step 9: Keep synchronous termination and Drop fast**

Keep `terminate(&self)` limited to one best-effort `TerminateJobObject` call.
`Drop` may invoke only that operation. Neither may scan, wait, take the capture
mutex, panic, or overwrite a stored error.

- [ ] **Step 10: Make the generation tests GREEN and run lower-level tests**

Run:

```powershell
cargo test --jobs 1 --locked --lib windows_process_tree `
  -- --nocapture --test-threads=1
cargo test --jobs 1 --locked --test windows_process `
  -- --test-threads=1
```

Expected: both commands exit 0. The three higher-level PowerShell tests may
remain RED because caller integration is intentionally not part of this task.

- [ ] **Step 11: Commit and review the Task 1 hardening**

Run:

```powershell
cargo fmt --all --check
git diff --check
git add -- src/subprocess.rs
git diff --cached --check
git diff --cached --name-only
git commit -m "fix: harden Windows process generation ownership"
```

Expected: the staged set contains only `src/subprocess.rs`, including its
Windows-only tests. Create a normal follow-up commit at the current merged
`HEAD`. Preserve merge `da0a4d0`, initial Task 1 commit `4d9f745`, and upstream
Alpha.27 publication commit `f28d8fe`; do not rewrite existing history.

Dispatch the Task 1 specification reviewer and reclaim it. Only after approval,
dispatch the quality reviewer and reclaim it. Fix and re-review every Critical
or Important finding in follow-up commits.

## Task 2: Integrate one-shot PowerShell cleanup

This task uses the normal-completion and cancellation REDs to integrate early
capture and the consuming fence into both one-shot return paths.

**Files:**

- Modify: `src/builtin_tools.rs`
- Modify only if the primitive contract requires it: `src/subprocess.rs`
- Test: `tests/builtin_tools.rs`

- [ ] **Step 1: Reconfirm both one-shot tests are RED**

Run each exact Task 0 command again. Expected: both exit 101 only at unchanged
process-not-reaped assertions. Stop if either fails for another reason.

- [ ] **Step 2: Integrate cancellation cleanup**

When cancellation wins while the root is live:

1. await `capture_and_terminate` before losing the live ancestry opportunity;
2. request and await root child termination using the existing behavior;
3. always await the consuming fixed-point fence;
4. on fence success, join both stream-copy tasks and preserve current kill/wait
   error ordering; and
5. return `CANCELLED` only after successful process-tree and stream cleanup.

If capture or the consuming fence fails, abort and await both stream-copy tasks,
then return `bash_error("could not reap PowerShell process tree", error)`.
Cleanup failure takes precedence over the original success or cancellation
result.

- [ ] **Step 3: Integrate normal completion cleanup**

After the root wait, run the consuming fixed-point fence before joining stream
tasks or publishing output. A normal root may already have exited, so this path
uses the owned historical root generation, identities retained during available
capture opportunities, and the final snapshot rules. On fence failure, abort
and await both stream tasks and surface the same specific cleanup error.

- [ ] **Step 4: Make both one-shot tests GREEN**

Run:

```powershell
cargo test --jobs 1 --locked --test builtin_tools `
  powershell_normal_completion_reaps_its_descendant_tree `
  -- --exact --nocapture --test-threads=1
cargo test --jobs 1 --locked --test builtin_tools `
  powershell_cancellation_reaps_its_descendant_tree `
  -- --exact --nocapture --test-threads=1
```

Expected: both commands exit 0, and the descendant is absent within the
unchanged assertion window.

- [ ] **Step 5: Commit and review Task 2**

Run:

```powershell
cargo fmt --all --check
git diff --check
git add -- src/builtin_tools.rs src/subprocess.rs
git diff --cached --check
git commit -m "fix: reap one-shot PowerShell process trees"
```

Stage `src/subprocess.rs` only if Task 2 legitimately changes it. Dispatch and
reclaim the specification reviewer, then dispatch and reclaim the quality
reviewer. Resolve and re-review every Critical or Important finding.

## Task 3: Integrate persistent cleanup requests

This task makes the persistent reaper capture escaped descendants while the
root is still alive and preserves unconditional lifecycle completion.

**Files:**

- Modify: `src/subprocess.rs`
- Test: `tests/builtin_tools.rs`

- [ ] **Step 1: Reconfirm the persistent lifecycle test is RED**

Run the exact persistent command from Task 0. Expected: exit 101 only at an
existing process-not-reaped assertion.

- [ ] **Step 2: Add a persistent cleanup-request notification**

Add a notification owned by `PersistentShellInner`. `stop`, timeout,
cancellation, disposal, and last-owner `Drop` must publish their existing first
cause and signal the request without scanning or waiting in those paths.

In the reaper, use a biased selection that prefers an already-ready cleanup
request over root exit. When the request wins while the root is alive, await
`capture_and_terminate` before waiting for the root.

- [ ] **Step 3: Add the stable cleanup error**

Add a Windows-only `PERSISTENT_SHELL_CLEANUP` error constructor whose message
identifies failed PowerShell process-tree cleanup and whose data contains the
native error string. Do not change `ProcessDone`, `disable`, `shutdown`,
`cancel`, or `dispose` return types.

- [ ] **Step 4: Build one unconditional reaper completion tail**

After root exit, take the Job owner and await the consuming fence before stream
drainers. On success, await both drainers and publish the existing generic
closed-shell error only if no earlier command result won.

On capture, fence, drainer, or blocking-task failure:

1. publish `PERSISTENT_SHELL_CLEANUP` through `fail_active` before any generic
   closed-shell error;
2. preserve cancellation, timeout, disposal, or stream errors that already won;
3. abort and await both drainers when a failed fence cannot guarantee pipe EOF;
4. convert Tokio `JoinError`, including blocking-task panic, to cleanup error;
   and
5. reach one shared tail that calls `inner.complete(ProcessDone { ... })`
   exactly once.

No active command means the cleanup error remains internal best effort because
the public lifecycle APIs return `()`. Completion waiters must still wake.

- [ ] **Step 5: Make the persistent test GREEN and protect one-shot behavior**

Run:

```powershell
cargo test --jobs 1 --locked --test builtin_tools `
  persistent_powershell_cancel_disable_and_shutdown_reap_process_trees `
  -- --exact --nocapture --test-threads=1
cargo test --jobs 1 --locked --test builtin_tools `
  powershell_normal_completion_reaps_its_descendant_tree `
  -- --exact --nocapture --test-threads=1
cargo test --jobs 1 --locked --test builtin_tools `
  powershell_cancellation_reaps_its_descendant_tree `
  -- --exact --nocapture --test-threads=1
```

Expected: all three exact commands exit 0 with the unchanged reap window.

- [ ] **Step 6: Run the affected integration binary both ways**

Run:

```powershell
cargo test --jobs 1 --locked --test builtin_tools -- --test-threads=1
cargo test --jobs 1 --locked --test builtin_tools
```

Expected: both commands exit 0 with zero failures and no new warnings.

- [ ] **Step 7: Commit and review Task 3**

Run:

```powershell
cargo fmt --all --check
git diff --check
git add -- src/subprocess.rs
git diff --cached --check
git commit -m "fix: reap persistent PowerShell process trees"
```

Dispatch and reclaim the Task 3 specification reviewer. After approval,
dispatch and reclaim the quality reviewer. Fix and re-review every Critical or
Important finding.

## Task 4: Verify the complete PowerShell repair

This task produces fresh evidence for the complete Rust boundary before the
separate Cargo resource-bound plan begins.

**Files:**

- Verify: `src/subprocess.rs`
- Verify: `src/builtin_tools.rs`
- Verify: `tests/builtin_tools.rs`

- [ ] **Step 1: Run formatting and static checks**

Run:

```powershell
cargo fmt --all --check
cargo check --jobs 1 --all-targets --locked
cargo clippy --jobs 1 --all-targets --locked -- -D warnings
```

Expected: every command exits 0 with no warnings from Clippy.

- [ ] **Step 2: Run all focused Windows tests**

Run the three exact Task 3 commands, followed by:

```powershell
cargo test --jobs 1 --locked --test builtin_tools -- --test-threads=1
cargo test --jobs 1 --locked --test builtin_tools
cargo test --jobs 1 --locked --test windows_process -- --test-threads=1
```

Expected: every command exits 0 with zero failing tests.

- [ ] **Step 3: Run the full Rust suite with bounded compilation**

Run:

```powershell
$env:CARGO_TARGET_DIR = 'D:\Tessivum 验证缓存\02f061f-tree-cleanup'
cargo test --jobs 1 --all-targets --locked
```

Expected: Cargo exits 0 with zero failing tests. Preserve complete output for
the later Markdown report.

- [ ] **Step 4: Run final implementation review**

Dispatch one read-only reviewer across the Task 0 base SHA through Task 3 head
SHA. Require it to check the approved design line by line, inspect all unsafe
Windows calls and handle lifetimes, validate timeout and error precedence, and
report Critical, Important, and Minor findings with file and line references.
Reclaim it immediately. Resolve and re-review every Critical or Important
finding, then rerun every affected command.

- [ ] **Step 5: Freeze the PowerShell repair boundary**

Run:

```powershell
git status --short --branch
git diff --check
git log --oneline --decorate -8
```

Expected: only explicitly preserved untracked `.ci/` or later planned work may
remain. Do not push; proceed to the separately approved Windows Cargo
resource-bound plan.

[process-handles]: https://learn.microsoft.com/en-us/windows/win32/procthread/process-handles-and-identifiers
[get-process-times]: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesstimes
[terminate-process]: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess

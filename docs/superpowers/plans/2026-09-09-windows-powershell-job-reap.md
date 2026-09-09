# Windows PowerShell escaped-tree cleanup implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reap PowerShell descendants that escape Tessivum's Windows Job before
one-shot and persistent shell operations publish completion.

**Architecture:** Keep Job Object termination for processes that remain in the
Job, and add a Windows-native Toolhelp ancestry tracker for descendants that
escape it. Capture stable process handles while the root is alive, then consume
one bounded fixed-point cleanup fence before stream and lifecycle completion.

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
- Use `--jobs 1` for every compiling or linking Cargo command in this plan.
- Commit each implementation task separately. After each implementation
  commit, run specification review first and code-quality review second. Fix
  every Critical or Important finding and repeat the same review before moving
  on.
- Do not push this branch before the later fresh Groups 01-16 verification.

## File map

The implementation stays within the current process-ownership boundary.

- Modify `tests/builtin_tools.rs` to finish the deterministic normal-exit RED
  and remove temporary Job-membership diagnostics from existing tests.
- Modify `src/subprocess.rs` to own stable Windows process identities, discover
  descendants, implement capture and fixed-point cleanup, and integrate the
  persistent reaper request path.
- Modify `src/builtin_tools.rs` to integrate capture and cleanup into one-shot
  PowerShell completion and cancellation.
- Modify lower-level Windows process tests only when a deterministic native or
  pure ancestry behavior cannot be covered through `tests/builtin_tools.rs`.

## Task 0: Restore the baseline and establish all REDs

This task removes every rejected production experiment and creates valid RED
evidence before implementation. Do not commit production code in this task.

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

## Task 1: Add stable Windows process-tree ownership

This task implements the native identity, discovery, capture, and consuming
cleanup primitives. It does not change one-shot or persistent caller ordering.

**Files:**

- Modify: `src/subprocess.rs`
- Test: `src/subprocess.rs` Windows-only unit tests or an existing lower-level
  Windows process test when integration coverage is required

- [ ] **Step 1: Add failing pure ancestry tests**

Extract parent-chain evaluation behind a private Windows-only helper that can
be tested with synthetic snapshot entries. Add tests proving that it:

- includes direct and transitive descendants of the stable root;
- uses every retained validated identity as an ancestry anchor;
- excludes an unrelated PID and breaks parent cycles without looping; and
- does not treat a PID from a stale snapshot as owned after fresh-snapshot
  ancestry no longer reaches an owned anchor.

Run:

```powershell
cargo test --jobs 1 --locked --lib windows_process_tree `
  -- --nocapture --test-threads=1
```

Expected: the new test target fails to compile or assert because the helper is
not implemented. Confirm the failure is specific to the new behavior.

- [ ] **Step 2: Introduce RAII ownership state**

Replace the tuple `WindowsJob` representation with a private `Arc`-owned state
that holds:

- the Job as `OwnedHandle` or an equivalent standard RAII owner;
- the stable root PID;
- a mutex-protected PID-deduplicated collection of validated descendant
  `OwnedHandle` values; and
- the first capture error, which later errors cannot overwrite.

Do not add a blanket `unsafe impl Send` or `unsafe impl Sync`. The owner must be
movable into `spawn_blocking` through standard handle ownership.

- [ ] **Step 3: Capture the stable root identity in both spawn paths**

In `spawn` and `spawn_std`, get the root PID synchronously from the newly
created process handle. When assigning any raw process handle, call
`GetProcessId(process)` and return `last_os_error()` when it returns zero.
Keep the child object live until the consuming fence finishes so its root handle
pins the root identity.

- [ ] **Step 4: Implement snapshot and post-open validation**

Use `CreateToolhelp32Snapshot`, `Process32FirstW`, and `Process32NextW` to build
PID-to-parent relationships. For each newly traceable PID:

1. Open it with only the query, synchronize, and terminate rights needed.
2. Keep the handle open.
3. Take a fresh snapshot after the open.
4. Require the same PID to remain present and its current parent chain to reach
   the stable root or a previously validated retained identity.
5. Close without termination if the PID disappeared or now has unrelated
   ancestry; treat snapshot or access ambiguity as an error.

Retain validated Job members as anchors as well as escaped descendants. Never
terminate solely from a PID observed in an old snapshot.

- [ ] **Step 5: Implement asynchronous early capture**

Add `capture_and_terminate(&self)`. It must clone the private `Arc` into
`tokio::task::spawn_blocking`, perform one discovery/open/revalidation pass,
store newly validated handles, preserve the first error, and only then call
fast `TerminateJobObject`. Repeated calls must be idempotent and may add newly
discovered identities.

The method performs no wait or fixed-point loop. It returns the capture error to
the caller, but callers must still wait for the root and run consuming cleanup
before surfacing that error.

- [ ] **Step 6: Implement the consuming fixed-point fence**

Add one consuming async cleanup method that moves the owner into
`spawn_blocking`. The blocking operation creates one 10-second `Instant`
deadline at entry and never restarts it. Until a complete fixed point, it must:

1. discover traceable descendants from the root and retained anchors;
2. open and fresh-snapshot revalidate every new identity;
3. call `TerminateJobObject` again;
4. terminate every active retained descendant through its handle;
5. wait on retained handles only for the remaining shared deadline; and
6. repeat discovery until a full validation pass adds no identity.

Success requires all retained handles signaled, Job accounting reporting zero
active processes, and one final validation snapshot adding no identity.
Snapshot failure, a present process that cannot be opened, failed termination,
failed wait, a blocking-task panic, or deadline expiry returns
`std::io::Error`. `ERROR_ACCESS_DENIED` never means that a process exited.
Return the stored first capture error after completing the best available fence.

- [ ] **Step 7: Keep synchronous termination and Drop fast**

Keep `terminate(&self)` limited to one best-effort `TerminateJobObject` call.
`Drop` may invoke only that operation. Neither may scan, wait, take the capture
mutex, panic, or overwrite a stored error.

- [ ] **Step 8: Make the ancestry tests GREEN and run lower-level tests**

Run:

```powershell
cargo test --jobs 1 --locked --lib windows_process_tree `
  -- --nocapture --test-threads=1
cargo test --jobs 1 --locked --test windows_process `
  -- --test-threads=1
```

Expected: both commands exit 0. The three higher-level PowerShell tests may
remain RED because caller integration is intentionally not part of this task.

- [ ] **Step 9: Commit and review Task 1**

Run:

```powershell
cargo fmt --all --check
git diff --check
git add -- src/subprocess.rs
git diff --cached --check
git commit -m "fix: track escaped Windows process identities"
```

Dispatch the Task 1 specification reviewer and reclaim it. Only after approval,
dispatch the quality reviewer and reclaim it. Fix and re-review every Critical
or Important finding.

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
uses identities retained during available capture opportunities and the final
snapshot rules. On fence failure, abort and await both stream tasks and surface
the same specific cleanup error.

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

# Windows PowerShell Job reap implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make one-shot and persistent PowerShell cleanup wait until every
owned Windows Job process exits before publishing completion.

**Architecture:** Add one consuming async fence to `WindowsJob` that keeps the
Job handle alive while the existing blocking accounting poll runs through
`spawn_blocking`. Callers terminate and fence before joining output drainers.
If fencing fails, callers abort and await drainers so retained pipes cannot
hide the bounded cleanup error or block lifecycle completion.

**Tech stack:** Rust 2024, Tokio, Windows Job Objects, and native Windows
integration tests.

---

## File map

The implementation remains within the existing process-ownership boundary.

- Modify `tests/builtin_tools.rs` to add one normal-completion descendant test.
- Modify `src/subprocess.rs` to add the consuming async Job fence and use it in
  persistent reaping.
- Modify `src/builtin_tools.rs` to fence one-shot PowerShell cleanup before
  joining output tasks.

### Task 1: Fence one-shot PowerShell completion

This task establishes every one-shot RED before changing production code, then
adds the shared Job primitive and one-shot integration.

**Files:**

- Modify: `tests/builtin_tools.rs:1000`
- Modify: `src/subprocess.rs:321`
- Modify: `src/builtin_tools.rs:1092`
- Test: `tests/builtin_tools.rs`

- [ ] **Step 1: Reconfirm the existing cancellation RED**

Run from the development worktree with generated artifacts on the spacious
NTFS target:

```powershell
$env:CARGO_TARGET_DIR = 'D:\Tessivum 验证缓存\a1495f3-compaction'
cargo test --jobs 1 --locked --test builtin_tools `
  powershell_cancellation_reaps_its_descendant_tree `
  -- --exact --nocapture --test-threads=1
```

Expected: exit 101 with `PowerShell process <PID> must be reaped`. Do not edit
production code unless this remains a behavior failure rather than a compile
or environment error.

- [ ] **Step 2: Add a normal-completion descendant test**

Add `powershell_normal_completion_reaps_its_descendant_tree` next to the
cancellation test. Use the existing `ToolRuntime`, `bash_config`, `text`, and
`assert_reaped` helpers. The PowerShell command must:

1. Start `%ComSpec% /d /c ping -n 30 127.0.0.1 >nul` with `Start-Process`.
2. Redirect the descendant's stdout and stderr to files under the temporary
   workspace so it does not retain the root PowerShell capture pipes.
3. Write only the descendant PID to standard output and let the root shell exit
   normally.

Assert that the tool output is successful, parse `text(&output)` as the PID,
and call the unchanged `assert_reaped` helper.

- [ ] **Step 3: Run the new test and verify RED**

```powershell
cargo test --jobs 1 --locked --test builtin_tools `
  powershell_normal_completion_reaps_its_descendant_tree `
  -- --exact --nocapture --test-threads=1
```

Expected: exit 101 because the tool returns before the descendant is fully
reaped. If it passes, strengthen only process creation and pipe isolation until
the missing fence is what causes RED. Do not shorten or extend `assert_reaped`.

- [ ] **Step 4: Add the consuming async Windows Job fence**

In `impl WindowsJob`, add a Windows-only method with this ownership contract:

```rust
pub(crate) async fn terminate_and_wait(self) -> std::io::Result<()> {
    self.terminate();
    tokio::task::spawn_blocking(move || self.wait_for_exit())
        .await
        .map_err(|error| {
            std::io::Error::other(format!(
                "Windows Job cleanup task failed: {error}"
            ))
        })?
}
```

Keep `terminate` and `wait_for_exit` for existing callers. Do not change the
10-second accounting deadline or the Job's kill-on-close configuration.

- [ ] **Step 5: Fence both one-shot return paths before stream joins**

In `run_windows_powershell`, replace each `job.terminate()` ownership boundary
with `job.terminate_and_wait().await` after the root child wait and before
`finish_copy`.

On fence success, call `finish_copy` for stdout and stderr as today. On fence
failure, call `abort()` on both JoinHandles, await both handles while ignoring
their cancellation results, and return:

```rust
bash_error("could not reap PowerShell Windows Job", error)
```

If the fence fails, its cleanup error takes precedence. After a successful
fence, preserve the existing ordering for root `wait` and non-`InvalidInput`
kill errors. Return `CANCELLED` only after the fence and stream tasks finish
successfully.

- [ ] **Step 6: Verify both one-shot tests are GREEN**

Run each exact test separately with `--test-threads=1`, then run them together
by name filter if the exact commands pass.

Expected: each exact command exits 0, and both descendant PIDs are absent before
the tool result boundary asserted by `assert_reaped`.

- [ ] **Step 7: Format, inspect, and commit Task 1**

```powershell
cargo fmt --all --check
git diff --check
git diff -- tests/builtin_tools.rs src/subprocess.rs src/builtin_tools.rs
git add -- tests/builtin_tools.rs src/subprocess.rs src/builtin_tools.rs
git commit -m "fix: fence one-shot PowerShell Job cleanup"
```

Before moving on, dispatch a specification-compliance reviewer for Task 1 and
then a code-quality reviewer. Resolve and re-review every Critical or Important
finding.

### Task 2: Fence persistent PowerShell reaping

This task uses the unchanged persistent lifecycle test as RED and keeps all
completion notification inside `reap_persistent_shell`.

**Files:**

- Modify: `src/subprocess.rs:1416`
- Test: `tests/builtin_tools.rs:1100`

- [ ] **Step 1: Reconfirm the persistent lifecycle RED after Task 1**

```powershell
$env:CARGO_TARGET_DIR = 'D:\Tessivum 验证缓存\a1495f3-compaction'
cargo test --jobs 1 --locked --test builtin_tools `
  persistent_powershell_cancel_disable_and_shutdown_reap_process_trees `
  -- --exact --nocapture --test-threads=1
```

Expected: exit 101 with an existing process-not-reaped assertion. Task 1 must
not be treated as proof that the persistent ownership boundary is fixed.

- [ ] **Step 2: Add a stable persistent cleanup error constructor**

Add a private Windows-only helper near `persistent_shell_closed`:

```rust
#[cfg(windows)]
fn persistent_shell_cleanup(error: std::io::Error) -> TessivumError {
    persistent_shell_error(
        "PERSISTENT_SHELL_CLEANUP",
        "persistent PowerShell Windows Job cleanup failed",
        json!({"error": error.to_string()}),
    )
}
```

Do not add a field to `ProcessDone` or change `disable`, `shutdown`, `cancel`,
or `dispose` return types.

- [ ] **Step 3: Reorder the Windows persistent reaper**

In `reap_persistent_shell`, keep Unix behavior unchanged. On Windows:

1. Take the Job from `inner.job` after the root child wait.
2. Await `terminate_and_wait` before joining stdout and stderr drainers.
3. On success, await both drainers and then publish the existing generic
   `persistent_shell_closed` error to any still-active incomplete command.
4. On failure, abort and await both drainers, then call
   `inner.fail_active(persistent_shell_cleanup(error))` before any generic
   closed-shell publication.
5. In both branches, compute the existing exit facts and call
   `inner.complete(ProcessDone { ... })` exactly once.

Earlier cancellation, timeout, disposal, or stream errors retain first-cause
precedence through `PersistentShellCommandState::fail`. A fence failure with no
active command remains internal because the public lifecycle APIs return `()`.

- [ ] **Step 4: Verify persistent cleanup is GREEN**

Run the exact persistent test from Step 1. Then rerun the one-shot cancellation
and normal-completion tests from Task 1.

Expected: all three exact commands exit 0 without changing the one-second
`assert_reaped` observation window.

- [ ] **Step 5: Run the affected integration binary both ways**

```powershell
cargo test --jobs 1 --locked --test builtin_tools -- --test-threads=1
cargo test --jobs 1 --locked --test builtin_tools
```

Expected: 0 failed tests in both runs.

- [ ] **Step 6: Format, inspect, and commit Task 2**

```powershell
cargo fmt --all --check
git diff --check
git diff -- src/subprocess.rs tests/builtin_tools.rs src/builtin_tools.rs
git add -- src/subprocess.rs
git commit -m "fix: fence persistent PowerShell Job cleanup"
```

Dispatch the Task 2 specification-compliance reviewer first and the
code-quality reviewer second. Reuse the implementer only to resolve review
findings, and reclaim every agent after its final response.

### Task 3: Verify the Rust repair boundary

This task produces fresh completion evidence without modifying source.

**Files:**

- Verify: `src/subprocess.rs`
- Verify: `src/builtin_tools.rs`
- Verify: `tests/builtin_tools.rs`

- [ ] **Step 1: Run static Rust checks**

```powershell
cargo fmt --all --check
cargo check --jobs 1 --all-targets --locked
cargo clippy --jobs 1 --all-targets --locked -- -D warnings
```

Expected: every command exits 0.

- [ ] **Step 2: Run the full Windows Rust suite**

```powershell
$env:CARGO_TARGET_DIR = 'D:\Tessivum 验证缓存\a1495f3-compaction'
cargo test --jobs 1 --all-targets --locked
```

Expected: exit 0 with zero failing tests. Preserve complete output. This is
development verification, not the later fresh Groups 01-16 acceptance run.

- [ ] **Step 3: Review the complete PowerShell implementation**

Dispatch one final read-only reviewer across the Task 1 base SHA through the
Task 2 head SHA. Resolve all Critical and Important findings, rerun the affected
commands, and immediately reclaim the reviewer.

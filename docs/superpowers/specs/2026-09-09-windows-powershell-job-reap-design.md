# Windows PowerShell Job reap fence design

This design makes Windows PowerShell tool cleanup complete before Tessivum
reports cancellation, disablement, shutdown, or normal command completion.
It closes a process-tree cleanup race exposed after merging Alpha.25.

## Context

The merged source at `a1495f33c7db1490b7705e67e18ae510666809d4`
failed two existing Windows integration tests:

- `powershell_cancellation_reaps_its_descendant_tree`;
- `persistent_powershell_cancel_disable_and_shutdown_reap_process_trees`.

Each test also failed when run alone with one test thread. Tessivum returned a
tool result, but the asserted descendant PowerShell process remained active
for longer than the test's one-second observation window. Each process exited
naturally about 0.2 to 0.3 seconds after the failed test command returned.

`WindowsJob::terminate` calls `TerminateJobObject`, which initiates
termination but does not wait for every Job process to exit. The code already
has `WindowsJob::wait_for_exit`, which polls Job accounting until
`ActiveProcesses` reaches zero. The affected cleanup paths do not call it.

## Goals

The implementation must make the documented process-tree ownership contract
true at the API boundary. A completed cleanup operation must imply that the
owned Windows Job contains no active processes.

The change must provide these outcomes:

- One-shot PowerShell cancellation waits for Job termination before returning
  `CANCELLED`.
- One-shot PowerShell completion terminates and fences unexpected descendants
  before returning output.
- Persistent-shell cancellation, disablement, shutdown, and stale retirement
  complete only after the Job reaches zero active processes.
- Job query failures and the existing 10-second timeout remain observable as
  tool cleanup errors.
- Blocking Windows Job polling does not occupy a Tokio async worker.

## Non-goals

This change does not extend the test timeout, add sleeps to tests, terminate
unowned processes, alter Unix process-group handling, or change PowerShell
command semantics. It does not redesign the subprocess service or modify the
Alpha.25 Agent lifecycle changes.

## Design

Add one asynchronous, consuming operation to the Windows Job owner in
`src/subprocess.rs`. The operation calls `TerminateJobObject`, then executes
the existing synchronous `wait_for_exit` poll through
`tokio::task::spawn_blocking`. Consuming the Job keeps its handle alive for the
entire fence and prevents a caller from accidentally reporting completion
while retaining an unfenced owner.

The operation maps a Tokio join failure to `std::io::Error`. It preserves the
existing Job query error and 10-second timeout without converting either into
success.

Use the operation at both ownership boundaries:

1. In one-shot `run_windows_powershell`, terminate the Job when cancellation
   wins, finish waiting for the root process and stream-copy tasks, then await
   the Job fence before returning `CANCELLED`.
2. In the normal one-shot completion path, fence the Job after the root process
   exits and before returning its normalized output.
3. In `reap_persistent_shell`, take the owned Job and await the same fence
   before publishing `ProcessDone` through `inner.complete`.

Persistent-shell cleanup remains centralized in `reap_persistent_shell`, so
cancellation, disablement, shutdown, and stale-workspace retirement inherit
the same completion guarantee.

## Error handling

The one-shot tool maps a failed Job fence through the existing `bash_error`
path with a specific cleanup message. Persistent-shell cleanup records the
fence failure in `ProcessDone` using the existing termination-error mechanism
before waking waiters. No path may return its original success or cancellation
result after the fence fails.

## TDD sequence

The two existing integration tests are the required RED evidence. They failed
individually under `--test-threads=1`, so default test parallelism is not
required to reproduce the race.

Implementation follows this sequence:

1. Re-run each exact test and preserve its current process-not-reaped failure.
2. Add the minimal asynchronous consuming Job fence.
3. Use it in one-shot PowerShell cancellation and completion.
4. Use it in persistent-shell reaping before completion notification.
5. Re-run both exact tests and require exit 0.
6. Run the full `builtin_tools` integration binary with one test thread and
   with its default test scheduling.
7. Run the wider Windows Rust suite with one Cargo build job.

## Files

The implementation surface is intentionally narrow:

- Modify `src/subprocess.rs` for the asynchronous Job fence and persistent
  reaper use.
- Modify `src/builtin_tools.rs` for one-shot PowerShell cleanup.
- Keep `tests/builtin_tools.rs` unchanged unless a new failure mode requires a
  focused assertion; do not weaken its existing reap checks.


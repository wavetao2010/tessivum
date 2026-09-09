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
- One-shot Job query failures and the existing 10-second timeout remain
  observable as tool cleanup errors.
- Persistent cleanup preserves an already-published first-cause error and
  reports a cleanup error to an active command when no earlier result won.
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
   wins, finish waiting for the root process, then await the Job fence before
   joining the stream-copy tasks or returning `CANCELLED`.
2. In the normal one-shot completion path, fence the Job after the root process
   exits and before joining the stream-copy tasks or returning its normalized
   output.
3. In `reap_persistent_shell`, take the owned Job and await the same fence
   before joining its stream drainers or publishing `ProcessDone` through
   `inner.complete`.

Persistent-shell cleanup remains centralized in `reap_persistent_shell`, so
cancellation, disablement, shutdown, and stale-workspace retirement inherit
the same completion guarantee.

## Error handling

The one-shot tool maps a failed Job fence through the existing `bash_error`
path with a specific cleanup message. If the fence succeeds, the tool joins the
stream-copy tasks normally. If the fence fails, it aborts and awaits both copy
tasks before returning the cleanup error. This prevents a descendant that still
holds a pipe from hiding the bounded fence error behind an unbounded stream
join. The tool does not return the original success or cancellation result
after the fence fails.

Persistent-shell cleanup attempts to publish a stable
`PERSISTENT_SHELL_CLEANUP` error through `fail_active` before publishing the
generic `persistent_shell_closed` error. The command's existing
first-result-wins behavior remains unchanged: a cancellation, disposal, or
stream error that already won is not overwritten by a later Job fence error.

If the persistent fence succeeds, the reaper joins both stream drainers before
calling `inner.complete`. If it fails, the reaper aborts and awaits both
drainers, attempts to publish `PERSISTENT_SHELL_CLEANUP`, and then calls
`inner.complete` unconditionally. This releases `stop`, `disable`, `shutdown`,
and other waiters even when a surviving process retains a pipe. The generic
closed-shell error is published only after successful fencing, or after the
cleanup-error attempt when no earlier command result won.

When no command is active, the current persistent lifecycle API cannot expose
the Job fence error: `ProcessDone` contains exit and termination facts only,
while `disable` and `shutdown` return `()`. This narrow repair therefore treats
that case as a best-effort internal cleanup failure. Expanding `ProcessDone` or
changing the public lifecycle methods to return `Result` is explicitly outside
this change. A failed fence does not establish the zero-active-process
guarantee, but it also cannot suppress completion notification.

## TDD sequence

The two existing integration tests are required RED evidence. They failed
individually under `--test-threads=1`, so default test parallelism is not
required to reproduce the race. A third focused test must establish RED for
normal one-shot completion when the PowerShell root exits while its descendant
is still active.

Implementation follows this sequence:

1. Re-run both existing exact tests and preserve their process-not-reaped
   failures.
2. Add the normal one-shot descendant test, run it before production changes,
   and require the same process-not-reaped RED.
3. Add the minimal asynchronous consuming Job fence.
4. Use it before stream joins in one-shot PowerShell cancellation and normal
   completion, including abort-and-await handling for fence failure.
5. Use it before stream joins and completion notification in persistent-shell
   reaping, including abort-and-await handling for fence failure.
6. Re-run all three exact tests and require exit 0.
7. Run the full `builtin_tools` integration binary with one test thread and
   with its default test scheduling.
8. Run the wider Windows Rust suite with one Cargo build job.

## Files

The implementation surface is intentionally narrow:

- Modify `src/subprocess.rs` for the asynchronous Job fence and persistent
  reaper use.
- Modify `src/builtin_tools.rs` for one-shot PowerShell cleanup.
- Modify `tests/builtin_tools.rs` only to add the focused normal-completion
  descendant test; do not weaken its existing reap checks or timeout.

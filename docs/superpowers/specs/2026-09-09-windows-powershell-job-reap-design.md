# Windows PowerShell escaped-descendant reap fence design

This design makes Windows PowerShell cleanup wait for both Job members and
descendants that escape Job membership under a nested host Job. It replaces an
accounting-only fence disproved by pre-termination process evidence.

## Context

The current correction baseline is the merged Alpha.27 source at
`da0a4d09a60280a745910c9d4de4c85b5210c9d5`. It contains the initial native
process-tree implementation from
`4d9f74573bae2ca05b666002de17e3abb8a504e1`. The original baseline failed
these existing Windows integration tests individually:

- `powershell_cancellation_reaps_its_descendant_tree`;
- `persistent_powershell_cancel_disable_and_shutdown_reap_process_trees`.

Each test returned an API result while a PowerShell-created process remained
active beyond the unchanged one-second observation window.

The first diagnosis found that `TerminateJobObject` is asynchronous and the
affected callers do not use the existing `WindowsJob::wait_for_exit`. A direct
accounting fence compiled, but it did not close the normal-completion RED.
Pre-termination probes then established the missing ownership fact:

- The root PowerShell process reported `IsProcessInJob == true`.
- Its suspended direct `CreateProcessW` child reported
  `IsProcessInJob == false` before executing any instruction.
- The existing test's `Start-Process` child also reported false before
  cancellation.
- Adding `CREATE_BREAKAWAY_FROM_JOB` to the root prevented PowerShell from
  starting in the current host and is not a usable repair.

Job accounting cannot fence a process that is not a Job member. The cleanup
boundary must retain the Job for normal members and explicitly own escaped
descendants by their root-parent relationship.

## Goals

The implementation must make a completed PowerShell cleanup operation imply
that no process discovered in the owned root tree remains active.

The change must provide these outcomes:

- One-shot cancellation reaps Job members and escaped descendants before
  returning `CANCELLED`.
- Normal one-shot completion reaps an escaped descendant before returning
  output.
- Persistent cancellation, timeout, disablement, shutdown, stale retirement,
  and last-owner drop publish completion only after tree cleanup finishes.
- Tree enumeration, process access, termination, Job query, wait, and Tokio
  join failures remain observable cleanup errors where current APIs can carry
  them.
- Fixed-point scanning and process-handle waits do not occupy a Tokio worker.

## Non-goals

This change does not call `taskkill`, extend test timeouts, add sleeps to tests,
kill processes based only on a numeric PID, alter Unix process groups, or change
PowerShell command semantics. It does not change public persistent-shell return
types, redesign the subprocess service, or modify unrelated Alpha.27 behavior.

This change is not a system-wide process monitor. After a root exits normally,
Windows does not retain a queryable ancestry chain through every already-exited
intermediate process. Normal-completion cleanup covers descendants whose current
snapshot ancestry still reaches the exact root generation or a generation
retained by an earlier snapshot. It does not claim to recover an adversarial
orphan whose full ancestry disappeared before Tessivum could observe it.

## Ownership model

Change `WindowsJob` from a bare Job handle to a private owner that records an
exact root process generation and exact generations for every retained process.
A generation identity contains the PID and the `GetProcessTimes` creation
timestamp. It also owns the process handle used to obtain and act on that
identity. Numeric PIDs and snapshot parent PIDs are discovery hints only; they
never authorize termination.

`spawn`, `spawn_std`, and `assign_raw` must produce the same owner. During
assignment, synchronously duplicate the caller's root process handle into an
`OwnedHandle`, call `GetProcessId` on that owned handle, and call
`GetProcessTimes` to read its creation timestamp. Fail assignment if handle
duplication, PID lookup, or creation-time lookup fails. This guarantees that
the owner does not borrow the child object's handle. Identity is never derived
from `Child::id()` after a wait.

Retain every validated generation as an
`std::os::windows::io::OwnedHandle` value or an equivalent local RAII owner
whose `Send` behavior follows the standard handle owner. Put the Job handle,
owned root generation, and mutex-protected generation map behind a private
`Arc`. Key retained state by `(PID, creation time)`, not PID alone. The state
also stores the first capture error. Do not store bare handles behind an
unexplained `unsafe impl Send` or `unsafe impl Sync`.

Keep synchronous `terminate(&self)` limited to a fast, best-effort
`TerminateJobObject` call. It performs no Toolhelp scan, takes no capture-state
lock, and never waits. `Drop` may call only this fast operation.

Add asynchronous `capture_and_terminate(&self)`. It clones the private `Arc`
into `spawn_blocking`, takes one discovery snapshot, opens every currently
traceable descendant regardless of Job membership, validates each exact
generation parent-first with a fresh snapshot as described below, stores
validated handles, records the first error without overwriting it, and calls
`TerminateJobObject`. Job-member generations remain ancestry anchors because
an intermediate member can create an escaped child before Job termination.

The async operation returns its error to the caller and also leaves the first
error in shared state for the consuming fence. A caller never returns directly
from capture failure: it still waits for the root, runs the fence, and completes
stream/lifecycle cleanup before surfacing the stored cleanup error.

Repeated capture is idempotent. It may retain additional generations and
deduplicates by `(PID, creation time)`. A handle keeps the process object
available for generation and exit-time queries after termination, but it does
not reserve the numeric PID. The operation does not run a fixed-point loop or
wait; those bounded operations belong to the consuming fence.

Add one consuming async cleanup method. It moves the owner into
`tokio::task::spawn_blocking` and performs one bounded native cleanup operation:

1. Reuse the owned root generation and every generation retained by an earlier
   `capture_and_terminate(&self)` call, then take a new Toolhelp process
   snapshot.
2. Treat `PROCESSENTRY32W.th32ParentProcessID` values as candidate-discovery
   hints. They are not process identities.
3. Process candidates parent-first. Before considering a descendant, require
   its immediate parent to be the exact root generation or an exact retained
   generation that was validated earlier.
4. Open each candidate PID with the minimum process rights needed to query,
   terminate, and wait. Keep the handle open, verify `GetProcessId`, and read
   its creation timestamp with `GetProcessTimes`.
5. Take a fresh Toolhelp snapshot after opening the candidate. Require the
   candidate's current immediate parent PID to name the previously validated
   parent generation. Then prove the edge from handle state and process times.
   For a live parent, require parent creation time to be no later than child
   creation time. For a signaled historical parent, also require child creation
   time to be no later than the parent's `GetProcessTimes` exit time.
6. Reject a current process whose PID matches an anchor but whose creation time
   differs. Do not use it as the old anchor, and do not terminate it. Missing
   timing data, inconsistent ordering, disappearance during validation, or any
   other identity ambiguity is a cleanup error.
7. Retain each validated generation before validating its descendants. Call
   `TerminateJobObject` again, terminate active retained generations through
   their owned handles, and wait only until the shared deadline.
8. Resnapshot and repeat parent-first validation until one full pass adds no
   generation. Then require all retained handles to be signaled and Job
   accounting to report zero active processes.
9. Take one final validation snapshot and require that it adds no generation
   before returning success.

The consuming blocking cleanup creates one `Instant` deadline at entry and
reuses it for every snapshot pass, Job query, termination pass, and handle wait.
It never restarts the deadline for a process or loop iteration. The earlier
asynchronous capture performs one discovery and validation pass without waits;
it records any failure for the consuming fence to return later.

A failed snapshot, a process that cannot be opened, missing process times,
failed termination of an unsignaled handle, failed wait, or deadline expiry
returns `std::io::Error`. `ERROR_ACCESS_DENIED` alone is not equivalent to
process exit. If `TerminateProcess` fails, immediately recheck that same owned
handle with a zero-time wait. Suppress only the benign
`ERROR_ACCESS_DENIED` race when the handle is signaled. If the handle is still
unsignaled or the wait fails, preserve the original termination error. Preserve
other termination errors even if a later observation finds the process exited.

## Generation identity safety

Windows guarantees that a process handle remains valid after termination for
queries against the process object. It separately states that a PID is valid
only until the process terminates and may then be reused. Therefore, neither an
open handle nor a snapshot can make a numeric PID a stable identity. The owner
uses the handle to preserve and query a specific process object, and uses its
`(PID, creation time)` pair to name that generation.

Every process is opened before termination, and every subsequent action uses
the same retained handle. After opening a PID, the helper verifies the handle's
PID and creation time, takes a fresh snapshot, and validates exactly one parent
edge. A PID reuse produces a different creation timestamp. The helper neither
treats the replacement as an old anchor nor terminates it. Any access, timing,
or identity ambiguity is an error, not permission to target a process.

Fresh fixed-point passes close the race where a validated generation creates a
late descendant while the root or another descendant is terminating. Each pass
starts from exact retained anchors and retains a validated parent before its
children. If an intermediate process exits, its owned handle remains a safe
historical anchor: `GetProcessTimes` supplies its creation and exit bounds, so a
child edge is valid only when the child's creation falls within that lifetime.
A reused PID necessarily names a later generation and cannot satisfy that
historical interval.

## Caller integration

Use the consuming tree fence at each Windows PowerShell ownership boundary:

1. In one-shot cancellation, await `capture_and_terminate` while the root is
   live, request root termination, wait for the root process, then await the
   consuming tree fence before joining stream tasks or returning `CANCELLED`.
2. In normal one-shot completion, await the tree fence after the root wait and
   before joining stream tasks or returning output.
3. Add a persistent cleanup-request notification. `stop`, timeout, cancellation,
   disposal, and last-owner `Drop` publish their existing first cause and signal
   this request without scanning the process table. The reaper uses a biased
   selection that prefers an already-ready cleanup request over root exit. When
   the request wins while the root is live, it awaits `capture_and_terminate`
   before waiting for root exit. It then takes the same owner and awaits the
   consuming tree fence before stream drainers and `inner.complete`.

Existing synchronous or drop-only `WindowsJob` callers retain fast best-effort
Job termination only. Any caller that claims awaited process-tree cleanup must
use asynchronous capture and the consuming fence. Unix paths remain
byte-for-byte unchanged.

## Error handling

The one-shot tool maps a failed tree fence through `bash_error` with a specific
cleanup message. On fence failure it aborts and awaits both stream-copy tasks
before returning the cleanup error. It does not return the original success or
cancellation result after cleanup fails.

Persistent cleanup attempts to publish `PERSISTENT_SHELL_CLEANUP` through
`fail_active` before the generic closed-shell error. Existing first-result-wins
behavior preserves cancellation, timeout, disposal, or stream errors that won
earlier.

After a persistent fence failure, the reaper aborts and awaits both drainers and
reaches one single cleanup tail. That tail calls `inner.complete` after success,
fence failure, drainer failure, join failure, cancellation, and a blocking-task
panic converted from Tokio `JoinError`. This prevents lifecycle waiters from
hanging. When no command is active, current `ProcessDone`, `disable`, and
`shutdown` APIs cannot surface the cleanup error; that case remains an internal
best-effort failure without expanding the public API.

`Drop` remains best effort: it may call fast `TerminateJobObject`, but it never
scans, locks capture state, waits, panics, or overwrites a stored error. The
consuming method owns the wrapper and prevents a second fence by consuming
`self`; its private `Arc` keeps the handle and retained identities alive through
blocking work. `WindowsJob` must remain usable in existing `Send` futures
without blanket unsafe trait implementations.

## TDD sequence

The implementation follows these behavior-first steps:

1. Preserve the three existing behavior regressions: one-shot cancellation,
   normal completion with a suspended escaped child, and persistent lifecycle
   cleanup. Their test baseline is `291348c`.
2. Before changing the current Task 1 production implementation, add separate
   pure regressions for retained-anchor PID reuse and root PID reuse. Each test
   must prove that PID-only ancestry accepts the reused generation's child.
   Exact creation and exit times must reject that child and must also exclude
   the replacement generation itself from retained and termination-target
   results. Run each exact test against `4d9f745` as merged in `da0a4d0`, and
   require RED at its new assertion before any production change.
3. Add a deterministic test for `TerminateProcess` failure normalization. It
   must cover signaled `ERROR_ACCESS_DENIED`, an unsignaled handle, and wait
   failure. Run it against the same production source and require RED for the
   expected missing helper or assertion reason.
4. Only after all three new REDs are recorded, add the owned root handle,
   `(PID, creation time)` generation keys, parent-first validation, historical
   exit-time checks, and termination-race normalization. Commit the correction
   normally at the current merged head without rewriting existing history.
5. Integrate one-shot cancellation and normal completion before stream joins.
6. Integrate persistent reaping before drainers and completion notification.
7. Run all three exact behavior tests, the generation-identity unit tests, full
   `builtin_tools`, `windows_process`, and the wider Windows Rust suite with one
   Cargo build job.

No test may extend the existing reap observation window or accept a process
that exits only after natural command completion. The focused test must include
a test-owned process-handle guard that terminates and waits for the
intentionally leaked child on panic or failed assertion. Helper compilation,
membership setup, timeout, or output-parsing failures are not valid RED
evidence.

## Files

The expected implementation surface is intentionally narrow:

- Modify `src/subprocess.rs` for exact root identity, Toolhelp discovery,
  generation-owned process handles, and persistent integration.
- Modify `src/builtin_tools.rs` for one-shot tree fencing.
- Modify `tests/builtin_tools.rs` for the focused normal-completion regression
  and removal of temporary diagnostics.
- Modify lower-level Windows process tests only when needed to express native
  helper behavior without production test hooks.

## Microsoft references

These Microsoft API contracts define the identity and termination rules used by
this design:

- [Process Handles and Identifiers][process-handles] explains that a process
  handle remains valid after termination, while a PID is valid only until
  termination and can then be reused.
- [`GetProcessTimes`][get-process-times] supplies creation and exit timestamps
  for a process object addressed through its handle.
- [`TerminateProcess`][terminate-process] documents asynchronous termination
  and the `ERROR_ACCESS_DENIED` result for an already terminated process.

[process-handles]: https://learn.microsoft.com/en-us/windows/win32/procthread/process-handles-and-identifiers
[get-process-times]: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesstimes
[terminate-process]: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess

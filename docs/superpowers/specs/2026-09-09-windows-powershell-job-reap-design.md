# Windows PowerShell escaped-descendant reap fence design

This design makes Windows PowerShell cleanup wait for both Job members and
descendants that escape Job membership under a nested host Job. It replaces an
accounting-only fence disproved by pre-termination process evidence.

## Context

The merged source at `a1495f33c7db1490b7705e67e18ae510666809d4`
failed these existing Windows integration tests individually:

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
kill processes based only on an unverified PID, alter Unix process groups, or
change PowerShell command semantics. It does not change public persistent-shell
return types, redesign the subprocess service, or modify Alpha.25 Agent logic.

This change is not a system-wide process monitor. After a root exits normally,
Windows does not retain a queryable ancestry chain through every already-exited
intermediate process. Normal-completion cleanup covers descendants whose current
snapshot ancestry still reaches the stable root or an identity retained by an
earlier snapshot; it does not claim to recover an adversarial orphan whose full
ancestry disappeared before Tessivum could observe it.

## Ownership model

Change `WindowsJob` from a bare Job handle to a private owner that also records
the assigned root process ID and retained handles for escaped descendants.
`spawn`, `spawn_std`, and `assign_raw` must produce the same owner; `assign_raw`
calls `GetProcessId(process)` and fails with the last operating-system error if
the result is zero. `spawn` and `spawn_std` obtain the root ID synchronously
from the newly spawned process handle before returning. Callers keep that root
process handle alive through the consuming fence; identity is never derived
from `Child::id()` after a wait.

Retain descendants as `std::os::windows::io::OwnedHandle` values or an
equivalent local RAII owner whose `Send` behavior follows the standard handle
owner. Put the Job handle, root ID, and mutex-protected PID-deduplicated capture
state behind a private `Arc`. The state also stores the first capture error. Do
not store bare handles behind an unexplained `unsafe impl Send` or
`unsafe impl Sync`.

Keep synchronous `terminate(&self)` limited to a fast, best-effort
`TerminateJobObject` call. It performs no Toolhelp scan, takes no capture-state
lock, and never waits. `Drop` may call only this fast operation.

Add asynchronous `capture_and_terminate(&self)`. It clones the private `Arc`
into `spawn_blocking`, takes one discovery snapshot, opens every currently
traceable descendant regardless of Job membership, revalidates each identity
with one fresh snapshot as described below, stores validated handles, records
the first error without overwriting it, and calls `TerminateJobObject`.
Job-member identities remain ancestry anchors because an intermediate member
can create an escaped child before Job termination.

The async operation returns its error to the caller and also leaves the first
error in shared state for the consuming fence. A caller never returns directly
from capture failure: it still waits for the root, runs the fence, and completes
stream/lifecycle cleanup before surfacing the stored cleanup error.

Repeated capture is idempotent. It may retain additional identities and
deduplicates by PID while retained handles pin those identities. The operation
does not run a fixed-point loop or wait; those bounded operations belong to the
consuming fence.

Add one consuming async cleanup method. It moves the owner into
`tokio::task::spawn_blocking` and performs one bounded native cleanup operation:

1. Reuse every handle retained by an earlier `terminate(&self)` call and take a
   new Toolhelp process snapshot.
2. Build parent relationships from
   `PROCESSENTRY32W.th32ParentProcessID`.
3. Find every process whose parent chain reaches the recorded root ID or a
   retained descendant identity.
4. Open each newly discovered PID with the minimum process rights needed to
   query, terminate, and wait. Keep the handle open, then take another Toolhelp
   snapshot and revalidate that the same PID is present and its current parent
   chain still reaches the stable root or an already validated retained
   identity.
5. If the PID is absent from the verification snapshot, close the new handle and
   treat that identity as exited. If the PID is present without owned ancestry,
   close it without termination. A snapshot or access ambiguity is an error.
6. Retain only revalidated handles. Call `TerminateJobObject` again, terminate
   active retained descendants through their handles, and wait only until the
   shared deadline.
7. Resnapshot, validate, and retain newly discovered identities until one full
   validation pass adds none. Then require all retained handles to be signaled
   and Job accounting to report zero active processes.
8. Take one final validation snapshot and require that it adds no identity
   before returning success.

The consuming blocking cleanup creates one `Instant` deadline at entry and
reuses it for every snapshot pass, Job query, termination pass, and handle wait.
It never restarts the deadline for a process or loop iteration. The earlier
asynchronous capture performs one discovery and validation pass without waits;
it records any failure for the consuming fence to return later.

A failed snapshot, a process that remains present but cannot be opened, failed
termination of an unsignaled handle, failed wait, or deadline expiry returns
`std::io::Error`. `ERROR_ACCESS_DENIED` is not equivalent to process exit. A
process disappearing before any handle is acquired counts as exited only after
a fresh validated snapshot proves the PID absent.

## PID safety

Callers keep the Tokio or standard child object alive until the consuming tree
fence returns. Its process handle prevents reuse of the recorded root ID during
enumeration. Every descendant is opened before termination; subsequent actions
use the retained handle, not a second lookup by PID.

The helper never terminates a PID merely because its number appeared in an old
snapshot. After opening a PID, it revalidates the current entry and ancestry in
a fresh snapshot while the handle pins the opened identity. If the PID was
reused between snapshots, the new ancestry check prevents termination. Any
access ambiguity is an error, not permission to target an unrelated process.

The pre-termination snapshot and fixed-point loop close the race where an
escaped descendant creates another process while the root or another descendant
is terminating. Retained handles keep known process identities stable across
iterations.

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

1. Restore the two existing exact tests without temporary membership
   instrumentation and reconfirm their process-not-reaped RED results.
2. Keep one focused normal-completion test whose root is in a Job and whose
   suspended direct child proves it escaped before the root returns. Run it
   against unmodified production HEAD and require RED only at the unchanged reap
   assertion before production edits.
3. Add lower-level test coverage for descendant discovery and PID/handle error
   behavior where it can be deterministic without test-only production hooks.
4. Add the root ID to `WindowsJob` and implement the consuming native tree
   fence.
5. Integrate one-shot cancellation and normal completion before stream joins.
6. Integrate persistent reaping before drainers and completion notification.
7. Run all three exact tests, full `builtin_tools`, `windows_process`, and the
   wider Windows Rust suite with one Cargo build job.

No test may extend the existing reap observation window or accept a process
that exits only after natural command completion. The focused test must include
a test-owned process-handle guard that terminates and waits for the
intentionally leaked child on panic or failed assertion. Helper compilation,
membership setup, timeout, or output-parsing failures are not valid RED
evidence.

## Files

The expected implementation surface is intentionally narrow:

- Modify `src/subprocess.rs` for root identity, Toolhelp discovery, stable
  process handles, and persistent integration.
- Modify `src/builtin_tools.rs` for one-shot tree fencing.
- Modify `tests/builtin_tools.rs` for the focused normal-completion regression
  and removal of temporary diagnostics.
- Modify lower-level Windows process tests only when needed to express native
  helper behavior without production test hooks.

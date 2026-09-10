//! Shell-free subprocess ownership with bounded, replayable output.

#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(windows)]
use std::sync::atomic::AtomicUsize;
#[cfg(any(unix, windows))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::{
    collections::BTreeSet,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(any(unix, windows))]
use tessivum_core::CancellationToken;
use tessivum_core::{ContextHandle, CoreError, ServiceHandle, ServiceKey};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    process::{Child, ChildStderr, ChildStdout, Command},
    sync::Notify,
    time,
};
#[cfg(any(unix, windows))]
use tokio::{process::ChildStdin, sync::Mutex as AsyncMutex};

use crate::TessivumError;

const DEFAULT_TAIL_BYTES: usize = 64 * 1024;
const MAX_TAIL_BYTES: usize = 16 * 1024 * 1024;
const MAX_READ_BYTES: usize = 16 * 1024 * 1024;

/// Stable key for the local subprocess capability.
pub fn subprocess_service_key() -> ServiceKey {
    ServiceKey::new("harness.subprocess", "1")
}

/// Explicit stdin behavior for a child process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessStdin {
    Inherit,
    Null,
    Bytes(Vec<u8>),
}

/// Bounded capture policy for one child output stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureOutput {
    pub tail_bytes: usize,
    /// An optional, create-new full-output file. The in-memory tail remains
    /// bounded even when this is configured.
    pub spill_path: Option<PathBuf>,
}

impl Default for CaptureOutput {
    fn default() -> Self {
        Self {
            tail_bytes: DEFAULT_TAIL_BYTES,
            spill_path: None,
        }
    }
}

/// Explicit stdout or stderr behavior for a child process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessOutput {
    Inherit,
    Null,
    Capture(CaptureOutput),
}

/// A shell-free child-process request. Every argument becomes one argv entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubprocessRequest {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// `None` removes an inherited key; `Some` is an explicit opt-in value.
    pub env: BTreeMap<String, Option<String>>,
    pub stdin: ProcessStdin,
    pub stdout: ProcessOutput,
    pub stderr: ProcessOutput,
    pub terminate_grace: Duration,
}

impl SubprocessRequest {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            cwd: None,
            env: BTreeMap::new(),
            stdin: ProcessStdin::Null,
            stdout: ProcessOutput::Capture(CaptureOutput::default()),
            stderr: ProcessOutput::Capture(CaptureOutput::default()),
            terminate_grace: Duration::from_millis(500),
        }
    }
}

/// Terminal first-cause facts. A terminal process is always represented by a
/// value; only spawn/validation failures return `Err`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessTermination {
    Terminated,
    TimedOut,
    Aborted,
    Shutdown,
}

/// Exit facts for one owned child tree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessDone {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub termination: Option<ProcessTermination>,
}

/// Snapshot of a bounded captured output stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutputSnapshot {
    pub tail: Vec<u8>,
    pub total_bytes: u64,
    pub available_from: u64,
    pub spill_path: Option<PathBuf>,
    pub spill_error: Option<String>,
}

/// A non-consuming range read of a completed output stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessOutputRead {
    pub offset: u64,
    pub next_offset: u64,
    pub total_bytes: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
struct CapturedOutput {
    tail: Vec<u8>,
    total_bytes: u64,
    spill_path: Option<PathBuf>,
    spill_error: Option<String>,
}

impl CapturedOutput {
    fn empty() -> Self {
        Self {
            tail: Vec::new(),
            total_bytes: 0,
            spill_path: None,
            spill_error: None,
        }
    }

    fn snapshot(&self) -> ProcessOutputSnapshot {
        ProcessOutputSnapshot {
            tail: self.tail.clone(),
            total_bytes: self.total_bytes,
            available_from: self.total_bytes.saturating_sub(self.tail.len() as u64),
            spill_path: self.spill_path.clone(),
            spill_error: self.spill_error.clone(),
        }
    }
}

struct ProcessState {
    pid: u32,
    termination: Option<ProcessTermination>,
    done: Option<ProcessDone>,
    stdout: CapturedOutput,
    stderr: CapturedOutput,
}

struct ProcessInner {
    state: Mutex<ProcessState>,
    #[cfg(windows)]
    job: Mutex<Option<WindowsJob>>,
    done: Notify,
}

#[cfg(windows)]
pub(crate) struct WindowsJob {
    inner: Arc<WindowsJobState>,
}

#[cfg(windows)]
struct WindowsJobState {
    job: OwnedHandle,
    root: ProcessGeneration,
    capture: Mutex<WindowsCaptureState>,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ProcessGenerationKey {
    pid: u32,
    creation_time: u64,
}

#[cfg(windows)]
struct ProcessGeneration {
    key: ProcessGenerationKey,
    handle: OwnedHandle,
}

#[cfg(windows)]
#[derive(Default)]
struct WindowsCaptureState {
    retained: BTreeMap<ProcessGenerationKey, ProcessGeneration>,
    first_error: Option<WindowsCaptureError>,
}

#[cfg(windows)]
struct WindowsCaptureError {
    kind: std::io::ErrorKind,
    raw_os_error: Option<i32>,
    message: String,
}

#[cfg(windows)]
impl WindowsCaptureError {
    fn from_error(error: &std::io::Error) -> Self {
        Self {
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
            message: error.to_string(),
        }
    }

    fn to_error(&self) -> std::io::Error {
        match self.raw_os_error {
            Some(code) => std::io::Error::from_raw_os_error(code),
            None => std::io::Error::new(self.kind, self.message.clone()),
        }
    }
}

#[cfg(windows)]
impl WindowsJob {
    /// Spawns a Tokio child suspended, attaches it to a kill-on-close Job,
    /// then resumes its only thread. No child instruction can run before the
    /// Job owns the process. Every post-spawn failure terminates and waits for
    /// the still-suspended process before returning.
    /// This helper owns `CommandExt::creation_flags`; callers must not depend
    /// on flags configured on the command before this call.
    pub(crate) fn spawn(command: &mut Command) -> std::io::Result<(Child, Self)> {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

        command.as_std_mut().creation_flags(CREATE_SUSPENDED);
        let mut child = command.spawn()?;
        let process = match child.raw_handle() {
            Some(process) => process as windows_sys::Win32::Foundation::HANDLE,
            None => {
                terminate_and_reap_tokio_child(&mut child);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "suspended process has no process handle",
                ));
            }
        };
        let pid = process_id(process).inspect_err(|_| terminate_and_wait_process(process))?;
        let job = match unsafe { Self::assign_raw(process) } {
            Ok(job) => job,
            Err(error) => {
                terminate_and_wait_process(process);
                return Err(error);
            }
        };
        if let Err(error) = resume_suspended_process(pid) {
            job.terminate();
            terminate_and_wait_process(process);
            return Err(error);
        }
        Ok((child, job))
    }

    /// `std::process` counterpart to [`WindowsJob::spawn`].
    pub(crate) fn spawn_std(
        command: &mut std::process::Command,
    ) -> std::io::Result<(std::process::Child, Self)> {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

        command.creation_flags(CREATE_SUSPENDED);
        let mut child = command.spawn()?;
        let process = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        if process.is_null() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "suspended process has no process handle",
            ));
        }
        let pid = process_id(process).inspect_err(|_| terminate_and_wait_process(process))?;
        let job = match unsafe { Self::assign_raw(process) } {
            Ok(job) => job,
            Err(error) => {
                terminate_and_wait_process(process);
                return Err(error);
            }
        };
        if let Err(error) = resume_suspended_process(pid) {
            job.terminate();
            terminate_and_wait_process(process);
            return Err(error);
        }
        Ok((child, job))
    }

    /// Attaches an already-suspended process to a new kill-on-close Job.
    ///
    /// # Safety
    /// `process` must be a valid process handle with assignment rights and its
    /// primary thread must not have been resumed. The caller owns that process
    /// handle, must terminate and wait for it if this returns `Err`, and may
    /// resume it only after this returns `Ok`.
    pub(crate) unsafe fn assign_raw(
        process: windows_sys::Win32::Foundation::HANDLE,
    ) -> std::io::Result<Self> {
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
        };

        if process.is_null() || process == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "process handle is invalid",
            ));
        }
        let root = ProcessGeneration::from_owned_handle(duplicate_process_handle(process)?)?;
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `CreateJobObjectW` returned a new owned, non-null handle.
        let job = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { AssignProcessToJobObject(handle, process) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            inner: Arc::new(WindowsJobState {
                job,
                root,
                capture: Mutex::new(WindowsCaptureState::default()),
            }),
        })
    }

    pub(crate) fn terminate(&self) {
        let _ = terminate_windows_job(&self.inner);
    }

    pub(crate) async fn capture_and_terminate(&self) -> std::io::Result<()> {
        let state = Arc::clone(&self.inner);
        let task_state = Arc::clone(&state);
        match tokio::task::spawn_blocking(move || {
            let discovery = discover_and_retain_processes(&task_state, None);
            let mut first_error = discovery.first_error;
            if let Err(error) = terminate_windows_job(&task_state) {
                preserve_first_capture_error(&task_state, &error);
                preserve_first_error(&mut first_error, error);
            }
            if let Some(error) = first_error {
                Err(error)
            } else {
                Ok(())
            }
        })
        .await
        {
            Ok(result) => result,
            Err(error) => {
                let error = blocking_task_error("Windows process-tree capture", error);
                preserve_first_capture_error(&state, &error);
                let _ = terminate_windows_job(&state);
                Err(error)
            }
        }
    }

    pub(crate) async fn cleanup_process_tree(self) -> std::io::Result<()> {
        let state = Arc::clone(&self.inner);
        match tokio::task::spawn_blocking(move || cleanup_windows_process_tree(&self.inner)).await {
            Ok(result) => result,
            Err(error) => {
                let task_error = blocking_task_error("Windows process-tree cleanup", error);
                match lock(&state.capture)
                    .first_error
                    .as_ref()
                    .map(WindowsCaptureError::to_error)
                {
                    Some(error) => Err(error),
                    None => Err(task_error),
                }
            }
        }
    }

    /// Fences permission teardown against asynchronous Job termination.
    pub(crate) fn wait_for_exit(&self) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::{
            JobObjectBasicAccountingInformation, QueryInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        };

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            if unsafe {
                QueryInformationJobObject(
                    self.inner.job.as_raw_handle() as _,
                    JobObjectBasicAccountingInformation,
                    std::ptr::from_mut(&mut info).cast(),
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if info.ActiveProcesses == 0 {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Windows Job processes did not exit after termination",
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(windows)]
fn process_id(process: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<u32> {
    use windows_sys::Win32::System::Threading::GetProcessId;

    // SAFETY: callers provide a live process handle; GetProcessId does not retain it.
    let pid = unsafe { GetProcessId(process) };
    if pid == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(pid)
    }
}

#[cfg(windows)]
impl ProcessGeneration {
    fn from_owned_handle(handle: OwnedHandle) -> std::io::Result<Self> {
        let process = handle.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        let pid = process_id(process)?;
        let creation_time = process_times(process)?.creation_time;
        Ok(Self {
            key: ProcessGenerationKey { pid, creation_time },
            handle,
        })
    }
}

#[cfg(windows)]
fn duplicate_process_handle(
    process: windows_sys::Win32::Foundation::HANDLE,
) -> std::io::Result<OwnedHandle> {
    use windows_sys::Win32::{
        Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS},
        System::Threading::GetCurrentProcess,
    };

    // SAFETY: `GetCurrentProcess` returns the current process pseudo-handle.
    let current_process = unsafe { GetCurrentProcess() };
    let mut duplicate = std::ptr::null_mut();
    // SAFETY: source and target are the current process, `process` is valid by
    // the caller contract, and `duplicate` points to writable handle storage.
    if unsafe {
        DuplicateHandle(
            current_process,
            process,
            current_process,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `DuplicateHandle` returned a new owned, non-null process handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(duplicate.cast()) })
}

#[cfg(windows)]
#[derive(Clone, Copy)]
struct ProcessTimes {
    creation_time: u64,
    exit_time: u64,
}

#[cfg(windows)]
fn file_time_to_u64(time: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

#[cfg(windows)]
fn process_times(process: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<ProcessTimes> {
    use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: callers keep `process` valid and all four FILETIME outputs are writable.
    if unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let creation_time = file_time_to_u64(creation);
    if creation_time == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Windows process has no creation time",
        ));
    }
    Ok(ProcessTimes {
        creation_time,
        exit_time: file_time_to_u64(exit),
    })
}

#[cfg(windows)]
fn terminate_windows_job(state: &WindowsJobState) -> std::io::Result<()> {
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;

    // SAFETY: the state owns a live Job handle for the duration of this call.
    if unsafe { TerminateJobObject(state.job.as_raw_handle() as _, 1) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn traceable_processes(
    parents: &HashMap<u32, u32>,
    root_pid: u32,
    retained: &BTreeSet<u32>,
) -> BTreeSet<u32> {
    parents
        .keys()
        .copied()
        .filter(|pid| {
            *pid != root_pid
                && !retained.contains(pid)
                && process_reaches_owned_anchor(*pid, parents, root_pid, retained)
        })
        .collect()
}

#[cfg(windows)]
fn process_reaches_owned_anchor(
    pid: u32,
    parents: &HashMap<u32, u32>,
    root_pid: u32,
    retained: &BTreeSet<u32>,
) -> bool {
    let mut current = pid;
    let mut visited = BTreeSet::new();
    while visited.insert(current) {
        let Some(parent) = parents.get(&current).copied() else {
            return false;
        };
        if parent == root_pid || retained.contains(&parent) {
            return true;
        }
        current = parent;
    }
    false
}

#[cfg(windows)]
fn validate_generation_edge(
    parent_creation_time: u64,
    parent_exit_time: Option<u64>,
    child_creation_time: u64,
) -> std::io::Result<()> {
    if parent_creation_time > child_creation_time {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "child process generation predates its parent generation",
        ));
    }
    if let Some(exit_time) = parent_exit_time {
        if exit_time < parent_creation_time {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "parent process exit time predates its creation time",
            ));
        }
        if child_creation_time > exit_time {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "child process generation was created after its parent generation exited",
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn parent_generation_exit_time(parent: &ProcessGeneration) -> std::io::Result<Option<u64>> {
    let exit_time = match process_handle_signaled(&parent.handle)? {
        false => None,
        true => {
            let times = process_times(parent.handle.as_raw_handle() as _)?;
            if times.creation_time != parent.key.creation_time {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "owned parent process creation time changed",
                ));
            }
            if times.exit_time == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "signaled parent process has no exit time",
                ));
            }
            Some(times.exit_time)
        }
    };
    Ok(exit_time)
}

#[cfg(windows)]
fn validate_candidate_parent(
    state: &WindowsJobState,
    capture: &WindowsCaptureState,
    parent_pid: u32,
    child_creation_time: u64,
) -> std::io::Result<()> {
    let parents = std::iter::once(&state.root)
        .chain(capture.retained.values())
        .filter(|generation| generation.key.pid == parent_pid);
    let mut matched = false;
    let mut last_mismatch = None;
    for parent in parents {
        let exit_time = parent_generation_exit_time(parent)?;
        match validate_generation_edge(parent.key.creation_time, exit_time, child_creation_time) {
            Ok(()) if matched => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("multiple owned process generations match parent PID {parent_pid}"),
                ));
            }
            Ok(()) => matched = true,
            Err(error) => last_mismatch = Some(error),
        }
    }
    if matched {
        Ok(())
    } else {
        Err(last_mismatch.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("parent PID {parent_pid} is not an owned process generation"),
            )
        }))
    }
}

#[cfg(windows)]
fn snapshot_process_parents() -> std::io::Result<HashMap<u32, u32>> {
    use windows_sys::Win32::{
        Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: Toolhelp returned a new valid snapshot handle.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot.cast()) };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let raw_snapshot = snapshot.as_raw_handle() as _;
    if unsafe { Process32FirstW(raw_snapshot, &mut entry) } == 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
            Ok(HashMap::new())
        } else {
            Err(error)
        };
    }

    let mut parents = HashMap::new();
    loop {
        parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        if unsafe { Process32NextW(raw_snapshot, &mut entry) } != 0 {
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
            return Ok(parents);
        }
        return Err(error);
    }
}

#[cfg(windows)]
fn open_and_revalidate_process(
    pid: u32,
    expected_parent_pid: u32,
    state: &WindowsJobState,
    capture: &WindowsCaptureState,
) -> std::io::Result<ProcessGeneration> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    };

    const SYNCHRONIZE: u32 = 0x0010_0000;
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE,
            0,
            pid,
        )
    };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `OpenProcess` returned a new owned, non-null process handle.
    let process = unsafe { OwnedHandle::from_raw_handle(process.cast()) };
    let generation = ProcessGeneration::from_owned_handle(process)?;
    if generation.key.pid != pid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "opened process identity changed from PID {pid} to {}",
                generation.key.pid
            ),
        ));
    }

    let current = snapshot_process_parents()?;
    let current_parent_pid = current.get(&pid).copied().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("process PID {pid} disappeared during generation validation"),
        )
    })?;
    if current_parent_pid != expected_parent_pid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "process PID {pid} changed parent from {expected_parent_pid} to {current_parent_pid}"
            ),
        ));
    }
    validate_candidate_parent(
        state,
        capture,
        current_parent_pid,
        generation.key.creation_time,
    )?;
    Ok(generation)
}

#[cfg(windows)]
#[derive(Default)]
struct WindowsDiscoveryOutcome {
    added: usize,
    first_error: Option<std::io::Error>,
}

#[cfg(windows)]
fn discover_and_retain_processes(
    state: &WindowsJobState,
    deadline: Option<std::time::Instant>,
) -> WindowsDiscoveryOutcome {
    let mut capture = lock(&state.capture);
    let mut outcome = WindowsDiscoveryOutcome::default();
    if let Err(error) = check_windows_cleanup_deadline(deadline) {
        outcome.first_error = Some(error);
        preserve_discovery_error(&mut capture, &outcome);
        return outcome;
    }
    let parents = match snapshot_process_parents() {
        Ok(parents) => parents,
        Err(error) => {
            outcome.first_error = Some(error);
            preserve_discovery_error(&mut capture, &outcome);
            return outcome;
        }
    };
    if let Err(error) = check_windows_cleanup_deadline(deadline) {
        outcome.first_error = Some(error);
        preserve_discovery_error(&mut capture, &outcome);
        return outcome;
    }
    let retained_pids = capture
        .retained
        .keys()
        .map(|generation| generation.pid)
        .collect::<BTreeSet<_>>();
    let mut candidates = traceable_processes(&parents, state.root.key.pid, &retained_pids);
    let mut anchor_pids = retained_pids;
    anchor_pids.insert(state.root.key.pid);
    loop {
        let ready = candidates
            .iter()
            .copied()
            .filter(|pid| {
                parents
                    .get(pid)
                    .is_some_and(|parent_pid| anchor_pids.contains(parent_pid))
            })
            .collect::<Vec<_>>();
        if ready.is_empty() {
            break;
        }
        for pid in ready {
            candidates.remove(&pid);
            if let Err(error) = check_windows_cleanup_deadline(deadline) {
                preserve_first_error(&mut outcome.first_error, error);
                break;
            }
            let parent_pid = parents[&pid];
            match open_and_revalidate_process(pid, parent_pid, state, &capture) {
                Ok(generation) => {
                    let key = generation.key;
                    if capture.retained.insert(key, generation).is_none() {
                        outcome.added += 1;
                    }
                    anchor_pids.insert(pid);
                }
                Err(error) => preserve_first_error(&mut outcome.first_error, error),
            }
            if let Err(error) = check_windows_cleanup_deadline(deadline) {
                preserve_first_error(&mut outcome.first_error, error);
                break;
            }
        }
        if outcome
            .first_error
            .as_ref()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
        {
            break;
        }
    }
    if !candidates.is_empty() {
        preserve_first_error(
            &mut outcome.first_error,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "candidate process ancestry could not be validated parent-first",
            ),
        );
    }
    preserve_discovery_error(&mut capture, &outcome);
    outcome
}

#[cfg(windows)]
fn preserve_discovery_error(capture: &mut WindowsCaptureState, outcome: &WindowsDiscoveryOutcome) {
    if capture.first_error.is_none() {
        if let Some(error) = outcome.first_error.as_ref() {
            capture.first_error = Some(WindowsCaptureError::from_error(error));
        }
    }
}

#[cfg(windows)]
fn preserve_first_capture_error(state: &WindowsJobState, error: &std::io::Error) {
    let mut capture = lock(&state.capture);
    if capture.first_error.is_none() {
        capture.first_error = Some(WindowsCaptureError::from_error(error));
    }
}

#[cfg(windows)]
fn preserve_first_error(slot: &mut Option<std::io::Error>, error: std::io::Error) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

#[cfg(windows)]
fn blocking_task_error(context: &str, error: tokio::task::JoinError) -> std::io::Error {
    let failure = if error.is_panic() {
        "panicked"
    } else if error.is_cancelled() {
        "was cancelled"
    } else {
        "failed"
    };
    std::io::Error::other(format!("{context} blocking task {failure}: {error}"))
}

#[cfg(windows)]
fn check_windows_cleanup_deadline(deadline: Option<std::time::Instant>) -> std::io::Result<()> {
    if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
        Err(windows_cleanup_timeout())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_cleanup_timeout() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "Windows process tree did not exit before the cleanup deadline",
    )
}

#[cfg(windows)]
fn query_windows_job_active_processes(state: &WindowsJobState) -> std::io::Result<u32> {
    use windows_sys::Win32::System::JobObjects::{
        JobObjectBasicAccountingInformation, QueryInformationJobObject,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    };

    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: the state owns the Job handle and `info` is writable for its exact size.
    if unsafe {
        QueryInformationJobObject(
            state.job.as_raw_handle() as _,
            JobObjectBasicAccountingInformation,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of_val(&info) as u32,
            std::ptr::null_mut(),
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(info.ActiveProcesses)
    }
}

#[cfg(windows)]
fn process_handle_signaled(process: &OwnedHandle) -> std::io::Result<bool> {
    wait_for_process_handle(process, 0).map(|result| result == WindowsWaitResult::Signaled)
}

#[cfg(windows)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum WindowsWaitResult {
    Signaled,
    TimedOut,
}

#[cfg(windows)]
fn normalize_terminate_failure(
    termination_error: std::io::Error,
    immediate_wait: std::io::Result<WindowsWaitResult>,
) -> Option<std::io::Error> {
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

    if termination_error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
        && matches!(immediate_wait, Ok(WindowsWaitResult::Signaled))
    {
        None
    } else {
        Some(termination_error)
    }
}

#[cfg(windows)]
fn wait_for_process_handle(
    process: &OwnedHandle,
    timeout_millis: u32,
) -> std::io::Result<WindowsWaitResult> {
    use windows_sys::Win32::{
        Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::WaitForSingleObject,
    };

    // SAFETY: `process` owns a live synchronizable handle for the duration of the wait.
    match unsafe { WaitForSingleObject(process.as_raw_handle() as _, timeout_millis) } {
        WAIT_OBJECT_0 => Ok(WindowsWaitResult::Signaled),
        WAIT_TIMEOUT => Ok(WindowsWaitResult::TimedOut),
        WAIT_FAILED => Err(std::io::Error::last_os_error()),
        result => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected process wait result {result}"),
        )),
    }
}

#[cfg(windows)]
fn remaining_wait_millis(deadline: std::time::Instant) -> Option<u32> {
    let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
    wait_millis_for_remaining_duration(remaining)
}

#[cfg(windows)]
fn wait_millis_for_remaining_duration(remaining: Duration) -> Option<u32> {
    let millis = remaining.as_millis();
    if millis == 0 {
        None
    } else {
        Some(millis.min((u32::MAX - 1) as u128) as u32)
    }
}

#[cfg(windows)]
fn terminate_and_wait_retained_processes(
    state: &WindowsJobState,
    deadline: std::time::Instant,
) -> (bool, Option<std::io::Error>) {
    use windows_sys::Win32::System::Threading::TerminateProcess;

    let capture = lock(&state.capture);
    let mut first_error = None;
    for generation in capture.retained.values() {
        match process_handle_signaled(&generation.handle) {
            Ok(true) => {}
            Ok(false) => {
                // SAFETY: the retained handle has PROCESS_TERMINATE access and remains owned.
                if unsafe { TerminateProcess(generation.handle.as_raw_handle() as _, 1) } == 0 {
                    let termination_error = std::io::Error::last_os_error();
                    let immediate_wait = wait_for_process_handle(&generation.handle, 0);
                    if let Some(error) =
                        normalize_terminate_failure(termination_error, immediate_wait)
                    {
                        preserve_first_error(&mut first_error, error);
                    }
                }
            }
            Err(error) => preserve_first_error(&mut first_error, error),
        }
    }

    let mut all_signaled = true;
    for generation in capture.retained.values() {
        match process_handle_signaled(&generation.handle) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                all_signaled = false;
                preserve_first_error(&mut first_error, error);
                continue;
            }
        }
        let Some(timeout_millis) = remaining_wait_millis(deadline) else {
            all_signaled = false;
            preserve_first_error(&mut first_error, windows_cleanup_timeout());
            continue;
        };
        match wait_for_process_handle(&generation.handle, timeout_millis) {
            Ok(WindowsWaitResult::Signaled) => {}
            Ok(WindowsWaitResult::TimedOut) => {
                all_signaled = false;
                preserve_first_error(&mut first_error, windows_cleanup_timeout());
            }
            Err(error) => {
                all_signaled = false;
                preserve_first_error(&mut first_error, error);
            }
        }
    }
    (all_signaled, first_error)
}

#[cfg(windows)]
fn cleanup_windows_process_tree(state: &WindowsJobState) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut first_error = lock(&state.capture)
        .first_error
        .as_ref()
        .map(WindowsCaptureError::to_error);

    loop {
        let discovery = discover_and_retain_processes(state, Some(deadline));
        let discovery_complete = discovery.first_error.is_none();
        preserve_optional_error(&mut first_error, discovery.first_error);

        if let Err(error) = terminate_windows_job(state) {
            preserve_first_error(&mut first_error, error);
        }
        if let Err(error) = check_windows_cleanup_deadline(Some(deadline)) {
            preserve_first_error(&mut first_error, error);
        }
        let (all_retained_signaled, retained_error) =
            terminate_and_wait_retained_processes(state, deadline);
        preserve_optional_error(&mut first_error, retained_error);
        if let Err(error) = check_windows_cleanup_deadline(Some(deadline)) {
            preserve_first_error(&mut first_error, error);
        }

        let job_is_empty = match query_windows_job_active_processes(state) {
            Ok(0) => true,
            Ok(_) => false,
            Err(error) => {
                preserve_first_error(&mut first_error, error);
                false
            }
        };
        if let Err(error) = check_windows_cleanup_deadline(Some(deadline)) {
            preserve_first_error(&mut first_error, error);
        }

        if discovery_complete && discovery.added == 0 && all_retained_signaled && job_is_empty {
            let final_discovery = discover_and_retain_processes(state, Some(deadline));
            let final_complete = final_discovery.first_error.is_none();
            preserve_optional_error(&mut first_error, final_discovery.first_error);
            if final_complete && final_discovery.added == 0 {
                return match first_error {
                    Some(error) => Err(error),
                    None => Ok(()),
                };
            }
        }

        if std::time::Instant::now() >= deadline {
            preserve_first_error(&mut first_error, windows_cleanup_timeout());
            return Err(first_error.expect("cleanup deadline always supplies an error"));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(Duration::from_millis(1).min(remaining));
    }
}

#[cfg(windows)]
fn preserve_optional_error(first: &mut Option<std::io::Error>, next: Option<std::io::Error>) {
    if let Some(error) = next {
        preserve_first_error(first, error);
    }
}

#[cfg(windows)]
fn resume_suspended_process(pid: u32) -> std::io::Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                THREADENTRY32,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut found = None;
    let mut present = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while present {
        if entry.th32OwnerProcessID == pid {
            found = Some(entry.th32ThreadID);
            break;
        }
        present = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    let thread_id = found.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "suspended process primary thread was not found",
        )
    })?;
    let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
    if thread.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let previous = unsafe { ResumeThread(thread) };
    let resume_error = (previous == u32::MAX).then(std::io::Error::last_os_error);
    unsafe { CloseHandle(thread) };
    if let Some(error) = resume_error {
        return Err(error);
    }
    if previous != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("suspended process thread had unexpected suspend count {previous}"),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn terminate_and_reap_tokio_child(child: &mut Child) {
    let _ = child.start_kill();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

#[cfg(windows)]
fn terminate_and_wait_process(process: windows_sys::Win32::Foundation::HANDLE) {
    use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject, INFINITE};
    unsafe {
        TerminateProcess(process, 1);
        WaitForSingleObject(process, INFINITE);
    }
}

/// Handle to one owned child process group.
#[derive(Clone)]
pub struct Subprocess {
    inner: Arc<ProcessInner>,
}

impl fmt::Debug for Subprocess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Subprocess")
            .field("pid", &self.pid())
            .field("done", &self.done())
            .finish()
    }
}

impl Subprocess {
    pub fn pid(&self) -> u32 {
        lock(&self.inner.state).pid
    }

    /// Returns exit facts once, without consuming them.
    pub fn done(&self) -> Option<ProcessDone> {
        lock(&self.inner.state).done.clone()
    }

    pub fn stdout(&self) -> Option<ProcessOutputSnapshot> {
        let state = lock(&self.inner.state);
        state.done.as_ref()?;
        Some(state.stdout.snapshot())
    }

    pub fn stderr(&self) -> Option<ProcessOutputSnapshot> {
        let state = lock(&self.inner.state);
        state.done.as_ref()?;
        Some(state.stderr.snapshot())
    }

    /// Waits for process-tree exit and output-drain completion.
    pub async fn wait(&self) -> ProcessDone {
        loop {
            let notified = self.inner.done.notified();
            if let Some(done) = self.done() {
                return done;
            }
            notified.await;
        }
    }

    /// Starts first-wins timeout termination, then waits for the complete tree.
    pub async fn wait_timeout(&self, timeout: Duration) -> ProcessDone {
        let notified = self.inner.done.notified();
        if let Some(done) = self.done() {
            return done;
        }
        tokio::select! {
            _ = notified => self.wait().await,
            _ = time::sleep(timeout) => {
                self.stop(ProcessTermination::TimedOut, self.grace_or_default()).await;
                self.wait().await
            }
        }
    }

    /// SIGTERMs the Unix process group, waits `grace`, SIGKILLs if needed, and
    /// waits for all output handles to close.
    pub async fn terminate(&self, grace: Duration) -> ProcessDone {
        self.stop(ProcessTermination::Terminated, grace).await;
        self.wait().await
    }

    /// Abort is an explicit first-cause termination, not a dropped future.
    pub async fn abort(&self, grace: Duration) -> ProcessDone {
        self.stop(ProcessTermination::Aborted, grace).await;
        self.wait().await
    }

    /// Reads a completed output stream without advancing any cursor.
    pub async fn read_stdout(
        &self,
        offset: u64,
        max_bytes: usize,
    ) -> Result<ProcessOutputRead, TessivumError> {
        self.read_output(true, offset, max_bytes).await
    }

    /// Reads a completed output stream without advancing any cursor.
    pub async fn read_stderr(
        &self,
        offset: u64,
        max_bytes: usize,
    ) -> Result<ProcessOutputRead, TessivumError> {
        self.read_output(false, offset, max_bytes).await
    }

    async fn read_output(
        &self,
        stdout: bool,
        offset: u64,
        max_bytes: usize,
    ) -> Result<ProcessOutputRead, TessivumError> {
        if max_bytes > MAX_READ_BYTES {
            return Err(process_error(
                "SUBPROCESS_OUTPUT_TOO_LARGE",
                "requested output range exceeds the limit",
                json!({"maxBytes": max_bytes, "limit": MAX_READ_BYTES}),
            ));
        }
        let captured = {
            let state = lock(&self.inner.state);
            if state.done.is_none() {
                return Err(process_error(
                    "SUBPROCESS_NOT_EXITED",
                    "output offsets are available only after process exit",
                    json!({"pid": state.pid}),
                ));
            }
            if stdout {
                state.stdout.clone()
            } else {
                state.stderr.clone()
            }
        };
        if offset > captured.total_bytes {
            return Err(process_error(
                "SUBPROCESS_INVALID_OFFSET",
                "output offset exceeds the completed stream length",
                json!({"offset": offset, "totalBytes": captured.total_bytes}),
            ));
        }
        let wanted = max_bytes.min((captured.total_bytes - offset) as usize);
        let bytes = if wanted == 0 {
            Vec::new()
        } else if let Some(path) = &captured.spill_path {
            read_spill(path, offset, wanted).await?
        } else {
            let available_from = captured
                .total_bytes
                .saturating_sub(captured.tail.len() as u64);
            if offset < available_from {
                return Err(process_error(
                    "SUBPROCESS_OUTPUT_TRUNCATED",
                    "requested output range is no longer retained",
                    json!({"offset": offset, "availableFrom": available_from}),
                ));
            }
            let start = (offset - available_from) as usize;
            captured.tail[start..start + wanted].to_vec()
        };
        Ok(ProcessOutputRead {
            offset,
            next_offset: offset + bytes.len() as u64,
            total_bytes: captured.total_bytes,
            bytes,
        })
    }

    async fn stop(&self, cause: ProcessTermination, grace: Duration) {
        let (pid, first) = {
            let mut state = lock(&self.inner.state);
            if state.done.is_some() {
                return;
            }
            let first = state.termination.is_none();
            if first {
                state.termination = Some(cause);
            }
            (state.pid, first)
        };
        if !first {
            return;
        }
        #[cfg(unix)]
        let _ = signal_tree(pid, libc::SIGTERM);
        #[cfg(windows)]
        if let Some(job) = lock(&self.inner.job).as_ref() {
            job.terminate();
        }
        let notified = self.inner.done.notified();
        if self.done().is_none() {
            tokio::select! {
                _ = notified => return,
                _ = time::sleep(grace) => {}
            }
        }
        if self.done().is_none() {
            force_terminate_tree(pid);
        }
    }

    fn grace_or_default(&self) -> Duration {
        // The request grace is applied by the caller that owns scheduling. A
        // conservative default protects direct timeout users as well.
        Duration::from_millis(500)
    }
}

#[derive(Default)]
struct RuntimeInner {
    children: Mutex<HashMap<u32, Arc<ProcessInner>>>,
}
#[cfg(windows)]
impl Drop for RuntimeInner {
    fn drop(&mut self) {
        for inner in lock(&self.children).values() {
            let mut state = lock(&inner.state);
            if state.done.is_none() && state.termination.is_none() {
                state.termination = Some(ProcessTermination::Shutdown);
            }
            drop(state);
            if let Some(job) = lock(&inner.job).as_ref() {
                job.terminate();
            }
        }
    }
}
struct ReapStreams {
    input: Option<Vec<u8>>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    stdout_policy: Option<CaptureOutput>,
    stderr_policy: Option<CaptureOutput>,
}

/// Process-local owner for all spawned child trees.
#[derive(Clone, Default)]
pub struct SubprocessRuntime {
    inner: Arc<RuntimeInner>,
}

impl fmt::Debug for SubprocessRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubprocessRuntime")
            .field("children", &lock(&self.inner.children).len())
            .finish()
    }
}

impl SubprocessRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(
        &self,
        context: &ContextHandle,
    ) -> Result<ServiceHandle<SubprocessRuntime>, CoreError> {
        context.provide(subprocess_service_key(), self.clone())
    }

    /// Resolves and spawns a literal argv process. This method never invokes a
    /// shell or parses a command string.
    pub async fn spawn(&self, request: SubprocessRequest) -> Result<Subprocess, TessivumError> {
        validate_request(&request)?;
        let program = resolve_program(&request.argv[0])?;
        let mut command = Command::new(program);
        command.args(&request.argv[1..]);
        if let Some(cwd) = &request.cwd {
            command.current_dir(canonical_cwd(cwd)?);
        }
        configure_environment(&mut command, &request.env)?;
        configure_stdio(&mut command, &request)?;
        configure_process_group(&mut command);
        #[cfg(not(windows))]
        let mut child = command.spawn().map_err(|error| {
            process_error(
                "SUBPROCESS_SPAWN_FAILED",
                "subprocess could not be spawned",
                json!({"program": request.argv[0], "error": error.to_string()}),
            )
        })?;
        #[cfg(windows)]
        let (mut child, job) = WindowsJob::spawn(&mut command).map_err(|error| {
            process_error(
                "SUBPROCESS_SPAWN_FAILED",
                "subprocess could not be spawned into a managed Windows Job Object",
                json!({"program": request.argv[0], "error": error.to_string()}),
            )
        })?;
        let pid = child.id().ok_or_else(|| {
            process_error(
                "SUBPROCESS_SPAWN_FAILED",
                "spawned subprocess did not report a process identifier",
                json!({"program": request.argv[0]}),
            )
        })?;
        let streams = ReapStreams {
            input: match request.stdin {
                ProcessStdin::Bytes(bytes) => Some(bytes),
                _ => None,
            },
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            stdout_policy: capture_policy(&request.stdout),
            stderr_policy: capture_policy(&request.stderr),
        };
        let inner = Arc::new(ProcessInner {
            state: Mutex::new(ProcessState {
                pid,
                termination: None,
                done: None,
                stdout: CapturedOutput::empty(),
                stderr: CapturedOutput::empty(),
            }),
            #[cfg(windows)]
            job: Mutex::new(Some(job)),
            done: Notify::new(),
        });
        lock(&self.inner.children).insert(pid, Arc::clone(&inner));
        let weak_runtime = Arc::downgrade(&self.inner);
        tokio::spawn(reap_child(child, streams, Arc::clone(&inner), weak_runtime));
        Ok(Subprocess { inner })
    }

    /// Terminates and joins every still-owned child tree.
    pub async fn shutdown(&self) {
        let children: Vec<_> = lock(&self.inner.children).values().cloned().collect();
        let handles: Vec<_> = children
            .into_iter()
            .map(|inner| async move {
                let process = Subprocess { inner };
                let _ = process
                    .stop(ProcessTermination::Shutdown, Duration::from_millis(500))
                    .await;
                process.wait().await
            })
            .collect();
        let _ = join_all(handles).await;
    }
}

/// Caller-owned authority check performed before every persistent shell command.
/// A stale workspace must return an error rather than reuse an old shell.
#[cfg(any(unix, windows))]
pub type PersistentShellLeaseValidator =
    Arc<dyn Fn() -> Result<(), TessivumError> + Send + Sync + 'static>;

/// Immutable spawn plan for one persistent platform shell.
#[cfg(any(unix, windows))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistentShellConfig {
    /// Fixed shell or sandbox-wrapper argv.
    pub argv: Vec<String>,
    /// Fallback workspace path used when no validated directory descriptor is supplied.
    pub workspace: PathBuf,
    /// A caller-validated directory descriptor used for the initial child cwd.
    #[cfg(unix)]
    pub cwd_fd: Option<RawFd>,
    /// Explicit environment additions/removals after the normal ambient-secret scrub.
    pub env: BTreeMap<String, Option<String>>,
    /// Per-stream in-memory tail limit for one command.
    pub max_output_bytes: usize,
    pub terminate_grace: Duration,
}

#[cfg(any(unix, windows))]
impl PersistentShellConfig {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        #[cfg(unix)]
        let argv = vec!["/bin/sh".into(), "-s".into()];
        #[cfg(windows)]
        let argv = vec![
            "powershell.exe".into(),
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "-".into(),
        ];
        Self {
            argv,
            workspace: workspace.into(),
            #[cfg(unix)]
            cwd_fd: None,
            env: BTreeMap::new(),
            max_output_bytes: DEFAULT_TAIL_BYTES,
            terminate_grace: Duration::from_millis(500),
        }
    }
}

/// One request evaluated by a [`PersistentShell`].
#[cfg(any(unix, windows))]
#[derive(Clone, Debug)]
pub struct PersistentShellCommand {
    pub script: String,
    pub timeout: Duration,
    pub cancellation: Option<CancellationToken>,
}

#[cfg(any(unix, windows))]
impl PersistentShellCommand {
    pub fn new(script: impl Into<String>) -> Self {
        Self {
            script: script.into(),
            timeout: Duration::from_secs(30),
            cancellation: None,
        }
    }

    pub fn cancelled_by(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

/// Completed bounded output from one persistent shell command.
#[cfg(any(unix, windows))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistentShellResult {
    pub exit_code: i32,
    pub stdout: ProcessOutputSnapshot,
    pub stderr: ProcessOutputSnapshot,
}

#[cfg(any(unix, windows))]
struct PersistentShellCommandState {
    nonce: String,
    state: Mutex<PersistentShellCommandCapture>,
    done: Notify,
}

#[cfg(any(unix, windows))]
struct PersistentShellCommandCapture {
    stdout: CapturedOutput,
    stderr: CapturedOutput,
    stdout_status: Option<i32>,
    stderr_status: Option<i32>,
    error: Option<TessivumError>,
}

#[cfg(any(unix, windows))]
impl PersistentShellCommandState {
    fn new(nonce: String, max_output_bytes: usize) -> Self {
        Self {
            nonce,
            state: Mutex::new(PersistentShellCommandCapture {
                stdout: CapturedOutput {
                    tail: Vec::with_capacity(max_output_bytes.min(8192)),
                    total_bytes: 0,
                    spill_path: None,
                    spill_error: None,
                },
                stderr: CapturedOutput {
                    tail: Vec::with_capacity(max_output_bytes.min(8192)),
                    total_bytes: 0,
                    spill_path: None,
                    spill_error: None,
                },
                stdout_status: None,
                stderr_status: None,
                error: None,
            }),
            done: Notify::new(),
        }
    }

    fn append(&self, stdout: bool, bytes: &[u8], max_output_bytes: usize) {
        let mut state = lock(&self.state);
        let output = if stdout {
            &mut state.stdout
        } else {
            &mut state.stderr
        };
        output.total_bytes += bytes.len() as u64;
        push_tail(&mut output.tail, bytes, max_output_bytes);
    }

    fn mark(&self, stdout: bool, status: i32) {
        let mut state = lock(&self.state);
        if stdout {
            state.stdout_status = Some(status);
        } else {
            state.stderr_status = Some(status);
        }
        self.done.notify_waiters();
    }

    fn fail(&self, error: TessivumError) -> bool {
        let mut state = lock(&self.state);
        if state.stdout_status.is_some()
            && state.stderr_status.is_some()
            && state.stdout_status == state.stderr_status
        {
            return false;
        }
        if state.error.is_none() {
            state.error = Some(error);
            self.done.notify_waiters();
        }
        true
    }

    fn result(&self) -> Option<Result<PersistentShellResult, TessivumError>> {
        let state = lock(&self.state);
        if let Some(error) = &state.error {
            return Some(Err(error.clone()));
        }
        match (state.stdout_status, state.stderr_status) {
            (Some(stdout_status), Some(stderr_status)) if stdout_status == stderr_status => {
                Some(Ok(PersistentShellResult {
                    exit_code: stdout_status,
                    stdout: state.stdout.snapshot(),
                    stderr: state.stderr.snapshot(),
                }))
            }
            (Some(_), Some(_)) => Some(Err(persistent_shell_error(
                "PERSISTENT_SHELL_PROTOCOL",
                "persistent shell completion status disagreed across output streams",
                json!({}),
            ))),
            _ => None,
        }
    }

    async fn wait(&self) -> Result<PersistentShellResult, TessivumError> {
        loop {
            let notified = self.done.notified();
            if let Some(result) = self.result() {
                return result;
            }
            notified.await;
        }
    }
}

/// Session-owned reusable platform shell with a fixed canonical workspace.
///
/// Commands are serialized. Output frames are random per command and removed
/// from the captured stream; an EOF, replaced shell, or closed stdin fails the
/// current command and permanently retires the instance rather than waiting for
/// a marker that cannot arrive.
#[cfg(any(unix, windows))]
#[cfg_attr(unix, derive(Clone))]
pub struct PersistentShell {
    inner: Arc<PersistentShellInner>,
}

#[cfg(windows)]
impl Clone for PersistentShell {
    fn clone(&self) -> Self {
        self.inner.owners.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[cfg(windows)]
impl Drop for PersistentShell {
    fn drop(&mut self) {
        if self.inner.owners.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        self.inner.disposed.store(true, Ordering::Release);
        self.inner.dispose_signal.notify_waiters();
        self.inner.request_termination(ProcessTermination::Shutdown);
        if let Some(job) = lock(&self.inner.job).as_ref() {
            job.terminate();
        }
    }
}

#[cfg(any(unix, windows))]
struct PersistentShellInner {
    #[cfg(windows)]
    owners: AtomicUsize,
    pid: u32,
    validator: PersistentShellLeaseValidator,
    max_output_bytes: usize,
    terminate_grace: Duration,
    stdin: AsyncMutex<Option<ChildStdin>>,
    serial: AsyncMutex<()>,
    active: Mutex<Option<Arc<PersistentShellCommandState>>>,
    termination: Mutex<Option<ProcessTermination>>,
    #[cfg(windows)]
    job: Mutex<Option<WindowsJob>>,
    #[cfg(windows)]
    cleanup_request: Notify,
    done_state: Mutex<Option<ProcessDone>>,
    done: Notify,
    closed: AtomicBool,
    disposed: AtomicBool,
    dispose_signal: Notify,
    dispose_gate: AsyncMutex<()>,
}

#[cfg(any(unix, windows))]
impl fmt::Debug for PersistentShell {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersistentShell")
            .field("pid", &self.inner.pid)
            .field("closed", &self.inner.closed.load(Ordering::Acquire))
            .finish()
    }
}

#[cfg(any(unix, windows))]
impl PersistentShell {
    /// Starts exactly one platform shell using the supplied canonical workspace
    /// plan. The lease validator is required and runs before the initial spawn
    /// and every later command.
    pub async fn start(
        config: PersistentShellConfig,
        validate_lease: impl Fn() -> Result<(), TessivumError> + Send + Sync + 'static,
    ) -> Result<Self, TessivumError> {
        validate_persistent_shell_config(&config)?;
        let validator: PersistentShellLeaseValidator = Arc::new(validate_lease);
        validator()?;
        #[cfg(unix)]
        let cwd = config
            .cwd_fd
            .is_none()
            .then(|| canonical_cwd(&config.workspace))
            .transpose()?;
        let program = config.argv[0].clone();
        let mut command = Command::new(&program);
        command.args(&config.argv[1..]);
        #[cfg(unix)]
        if let Some(directory) = config.cwd_fd {
            use std::os::unix::process::CommandExt;

            unsafe {
                command.as_std_mut().pre_exec(move || {
                    if libc::fchdir(directory) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
        } else {
            command.current_dir(cwd.expect("cwd is present without a directory descriptor"));
        }
        #[cfg(windows)]
        command.current_dir(canonical_cwd(&config.workspace)?);
        configure_environment(&mut command, &config.env)?;
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        configure_process_group(&mut command);
        #[cfg(not(windows))]
        let mut child = command.spawn().map_err(|error| {
            persistent_shell_error(
                "PERSISTENT_SHELL_UNAVAILABLE",
                "persistent shell could not be spawned",
                json!({"program": program, "error": error.to_string()}),
            )
        })?;
        #[cfg(windows)]
        let (mut child, job) = WindowsJob::spawn(&mut command).map_err(|error| {
            persistent_shell_error(
                "PERSISTENT_SHELL_UNAVAILABLE",
                "persistent PowerShell could not be spawned into a managed Windows Job Object",
                json!({"program": program, "error": error.to_string()}),
            )
        })?;
        let Some(pid) = child.id() else {
            #[cfg(windows)]
            job.terminate();
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(persistent_shell_error(
                "PERSISTENT_SHELL_UNAVAILABLE",
                "persistent shell did not report a process identifier",
                json!({"program": program}),
            ));
        };
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            #[cfg(unix)]
            terminate_persistent_shell_tree(pid, true).await;
            #[cfg(windows)]
            job.terminate();
            let _ = child.wait().await;
            return Err(persistent_shell_error(
                "PERSISTENT_SHELL_UNAVAILABLE",
                "persistent shell did not provide required standard streams",
                json!({"pid": pid}),
            ));
        };
        let inner = Arc::new(PersistentShellInner {
            #[cfg(windows)]
            owners: AtomicUsize::new(1),
            pid,
            validator,
            max_output_bytes: config.max_output_bytes,
            terminate_grace: config.terminate_grace,
            stdin: AsyncMutex::new(Some(stdin)),
            serial: AsyncMutex::new(()),
            active: Mutex::new(None),
            termination: Mutex::new(None),
            done_state: Mutex::new(None),
            #[cfg(windows)]
            job: Mutex::new(Some(job)),
            #[cfg(windows)]
            cleanup_request: Notify::new(),
            done: Notify::new(),
            closed: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            dispose_signal: Notify::new(),
            dispose_gate: AsyncMutex::new(()),
        });
        let stdout_task = tokio::spawn(drain_persistent_shell_stream(
            stdout,
            true,
            Arc::clone(&inner),
        ));
        let stderr_task = tokio::spawn(drain_persistent_shell_stream(
            stderr,
            false,
            Arc::clone(&inner),
        ));
        tokio::spawn(reap_persistent_shell(
            child,
            stdout_task,
            stderr_task,
            Arc::clone(&inner),
        ));
        Ok(Self { inner })
    }

    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    pub(crate) fn is_terminated(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire) || self.inner.disposed.load(Ordering::Acquire)
    }

    pub(crate) fn is_same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Evaluates one script after the lease remains valid. Calls never overlap.
    pub async fn run(
        &self,
        request: PersistentShellCommand,
    ) -> Result<PersistentShellResult, TessivumError> {
        validate_persistent_shell_command(&request)?;
        let cancellation = request.cancellation.clone();
        let operation = tokio::select! {
            biased;
            _ = self.inner.disposed() => return Err(persistent_shell_disposed()),
            _ = optional_cancellation(cancellation.clone()) => return Err(persistent_shell_cancelled()),
            operation = self.inner.serial.lock() => operation,
        };
        let result = self.run_locked(&request).await;
        drop(operation);
        result
    }

    async fn run_locked(
        &self,
        request: &PersistentShellCommand,
    ) -> Result<PersistentShellResult, TessivumError> {
        if self.inner.disposed.load(Ordering::Acquire) {
            return Err(persistent_shell_disposed());
        }
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(persistent_shell_closed());
        }
        if request
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(persistent_shell_cancelled());
        }
        if let Err(error) = (self.inner.validator)() {
            self.inner.stop(ProcessTermination::Terminated).await;
            return Err(error);
        }
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(persistent_shell_closed());
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let command = Arc::new(PersistentShellCommandState::new(
            nonce.clone(),
            self.inner.max_output_bytes,
        ));
        *lock(&self.inner.active) = Some(Arc::clone(&command));
        let frame = persistent_shell_frame(&request.script, &nonce);
        let write = {
            let mut stdin = self.inner.stdin.lock().await;
            match stdin.as_mut() {
                Some(stdin) => match stdin.write_all(frame.as_bytes()).await {
                    Ok(()) => stdin.flush().await,
                    Err(error) => Err(error),
                },
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "persistent shell stdin is closed",
                )),
            }
        };
        if let Err(error) = write {
            let failure = persistent_shell_error(
                "PERSISTENT_SHELL_CLOSED",
                "persistent shell stdin closed before command completion",
                json!({"error": error.to_string()}),
            );
            command.fail(failure.clone());
            self.inner.stop(ProcessTermination::Terminated).await;
            self.inner.clear_active(&command);
            return Err(failure);
        }
        let cancellation = request.cancellation.clone();
        let result = tokio::select! {
            biased;
            _ = self.inner.disposed() => {
                self.inner.stop(ProcessTermination::Shutdown).await;
                Err(persistent_shell_disposed())
            }
            _ = optional_cancellation(cancellation) => {
                self.inner.stop(ProcessTermination::Aborted).await;
                Err(persistent_shell_cancelled())
            }
            _ = time::sleep(request.timeout) => {
                self.inner.stop(ProcessTermination::TimedOut).await;
                Err(persistent_shell_timed_out(request.timeout))
            }
            result = command.wait() => result,
        };
        self.inner.clear_active(&command);
        result
    }

    /// Cancels the active command and its complete process group.
    pub async fn cancel(&self) {
        self.inner.stop(ProcessTermination::Aborted).await;
    }

    /// Idempotently stops the process group and joins its output drainers.
    pub async fn dispose(&self) {
        let _gate = self.inner.dispose_gate.lock().await;
        if self.inner.disposed.swap(true, Ordering::AcqRel) {
            self.inner.wait_closed().await;
            return;
        }
        self.inner.dispose_signal.notify_waiters();
        self.inner.stop(ProcessTermination::Shutdown).await;
    }
}

#[cfg(any(unix, windows))]
impl PersistentShellInner {
    async fn disposed(&self) {
        loop {
            let notified = self.dispose_signal.notified();
            if self.disposed.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
    fn clear_active(&self, command: &Arc<PersistentShellCommandState>) {
        let mut active = lock(&self.active);
        if active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, command))
        {
            *active = None;
        }
    }

    fn append(&self, stdout: bool, bytes: &[u8]) {
        if let Some(command) = lock(&self.active).as_ref() {
            command.append(stdout, bytes, self.max_output_bytes);
        }
    }

    fn mark(&self, stdout: bool, nonce: &str, status: i32) -> bool {
        let Some(command) = lock(&self.active).as_ref().cloned() else {
            return false;
        };
        if command.nonce != nonce {
            return false;
        }
        command.mark(stdout, status);
        true
    }

    fn fail_active(&self, error: TessivumError) -> bool {
        lock(&self.active)
            .as_ref()
            .is_some_and(|command| command.fail(error))
    }

    fn done(&self) -> Option<ProcessDone> {
        lock(&self.done_state).clone()
    }

    async fn wait_closed(&self) {
        loop {
            let notified = self.done.notified();
            if self.done().is_some() {
                return;
            }
            notified.await;
        }
    }

    fn terminated(&self) -> Option<ProcessTermination> {
        *lock(&self.termination)
    }

    fn request_termination(&self, cause: ProcessTermination) -> bool {
        if self.done().is_some() {
            return false;
        }
        let first = {
            let mut termination = lock(&self.termination);
            if termination.is_some() {
                false
            } else {
                *termination = Some(cause);
                true
            }
        };
        if first {
            self.closed.store(true, Ordering::Release);
            self.fail_active(persistent_shell_termination(cause));
            #[cfg(windows)]
            self.cleanup_request.notify_one();
        }
        first
    }

    async fn stop(self: &Arc<Self>, cause: ProcessTermination) {
        if !self.request_termination(cause) {
            self.wait_closed().await;
            return;
        }
        #[cfg(unix)]
        terminate_persistent_shell_tree(self.pid, false).await;
        #[cfg(windows)]
        if let Some(job) = lock(&self.job).as_ref() {
            job.terminate();
        }
        let notified = self.done.notified();
        if self.done().is_none() {
            tokio::select! {
                _ = notified => return,
                _ = time::sleep(self.terminate_grace) => {}
            }
        }
        if self.done().is_none() {
            #[cfg(unix)]
            terminate_persistent_shell_tree(self.pid, true).await;
        }
        self.wait_closed().await;
    }

    fn stream_failed(self: &Arc<Self>, error: TessivumError) {
        if self.done().is_some() {
            return;
        }
        self.closed.store(true, Ordering::Release);
        self.fail_active(error);
        let shell = Arc::clone(self);
        tokio::spawn(async move {
            shell.stop(ProcessTermination::Terminated).await;
        });
    }

    fn complete(&self, done: ProcessDone) {
        self.closed.store(true, Ordering::Release);
        *lock(&self.done_state) = Some(done);
        self.done.notify_waiters();
    }
}

#[cfg(unix)]
async fn reap_persistent_shell(
    mut child: Child,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
    inner: Arc<PersistentShellInner>,
) {
    let status = child.wait().await;
    inner.closed.store(true, Ordering::Release);
    #[cfg(windows)]
    if let Some(job) = lock(&inner.job).take() {
        job.terminate();
    }
    let incomplete = inner.fail_active(persistent_shell_closed());
    #[cfg(windows)]
    let _ = incomplete;
    #[cfg(unix)]
    if incomplete && inner.terminated().is_none() {
        terminate_persistent_shell_tree(inner.pid, false).await;
        time::sleep(inner.terminate_grace).await;
        terminate_persistent_shell_tree(inner.pid, true).await;
    }
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    let (exit_code, signal) = match status {
        Ok(status) => exit_facts(status),
        Err(_) => (None, None),
    };
    inner.complete(ProcessDone {
        exit_code,
        signal,
        termination: inner.terminated(),
    });
}

#[cfg(windows)]
async fn reap_persistent_shell(
    mut child: Child,
    stdout_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
    inner: Arc<PersistentShellInner>,
) {
    let (status, job) = tokio::select! {
        biased;
        _ = inner.cleanup_request.notified() => {
            let job = lock(&inner.job).take();
            if let Some(job) = job.as_ref() {
                let _ = job.capture_and_terminate().await;
            }
            (child.wait().await, job)
        }
        status = child.wait() => (status, lock(&inner.job).take()),
    };
    inner.closed.store(true, Ordering::Release);

    let cleanup_error = match job {
        Some(job) => job.cleanup_process_tree().await.err(),
        None => None,
    };
    let mut cleanup_failed = cleanup_error.is_some();
    if let Some(error) = cleanup_error.as_ref() {
        inner.fail_active(persistent_shell_cleanup(error));
        stdout_task.abort();
        stderr_task.abort();
    }

    let stdout_result = stdout_task.await;
    let stderr_result = stderr_task.await;
    if let Err(error) = stdout_result {
        cleanup_failed = true;
        inner.fail_active(persistent_shell_cleanup(&error));
    }
    if let Err(error) = stderr_result {
        cleanup_failed = true;
        inner.fail_active(persistent_shell_cleanup(&error));
    }
    if !cleanup_failed {
        if let Err(error) = status.as_ref() {
            inner.fail_active(persistent_shell_error(
                "PERSISTENT_SHELL_CLOSED",
                "persistent shell root process wait failed",
                json!({"error": error.to_string()}),
            ));
        }
        inner.fail_active(persistent_shell_closed());
    }

    let (exit_code, signal) = match status {
        Ok(status) => exit_facts(status),
        Err(_) => (None, None),
    };
    inner.complete(ProcessDone {
        exit_code,
        signal,
        termination: inner.terminated(),
    });
}

#[cfg(any(unix, windows))]
async fn drain_persistent_shell_stream<R>(
    mut reader: R,
    stdout: bool,
    inner: Arc<PersistentShellInner>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut parser = PersistentShellFrameParser::default();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                for token in parser.finish() {
                    dispatch_persistent_shell_token(&inner, stdout, token);
                }
                inner.stream_failed(persistent_shell_closed());
                return;
            }
            Ok(count) => {
                for token in parser.feed(&buffer[..count]) {
                    dispatch_persistent_shell_token(&inner, stdout, token);
                }
            }
            Err(error) => {
                inner.stream_failed(persistent_shell_error(
                    "PERSISTENT_SHELL_CLOSED",
                    "persistent shell output stream failed",
                    json!({"stream": if stdout {"stdout"} else {"stderr"}, "error": error.to_string()}),
                ));
                return;
            }
        }
    }
}

#[cfg(any(unix, windows))]
fn dispatch_persistent_shell_token(
    inner: &PersistentShellInner,
    stdout: bool,
    token: PersistentShellStreamToken,
) {
    match token {
        PersistentShellStreamToken::Data(bytes) => inner.append(stdout, &bytes),
        PersistentShellStreamToken::Marker {
            nonce,
            stdout: marker_stdout,
            status,
        } if stdout == marker_stdout && inner.mark(stdout, &nonce, status) => {}
        PersistentShellStreamToken::Marker {
            nonce,
            stdout: marker_stdout,
            status,
        } => inner.append(
            stdout,
            &persistent_shell_marker_bytes(&nonce, marker_stdout, status),
        ),
    }
}

#[cfg(any(unix, windows))]
const PERSISTENT_SHELL_FRAME_PREFIX: &[u8] = b"\x1eTESSIVUM-SHELL:";
#[cfg(any(unix, windows))]
const PERSISTENT_SHELL_FRAME_SUFFIX: &[u8] = b"\x1f\n";
#[cfg(any(unix, windows))]
const MAX_PERSISTENT_SHELL_FRAME_BYTES: usize = 128;

#[cfg(any(unix, windows))]
enum PersistentShellStreamToken {
    Data(Vec<u8>),
    Marker {
        nonce: String,
        stdout: bool,
        status: i32,
    },
}

#[cfg(any(unix, windows))]
#[derive(Default)]
struct PersistentShellFrameParser {
    pending: Vec<u8>,
}

#[cfg(any(unix, windows))]
impl PersistentShellFrameParser {
    fn feed(&mut self, bytes: &[u8]) -> Vec<PersistentShellStreamToken> {
        self.pending.extend_from_slice(bytes);
        let mut tokens = Vec::new();
        loop {
            let Some(start) = find_bytes(&self.pending, PERSISTENT_SHELL_FRAME_PREFIX) else {
                let keep = prefix_suffix_len(&self.pending, PERSISTENT_SHELL_FRAME_PREFIX);
                self.drain_data(self.pending.len().saturating_sub(keep), &mut tokens);
                break;
            };
            if start != 0 {
                self.drain_data(start, &mut tokens);
                continue;
            }
            let after_prefix = &self.pending[PERSISTENT_SHELL_FRAME_PREFIX.len()..];
            let Some(suffix) = find_bytes(after_prefix, PERSISTENT_SHELL_FRAME_SUFFIX) else {
                if self.pending.len() > MAX_PERSISTENT_SHELL_FRAME_BYTES {
                    self.drain_data(
                        self.pending.len() - MAX_PERSISTENT_SHELL_FRAME_BYTES,
                        &mut tokens,
                    );
                }
                break;
            };
            let end =
                PERSISTENT_SHELL_FRAME_PREFIX.len() + suffix + PERSISTENT_SHELL_FRAME_SUFFIX.len();
            if end > MAX_PERSISTENT_SHELL_FRAME_BYTES {
                self.drain_data(1, &mut tokens);
                continue;
            }
            let frame: Vec<_> = self.pending.drain(..end).collect();
            match parse_persistent_shell_marker(&frame) {
                Some((nonce, stdout, status)) => tokens.push(PersistentShellStreamToken::Marker {
                    nonce,
                    stdout,
                    status,
                }),
                None => tokens.push(PersistentShellStreamToken::Data(frame)),
            }
        }
        tokens
    }

    fn finish(&mut self) -> Vec<PersistentShellStreamToken> {
        let bytes = std::mem::take(&mut self.pending);
        if bytes.is_empty() {
            Vec::new()
        } else {
            vec![PersistentShellStreamToken::Data(bytes)]
        }
    }

    fn drain_data(&mut self, count: usize, tokens: &mut Vec<PersistentShellStreamToken>) {
        if count != 0 {
            tokens.push(PersistentShellStreamToken::Data(
                self.pending.drain(..count).collect(),
            ));
        }
    }
}

#[cfg(any(unix, windows))]
fn parse_persistent_shell_marker(bytes: &[u8]) -> Option<(String, bool, i32)> {
    let middle = bytes
        .strip_prefix(PERSISTENT_SHELL_FRAME_PREFIX)?
        .strip_suffix(PERSISTENT_SHELL_FRAME_SUFFIX)?;
    let text = std::str::from_utf8(middle).ok()?;
    let mut fields = text.split(':');
    let nonce = fields.next()?;
    let stdout = match fields.next()? {
        "O" => true,
        "E" => false,
        _ => return None,
    };
    let status = fields.next()?.parse::<i32>().ok()?;
    if fields.next().is_some()
        || nonce.len() != 32
        || !nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !(0..=255).contains(&status)
    {
        return None;
    }
    Some((nonce.to_owned(), stdout, status))
}

#[cfg(any(unix, windows))]
fn persistent_shell_marker_bytes(nonce: &str, stdout: bool, status: i32) -> Vec<u8> {
    let stream = if stdout { "O" } else { "E" };
    format!("\x1eTESSIVUM-SHELL:{nonce}:{stream}:{status}\x1f\n").into_bytes()
}

#[cfg(unix)]
fn persistent_shell_frame(script: &str, nonce: &str) -> String {
    let variable = format!("_tessivum_shell_status_{nonce}");
    format!(
        r#"{{
{script}
}}
{variable}=$?
command printf '\036TESSIVUM-SHELL:{nonce}:O:%s\037\n' "${variable}"
command printf '\036TESSIVUM-SHELL:{nonce}:E:%s\037\n' "${variable}" >&2
"#
    )
}

#[cfg(windows)]
fn persistent_shell_frame(script: &str, nonce: &str) -> String {
    let status = format!("__tessivum_status_{nonce}");
    let succeeded = format!("__tessivum_succeeded_{nonce}");
    let block = format!("__tessivum_block_{nonce}");
    let marker = format!("__tessivum_marker_{nonce}");
    let script = base64_encode(format!("{script}\n${succeeded} = $?").as_bytes());
    format!(
        r#"$OutputEncoding = [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$global:LASTEXITCODE = $null
${status} = 0
${succeeded} = $true
try {{
${block} = [ScriptBlock]::Create([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{script}')))
. ${block}
if ($null -ne $LASTEXITCODE) {{ ${status} = [int]$LASTEXITCODE }} elseif (-not ${succeeded}) {{ ${status} = 1 }}
}} catch {{
[Console]::Error.WriteLine($_.Exception.Message)
${status} = 1
}}
if (${status} -lt 0 -or ${status} -gt 255) {{ ${status} = 1 }}
${marker} = [char]0x1e + 'TESSIVUM-SHELL:{nonce}:O:' + ${status} + [char]0x1f + "`n"
[Console]::Out.Write(${marker})
[Console]::Out.Flush()
${marker} = [char]0x1e + 'TESSIVUM-SHELL:{nonce}:E:' + ${status} + [char]0x1f + "`n"
[Console]::Error.Write(${marker})
[Console]::Error.Flush()
Remove-Variable -Name '{status}','{succeeded}','{block}','{marker}' -ErrorAction SilentlyContinue

"#
    )
}

#[cfg(windows)]
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let bits = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        encoded.push(ALPHABET[((bits >> 18) & 63) as usize] as char);
        encoded.push(ALPHABET[((bits >> 12) & 63) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((bits >> 6) & 63) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(bits & 63) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

#[cfg(unix)]
async fn terminate_persistent_shell_tree(pid: u32, force: bool) {
    let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
    let _ = signal_tree(pid, signal);
}

#[cfg(any(unix, windows))]
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(any(unix, windows))]
fn prefix_suffix_len(bytes: &[u8], prefix: &[u8]) -> usize {
    (1..prefix.len())
        .rev()
        .find(|&length| bytes.ends_with(&prefix[..length]))
        .unwrap_or(0)
}

#[cfg(any(unix, windows))]
async fn optional_cancellation(cancellation: Option<CancellationToken>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(any(unix, windows))]
fn validate_persistent_shell_config(config: &PersistentShellConfig) -> Result<(), TessivumError> {
    let Some(program) = config.argv.first() else {
        return Err(persistent_shell_error(
            "PERSISTENT_SHELL_INVALID_ARGV",
            "persistent shell argv must contain a program",
            json!({}),
        ));
    };
    if program.is_empty()
        || program.contains('\0')
        || config.argv.iter().any(|arg| arg.contains('\0'))
    {
        return Err(persistent_shell_error(
            "PERSISTENT_SHELL_INVALID_ARGV",
            "persistent shell argv entries must be non-empty program text without NUL",
            json!({}),
        ));
    }

    if config.max_output_bytes > MAX_TAIL_BYTES {
        return Err(persistent_shell_error(
            "PERSISTENT_SHELL_OUTPUT_TOO_LARGE",
            "persistent shell output tail exceeds the limit",
            json!({"limit": MAX_TAIL_BYTES}),
        ));
    }
    if config.terminate_grace.is_zero() {
        return Err(persistent_shell_error(
            "PERSISTENT_SHELL_INVALID_GRACE",
            "persistent shell termination grace must be positive",
            json!({}),
        ));
    }
    for (key, value) in &config.env {
        if key.is_empty()
            || key.contains('=')
            || key.contains('\0')
            || value.as_ref().is_some_and(|value| value.contains('\0'))
        {
            return Err(persistent_shell_error(
                "PERSISTENT_SHELL_INVALID_ENV",
                "persistent shell environment contains an invalid key or value",
                json!({"key": key}),
            ));
        }
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn validate_persistent_shell_command(
    request: &PersistentShellCommand,
) -> Result<(), TessivumError> {
    if request.script.contains('\0') {
        return Err(persistent_shell_error(
            "PERSISTENT_SHELL_INVALID_SCRIPT",
            "persistent shell script must not contain NUL",
            json!({}),
        ));
    }
    if request.timeout.is_zero() {
        return Err(persistent_shell_error(
            "PERSISTENT_SHELL_INVALID_TIMEOUT",
            "persistent shell command timeout must be positive",
            json!({}),
        ));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn persistent_shell_error(code: &str, message: &str, details: serde_json::Value) -> TessivumError {
    TessivumError::new(code, message, "persistent-shell", details)
}

#[cfg(any(unix, windows))]
fn persistent_shell_closed() -> TessivumError {
    persistent_shell_error(
        "PERSISTENT_SHELL_CLOSED",
        "persistent shell closed before command completion",
        json!({}),
    )
}

#[cfg(windows)]
fn persistent_shell_cleanup(error: impl fmt::Display) -> TessivumError {
    persistent_shell_error(
        "PERSISTENT_SHELL_CLEANUP",
        "persistent PowerShell process-tree cleanup failed",
        json!({"error": error.to_string()}),
    )
}

#[cfg(any(unix, windows))]
fn persistent_shell_disposed() -> TessivumError {
    persistent_shell_error(
        "PERSISTENT_SHELL_DISPOSED",
        "persistent shell has been disposed",
        json!({}),
    )
}

#[cfg(any(unix, windows))]
fn persistent_shell_cancelled() -> TessivumError {
    persistent_shell_error(
        "PERSISTENT_SHELL_CANCELLED",
        "persistent shell command was cancelled",
        json!({}),
    )
}

#[cfg(any(unix, windows))]
fn persistent_shell_timed_out(timeout: Duration) -> TessivumError {
    persistent_shell_error(
        "PERSISTENT_SHELL_TIMEOUT",
        "persistent shell command timed out",
        json!({"timeoutMs": timeout.as_millis()}),
    )
}

#[cfg(any(unix, windows))]
fn persistent_shell_termination(cause: ProcessTermination) -> TessivumError {
    match cause {
        ProcessTermination::TimedOut => persistent_shell_error(
            "PERSISTENT_SHELL_TIMEOUT",
            "persistent shell command timed out",
            json!({}),
        ),
        ProcessTermination::Aborted => persistent_shell_cancelled(),
        ProcessTermination::Shutdown => persistent_shell_disposed(),
        ProcessTermination::Terminated => persistent_shell_error(
            "PERSISTENT_SHELL_TERMINATED",
            "persistent shell was terminated",
            json!({}),
        ),
    }
}

async fn reap_child(
    mut child: Child,
    streams: ReapStreams,
    inner: Arc<ProcessInner>,
    runtime: Weak<RuntimeInner>,
) {
    let ReapStreams {
        input,
        stdout,
        stderr,
        stdout_policy,
        stderr_policy,
    } = streams;
    let stdin_task = match (child.stdin.take(), input) {
        (Some(mut stdin), Some(input)) => Some(tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        })),
        _ => None,
    };
    let stdout_task = tokio::spawn(collect_stdout(stdout, stdout_policy));
    let stderr_task = tokio::spawn(collect_stderr(stderr, stderr_policy));
    let status = child.wait().await;
    #[cfg(windows)]
    if let Some(job) = lock(&inner.job).take() {
        job.terminate();
    }
    if let Some(task) = stdin_task {
        let _ = task.await;
    }
    let stdout = stdout_task.await.unwrap_or_else(|error| CapturedOutput {
        spill_error: Some(format!("stdout collector failed: {error}")),
        ..CapturedOutput::empty()
    });
    let stderr = stderr_task.await.unwrap_or_else(|error| CapturedOutput {
        spill_error: Some(format!("stderr collector failed: {error}")),
        ..CapturedOutput::empty()
    });
    let (exit_code, signal) = match status {
        Ok(status) => exit_facts(status),
        Err(_) => (None, None),
    };
    let pid = {
        let mut state = lock(&inner.state);
        let pid = state.pid;
        state.stdout = stdout;
        state.stderr = stderr;
        state.done = Some(ProcessDone {
            exit_code,
            signal,
            termination: state.termination,
        });
        pid
    };
    inner.done.notify_waiters();
    if let Some(runtime) = runtime.upgrade() {
        lock(&runtime.children).remove(&pid);
    }
}

async fn collect_stdout(
    reader: Option<ChildStdout>,
    policy: Option<CaptureOutput>,
) -> CapturedOutput {
    match (reader, policy) {
        (Some(reader), Some(policy)) => collect_output(reader, policy).await,
        _ => CapturedOutput::empty(),
    }
}

async fn collect_stderr(
    reader: Option<ChildStderr>,
    policy: Option<CaptureOutput>,
) -> CapturedOutput {
    match (reader, policy) {
        (Some(reader), Some(policy)) => collect_output(reader, policy).await,
        _ => CapturedOutput::empty(),
    }
}

async fn collect_output<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    policy: CaptureOutput,
) -> CapturedOutput {
    let mut spill = match &policy.spill_path {
        Some(path) => match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
        {
            Ok(file) => Some(file),
            Err(error) => {
                return CapturedOutput {
                    tail: Vec::new(),
                    total_bytes: 0,
                    spill_path: None,
                    spill_error: Some(format!("cannot create spill file: {error}")),
                }
            }
        },
        None => None,
    };
    let mut buffer = [0u8; 8192];
    let mut output = CapturedOutput {
        tail: Vec::with_capacity(policy.tail_bytes.min(8192)),
        total_bytes: 0,
        spill_path: policy.spill_path.clone(),
        spill_error: None,
    };
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => {
                output.total_bytes += count as u64;
                push_tail(&mut output.tail, &buffer[..count], policy.tail_bytes);
                if let Some(file) = spill.as_mut() {
                    if let Err(error) = file.write_all(&buffer[..count]).await {
                        output.spill_error = Some(format!("cannot write spill file: {error}"));
                        spill = None;
                        output.spill_path = None;
                    }
                }
            }
            Err(error) => {
                output.spill_error = Some(format!("cannot read child output: {error}"));
                output.spill_path = None;
                break;
            }
        }
    }
    if let Some(file) = spill.as_mut() {
        if let Err(error) = file.sync_data().await {
            output.spill_error = Some(format!("cannot sync spill file: {error}"));
            output.spill_path = None;
        }
    }
    output
}

fn push_tail(tail: &mut Vec<u8>, bytes: &[u8], capacity: usize) {
    if capacity == 0 {
        tail.clear();
        return;
    }
    if bytes.len() >= capacity {
        tail.clear();
        tail.extend_from_slice(&bytes[bytes.len() - capacity..]);
        return;
    }
    let excess = tail
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(capacity);
    if excess != 0 {
        tail.drain(..excess);
    }
    tail.extend_from_slice(bytes);
}

async fn read_spill(path: &Path, offset: u64, max: usize) -> Result<Vec<u8>, TessivumError> {
    let mut file = File::open(path).await.map_err(|error| {
        process_error(
            "SUBPROCESS_SPILL_UNAVAILABLE",
            "captured output spill file is unavailable",
            json!({"path": path.display().to_string(), "error": error.to_string()}),
        )
    })?;
    file.seek(SeekFrom::Start(offset)).await.map_err(|error| {
        process_error(
            "SUBPROCESS_SPILL_UNAVAILABLE",
            "captured output spill file cannot be sought",
            json!({"path": path.display().to_string(), "error": error.to_string()}),
        )
    })?;
    let mut bytes = vec![0; max];
    let count = file.read(&mut bytes).await.map_err(|error| {
        process_error(
            "SUBPROCESS_SPILL_UNAVAILABLE",
            "captured output spill file cannot be read",
            json!({"path": path.display().to_string(), "error": error.to_string()}),
        )
    })?;
    bytes.truncate(count);
    Ok(bytes)
}

fn validate_request(request: &SubprocessRequest) -> Result<(), TessivumError> {
    let Some(program) = request.argv.first() else {
        return Err(process_error(
            "SUBPROCESS_INVALID_ARGV",
            "subprocess argv must contain a program",
            json!({}),
        ));
    };
    if program.is_empty()
        || program.contains('\0')
        || request.argv.iter().any(|arg| arg.contains('\0'))
    {
        return Err(process_error(
            "SUBPROCESS_INVALID_ARGV",
            "subprocess argv entries must be non-empty program text without NUL",
            json!({}),
        ));
    }
    for output in [&request.stdout, &request.stderr] {
        if let ProcessOutput::Capture(policy) = output {
            if policy.tail_bytes > MAX_TAIL_BYTES {
                return Err(process_error(
                    "SUBPROCESS_OUTPUT_TOO_LARGE",
                    "captured output tail exceeds the limit",
                    json!({"limit": MAX_TAIL_BYTES}),
                ));
            }
            if let Some(path) = &policy.spill_path {
                if path.as_os_str().is_empty() || path.parent().is_none() {
                    return Err(process_error(
                        "SUBPROCESS_INVALID_SPILL",
                        "output spill path must name a file in an existing parent directory",
                        json!({"path": path.display().to_string()}),
                    ));
                }
            }
        }
    }
    for (key, value) in &request.env {
        if key.is_empty()
            || key.contains('=')
            || key.contains('\0')
            || value.as_ref().is_some_and(|v| v.contains('\0'))
        {
            return Err(process_error(
                "SUBPROCESS_INVALID_ENV",
                "subprocess environment contains an invalid key or value",
                json!({"key": key}),
            ));
        }
    }
    Ok(())
}

fn resolve_program(program: &str) -> Result<PathBuf, TessivumError> {
    if program.trim().is_empty() {
        return Err(process_error(
            "SUBPROCESS_INVALID_COMMAND",
            "subprocess command must not be empty",
            json!({}),
        ));
    }
    let path = Path::new(program);
    if path.is_absolute() {
        if executable(path) {
            return Ok(path.to_path_buf());
        }
        return Err(process_error(
            "SUBPROCESS_EXECUTABLE_NOT_FOUND",
            &format!("subprocess-local: command \"{program}\" is not an executable file"),
            json!({"command": program}),
        ));
    }
    if program.contains('/') || program.contains('\\') {
        return Err(process_error(
            "SUBPROCESS_INVALID_COMMAND",
            &format!("subprocess-local: command \"{program}\" is a relative path; use an absolute path or a bare PATH name"),
            json!({"command": program}),
        ));
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return Err(path_not_found(program));
    };
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(program);
        if executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(path_not_found(program))
}

fn path_not_found(program: &str) -> TessivumError {
    process_error(
        "SUBPROCESS_EXECUTABLE_NOT_FOUND",
        &format!("subprocess-local: command \"{program}\" was not found on PATH"),
        json!({"command": program}),
    )
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn canonical_cwd(cwd: &Path) -> Result<PathBuf, TessivumError> {
    if !cwd.is_absolute() {
        return Err(process_error(
            "SUBPROCESS_INVALID_CWD",
            "subprocess cwd must be an absolute directory",
            json!({"cwd": cwd.display().to_string()}),
        ));
    }
    let path = std::fs::canonicalize(cwd).map_err(|error| {
        process_error(
            "SUBPROCESS_INVALID_CWD",
            "subprocess cwd cannot be resolved",
            json!({"cwd": cwd.display().to_string(), "error": error.to_string()}),
        )
    })?;
    if path.is_dir() {
        Ok(path)
    } else {
        Err(process_error(
            "SUBPROCESS_INVALID_CWD",
            "subprocess cwd is not a directory",
            json!({"cwd": cwd.display().to_string()}),
        ))
    }
}

fn configure_environment(
    command: &mut Command,
    explicit: &BTreeMap<String, Option<String>>,
) -> Result<(), TessivumError> {
    command.env_clear();
    for (key, value) in std::env::vars_os() {
        if !ambient_secret(&key) {
            command.env(key, value);
        }
    }
    for (key, value) in explicit {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    Ok(())
}

fn ambient_secret(key: &OsString) -> bool {
    let key = key.to_string_lossy().to_ascii_uppercase();
    key.starts_with("DSH_")
        || key.contains("_TOKEN")
        || key.ends_with("TOKEN")
        || key.contains("_SECRET")
        || key.ends_with("SECRET")
        || key.contains("PASSWORD")
        || key.contains("API_KEY")
        || key.contains("CREDENTIAL")
        || key == "SSH_AUTH_SOCK"
}

fn configure_stdio(
    command: &mut Command,
    request: &SubprocessRequest,
) -> Result<(), TessivumError> {
    command.stdin(match request.stdin {
        ProcessStdin::Inherit => Stdio::inherit(),
        ProcessStdin::Null => Stdio::null(),
        ProcessStdin::Bytes(_) => Stdio::piped(),
    });
    command.stdout(match request.stdout {
        ProcessOutput::Inherit => Stdio::inherit(),
        ProcessOutput::Null => Stdio::null(),
        ProcessOutput::Capture(_) => Stdio::piped(),
    });
    command.stderr(match request.stderr {
        ProcessOutput::Inherit => Stdio::inherit(),
        ProcessOutput::Null => Stdio::null(),
        ProcessOutput::Capture(_) => Stdio::piped(),
    });
    Ok(())
}

fn capture_policy(output: &ProcessOutput) -> Option<CaptureOutput> {
    match output {
        ProcessOutput::Capture(policy) => Some(policy.clone()),
        _ => None,
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    // SAFETY: the child has not executed user code; `setpgid` is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_: &mut Command) {}

#[cfg(unix)]
fn signal_tree(pid: u32, signal: i32) -> std::io::Result<()> {
    // A negative pid addresses the detached process group. If the leader has
    // already exited, fall back to the direct pid for the narrow race.
    let group_result = unsafe { libc::kill(-(pid as i32), signal) };
    if group_result == 0 {
        return Ok(());
    }
    let group_error = std::io::Error::last_os_error();
    if group_error.raw_os_error() == Some(libc::ESRCH) {
        let direct_result = unsafe { libc::kill(pid as i32, signal) };
        if direct_result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error());
    }
    Err(group_error)
}

#[cfg(unix)]
fn force_terminate_tree(pid: u32) {
    let _ = signal_tree(pid, libc::SIGKILL);
}

#[cfg(not(unix))]
fn force_terminate_tree(_: u32) {}

fn exit_facts(status: std::process::ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        (status.code(), status.signal())
    }
    #[cfg(not(unix))]
    {
        (status.code(), None)
    }
}

fn process_error(code: &str, message: &str, details: serde_json::Value) -> TessivumError {
    TessivumError::new(code, message, "subprocess-local", details)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Explicit platform-shell adapter. It is intentionally separate from the
/// literal argv runtime so only callers choosing this type receive shell syntax.
#[derive(Clone, Debug)]
pub struct ShellAdapter {
    runtime: SubprocessRuntime,
}

impl ShellAdapter {
    pub fn new(runtime: SubprocessRuntime) -> Self {
        Self { runtime }
    }

    pub async fn start(
        &self,
        script: String,
        mut request: SubprocessRequest,
    ) -> Result<ShellProcess, TessivumError> {
        if script.contains('\0') {
            return Err(process_error(
                "SUBPROCESS_INVALID_SHELL",
                "shell script must not contain NUL",
                json!({}),
            ));
        }
        #[cfg(unix)]
        {
            request.argv = vec!["/bin/sh".into(), "-lc".into(), script];
        }
        #[cfg(windows)]
        {
            request.argv = vec![
                "cmd.exe".into(),
                "/D".into(),
                "/S".into(),
                "/C".into(),
                script,
            ];
        }
        #[cfg(not(any(unix, windows)))]
        {
            return Err(process_error(
                "SUBPROCESS_SHELL_UNAVAILABLE",
                "platform shell adapter is unavailable",
                json!({}),
            ));
        }
        let grace = request.terminate_grace;
        Ok(ShellProcess {
            process: self.runtime.spawn(request).await?,
            grace,
        })
    }
}

/// Background shell work with explicit read, kill, and done operations.
#[derive(Clone, Debug)]
pub struct ShellProcess {
    process: Subprocess,
    grace: Duration,
}

impl ShellProcess {
    pub fn done(&self) -> Option<ProcessDone> {
        self.process.done()
    }

    pub async fn read_stdout(
        &self,
        offset: u64,
        max_bytes: usize,
    ) -> Result<ProcessOutputRead, TessivumError> {
        self.process.read_stdout(offset, max_bytes).await
    }

    pub async fn read_stderr(
        &self,
        offset: u64,
        max_bytes: usize,
    ) -> Result<ProcessOutputRead, TessivumError> {
        self.process.read_stderr(offset, max_bytes).await
    }

    pub async fn kill(&self) -> ProcessDone {
        self.process.abort(self.grace).await
    }

    pub async fn wait(&self) -> Result<ProcessDone, TessivumError> {
        let done = self.process.wait().await;
        shell_result(done)
    }

    pub async fn wait_timeout(&self, timeout: Duration) -> Result<ProcessDone, TessivumError> {
        let done = self.process.wait_timeout(timeout).await;
        shell_result(done)
    }
}

fn shell_result(done: ProcessDone) -> Result<ProcessDone, TessivumError> {
    if let Some(termination) = done.termination {
        return Err(process_error(
            match termination {
                ProcessTermination::TimedOut => "SHELL_TIMEOUT",
                ProcessTermination::Aborted => "SHELL_ABORTED",
                ProcessTermination::Shutdown | ProcessTermination::Terminated => "SHELL_TERMINATED",
            },
            "shell process did not complete normally",
            json!({"exitCode": done.exit_code, "signal": done.signal, "cause": termination}),
        ));
    }
    if done.exit_code.unwrap_or_default() != 0 || done.signal.is_some() {
        return Err(process_error(
            "SHELL_NONZERO_EXIT",
            "shell process exited unsuccessfully",
            json!({"exitCode": done.exit_code, "signal": done.signal}),
        ));
    }
    Ok(done)
}

#[cfg(all(test, windows))]
mod windows_process_tree_tests {
    use std::{
        collections::{BTreeMap, BTreeSet, HashMap},
        time::Duration,
    };

    use super::{
        normalize_terminate_failure, traceable_processes, validate_generation_edge,
        wait_millis_for_remaining_duration, WindowsWaitResult,
    };

    fn parents(entries: &[(u32, u32)]) -> HashMap<u32, u32> {
        entries.iter().copied().collect()
    }

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    struct SyntheticGeneration {
        pid: u32,
        creation_time: u64,
        exit_time: Option<u64>,
    }

    fn generation_aware_results(
        snapshot: &HashMap<u32, u32>,
        current: &BTreeMap<u32, SyntheticGeneration>,
        root: SyntheticGeneration,
        retained: &BTreeSet<SyntheticGeneration>,
    ) -> (BTreeSet<SyntheticGeneration>, BTreeSet<SyntheticGeneration>) {
        let retained_generations = retained.clone();
        let mut anchors = retained.clone();
        anchors.insert(root);
        let anchor_pids = anchors
            .iter()
            .map(|generation| generation.pid)
            .collect::<BTreeSet<_>>();
        let mut candidates = traceable_processes(snapshot, root.pid, &anchor_pids);
        let mut termination_targets = BTreeSet::new();
        loop {
            let ready = candidates
                .iter()
                .copied()
                .filter(|pid| {
                    snapshot.get(pid).is_some_and(|parent_pid| {
                        anchors
                            .iter()
                            .any(|generation| generation.pid == *parent_pid)
                    })
                })
                .collect::<Vec<_>>();
            if ready.is_empty() {
                break;
            }
            for pid in ready {
                candidates.remove(&pid);
                let generation = current[&pid];
                let parent_pid = snapshot[&pid];
                let valid_parents = anchors
                    .iter()
                    .filter(|parent| parent.pid == parent_pid)
                    .filter(|parent| {
                        validate_generation_edge(
                            parent.creation_time,
                            parent.exit_time,
                            generation.creation_time,
                        )
                        .is_ok()
                    })
                    .count();
                if valid_parents == 1 {
                    anchors.insert(generation);
                    termination_targets.insert(generation);
                }
            }
        }
        (retained_generations, termination_targets)
    }

    #[test]
    fn rejects_reused_retained_anchor() {
        let historical_anchor = SyntheticGeneration {
            pid: 20,
            creation_time: 100,
            exit_time: Some(200),
        };
        let replacement = SyntheticGeneration {
            pid: 20,
            creation_time: 300,
            exit_time: None,
        };
        let replacement_child = SyntheticGeneration {
            pid: 30,
            creation_time: 400,
            exit_time: None,
        };
        let snapshot = parents(&[(20, 99), (30, 20)]);
        let current = BTreeMap::from([(20, replacement), (30, replacement_child)]);
        let root = SyntheticGeneration {
            pid: 10,
            creation_time: 50,
            exit_time: None,
        };
        let retained = BTreeSet::from([historical_anchor]);

        let (retained_generations, termination_targets) =
            generation_aware_results(&snapshot, &current, root, &retained);

        assert!(!retained_generations.contains(&replacement));
        assert!(!retained_generations.contains(&replacement_child));
        assert!(!termination_targets.contains(&replacement));
        assert!(!termination_targets.contains(&replacement_child));
    }

    #[test]
    fn rejects_reused_root() {
        let historical_root = SyntheticGeneration {
            pid: 10,
            creation_time: 100,
            exit_time: Some(200),
        };
        let replacement = SyntheticGeneration {
            pid: 10,
            creation_time: 300,
            exit_time: None,
        };
        let replacement_child = SyntheticGeneration {
            pid: 20,
            creation_time: 400,
            exit_time: None,
        };
        let snapshot = parents(&[(10, 99), (20, 10)]);
        let current = BTreeMap::from([(10, replacement), (20, replacement_child)]);

        let (retained_generations, termination_targets) =
            generation_aware_results(&snapshot, &current, historical_root, &BTreeSet::new());

        assert!(!retained_generations.contains(&replacement));
        assert!(!retained_generations.contains(&replacement_child));
        assert!(!termination_targets.contains(&replacement));
        assert!(!termination_targets.contains(&replacement_child));
    }

    #[test]
    fn normalizes_terminate_failure() {
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

        let access_denied = || std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32);
        let other_error_code = 87;

        assert!(
            normalize_terminate_failure(access_denied(), Ok(WindowsWaitResult::Signaled)).is_none()
        );
        assert_eq!(
            normalize_terminate_failure(access_denied(), Ok(WindowsWaitResult::TimedOut))
                .and_then(|error| error.raw_os_error()),
            Some(ERROR_ACCESS_DENIED as i32)
        );
        assert_eq!(
            normalize_terminate_failure(access_denied(), Err(std::io::Error::from_raw_os_error(6)))
                .and_then(|error| error.raw_os_error()),
            Some(ERROR_ACCESS_DENIED as i32)
        );
        assert_eq!(
            normalize_terminate_failure(
                std::io::Error::from_raw_os_error(other_error_code),
                Ok(WindowsWaitResult::Signaled)
            )
            .and_then(|error| error.raw_os_error()),
            Some(other_error_code)
        );
    }

    #[test]
    fn windows_process_tree_includes_direct_and_transitive_descendants() {
        let snapshot = parents(&[(10, 1), (20, 10), (30, 20)]);

        assert_eq!(
            traceable_processes(&snapshot, 10, &BTreeSet::new()),
            BTreeSet::from([20, 30])
        );
    }

    #[test]
    fn windows_process_tree_uses_every_retained_identity_as_an_anchor() {
        let snapshot = parents(&[(10, 1), (20, 99), (30, 20), (40, 30)]);
        let retained = BTreeSet::from([20, 30]);

        assert_eq!(
            traceable_processes(&snapshot, 10, &retained),
            BTreeSet::from([40])
        );
    }

    #[test]
    fn windows_process_tree_excludes_unrelated_pids_and_breaks_cycles() {
        let snapshot = parents(&[(10, 1), (20, 10), (30, 40), (40, 30), (50, 99)]);

        assert_eq!(
            traceable_processes(&snapshot, 10, &BTreeSet::new()),
            BTreeSet::from([20])
        );
    }

    #[test]
    fn windows_process_tree_rejects_stale_ancestry_after_a_fresh_snapshot() {
        let observed = parents(&[(10, 1), (20, 10)]);
        let revalidated = parents(&[(10, 1), (20, 99)]);

        assert_eq!(
            traceable_processes(&observed, 10, &BTreeSet::new()),
            BTreeSet::from([20])
        );
        assert!(traceable_processes(&revalidated, 10, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn windows_process_tree_never_rounds_a_wait_past_the_shared_deadline() {
        assert_eq!(
            wait_millis_for_remaining_duration(Duration::from_micros(999)),
            None
        );
        assert_eq!(
            wait_millis_for_remaining_duration(Duration::from_millis(1)),
            Some(1)
        );
    }
}

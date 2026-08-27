//! Shell-free subprocess ownership with bounded, replayable output.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
#[cfg(unix)]
use std::os::fd::RawFd;


use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{Mutex as AsyncMutex, Notify},
    time,
};

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
    done: Notify,
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
        let _ = signal_tree(pid, libc::SIGTERM);
        let notified = self.inner.done.notified();
        if self.done().is_none() {
            tokio::select! {
                _ = notified => return,
                _ = time::sleep(grace) => {}
            }
        }
        if self.done().is_none() {
            let _ = signal_tree(pid, libc::SIGKILL);
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
        let mut child = command.spawn().map_err(|error| {
            process_error(
                "SUBPROCESS_SPAWN_FAILED",
                "subprocess could not be spawned",
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
#[cfg(unix)]
pub type PersistentShellLeaseValidator =
    Arc<dyn Fn() -> Result<(), TessivumError> + Send + Sync + 'static>;

/// Immutable spawn plan for one persistent Unix shell.
#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistentShellConfig {
    /// Fixed shell or sandbox-wrapper argv. The default is `/bin/sh -s`.
    pub argv: Vec<String>,
    /// Fallback workspace path used when no validated directory descriptor is supplied.
    pub workspace: PathBuf,
    /// A caller-validated directory descriptor used for the initial child cwd.
    pub cwd_fd: Option<RawFd>,
    /// Explicit environment additions/removals after the normal ambient-secret scrub.
    pub env: BTreeMap<String, Option<String>>,
    /// Per-stream in-memory tail limit for one command.
    pub max_output_bytes: usize,
    pub terminate_grace: Duration,
}

#[cfg(unix)]
impl PersistentShellConfig {
pub fn new(workspace: impl Into<PathBuf>) -> Self {
    Self {
        argv: vec!["/bin/sh".into(), "-s".into()],
        workspace: workspace.into(),
        cwd_fd: None,
        env: BTreeMap::new(),
        max_output_bytes: DEFAULT_TAIL_BYTES,
        terminate_grace: Duration::from_millis(500),
    }
}
}

/// One request evaluated by a [`PersistentShell`].
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct PersistentShellCommand {
    pub script: String,
    pub timeout: Duration,
    pub cancellation: Option<CancellationToken>,
}

#[cfg(unix)]
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
#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistentShellResult {
    pub exit_code: i32,
    pub stdout: ProcessOutputSnapshot,
    pub stderr: ProcessOutputSnapshot,
}

#[cfg(unix)]
struct PersistentShellCommandState {
    nonce: String,
    state: Mutex<PersistentShellCommandCapture>,
    done: Notify,
}

#[cfg(unix)]
struct PersistentShellCommandCapture {
    stdout: CapturedOutput,
    stderr: CapturedOutput,
    stdout_status: Option<i32>,
    stderr_status: Option<i32>,
    error: Option<TessivumError>,
}

#[cfg(unix)]
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

/// Session-owned reusable `/bin/sh` with a fixed canonical workspace.
///
/// Commands are serialized. Output frames are random per command and removed
/// from the captured stream; an EOF, replaced shell, or closed stdin fails the
/// current command and permanently retires the instance rather than waiting for
/// a marker that cannot arrive.
#[cfg(unix)]
#[derive(Clone)]
pub struct PersistentShell {
    inner: Arc<PersistentShellInner>,
}

#[cfg(unix)]
struct PersistentShellInner {
    pid: u32,
    validator: PersistentShellLeaseValidator,
    max_output_bytes: usize,
    terminate_grace: Duration,
    stdin: AsyncMutex<Option<ChildStdin>>,
    serial: AsyncMutex<()>,
    active: Mutex<Option<Arc<PersistentShellCommandState>>>,
    termination: Mutex<Option<ProcessTermination>>,
    done_state: Mutex<Option<ProcessDone>>,
    done: Notify,
    closed: AtomicBool,
    disposed: AtomicBool,
    dispose_signal: Notify,
    dispose_gate: AsyncMutex<()>,
}

#[cfg(unix)]
impl fmt::Debug for PersistentShell {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersistentShell")
            .field("pid", &self.inner.pid)
            .field("closed", &self.inner.closed.load(Ordering::Acquire))
            .finish()
    }
}

#[cfg(unix)]
impl PersistentShell {
    /// Starts exactly one `/bin/sh -s` using the supplied canonical workspace
    /// plan. The lease validator is required and runs before the initial spawn
    /// and every later command.
    pub async fn start(
        config: PersistentShellConfig,
        validate_lease: impl Fn() -> Result<(), TessivumError> + Send + Sync + 'static,
    ) -> Result<Self, TessivumError> {
        validate_persistent_shell_config(&config)?;
        let validator: PersistentShellLeaseValidator = Arc::new(validate_lease);
        validator()?;
let cwd = config
    .cwd_fd
    .is_none()
    .then(|| canonical_cwd(&config.workspace))
    .transpose()?;
let mut command = Command::new(&config.argv[0]);
command.args(&config.argv[1..]);
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
configure_environment(&mut command, &config.env)?;
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        configure_process_group(&mut command);
        let mut child = command.spawn().map_err(|error| {
            persistent_shell_error(
                "PERSISTENT_SHELL_UNAVAILABLE",
                "persistent shell could not be spawned",
                json!({"program": "/bin/sh", "error": error.to_string()}),
            )
        })?;
        let Some(pid) = child.id() else {
            let _ = child.kill().await;
            return Err(persistent_shell_error(
                "PERSISTENT_SHELL_UNAVAILABLE",
                "persistent shell did not report a process identifier",
                json!({"program": "/bin/sh"}),
            ));
        };
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = signal_tree(pid, libc::SIGKILL);
            let _ = child.wait().await;
            return Err(persistent_shell_error(
                "PERSISTENT_SHELL_UNAVAILABLE",
                "persistent shell did not provide required standard streams",
                json!({"pid": pid}),
            ));
        };
        let inner = Arc::new(PersistentShellInner {
            pid,
            validator,
            max_output_bytes: config.max_output_bytes,
            terminate_grace: config.terminate_grace,
            stdin: AsyncMutex::new(Some(stdin)),
            serial: AsyncMutex::new(()),
            active: Mutex::new(None),
            termination: Mutex::new(None),
            done_state: Mutex::new(None),
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

#[cfg(unix)]
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

    async fn stop(self: &Arc<Self>, cause: ProcessTermination) {
        if self.done().is_some() {
            return;
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
        if !first {
            self.wait_closed().await;
            return;
        }
        self.closed.store(true, Ordering::Release);
        self.fail_active(persistent_shell_termination(cause));
        let _ = signal_tree(self.pid, libc::SIGTERM);
        let notified = self.done.notified();
        if self.done().is_none() {
            tokio::select! {
                _ = notified => return,
                _ = time::sleep(self.terminate_grace) => {}
            }
        }
        if self.done().is_none() {
            let _ = signal_tree(self.pid, libc::SIGKILL);
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
    let incomplete = inner.fail_active(persistent_shell_closed());
    if incomplete && inner.terminated().is_none() {
        let _ = signal_tree(inner.pid, libc::SIGTERM);
        time::sleep(inner.terminate_grace).await;
        let _ = signal_tree(inner.pid, libc::SIGKILL);
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
const PERSISTENT_SHELL_FRAME_PREFIX: &[u8] = b"\x1eTESSIVUM-SHELL:";
#[cfg(unix)]
const PERSISTENT_SHELL_FRAME_SUFFIX: &[u8] = b"\x1f\n";
#[cfg(unix)]
const MAX_PERSISTENT_SHELL_FRAME_BYTES: usize = 128;

#[cfg(unix)]
enum PersistentShellStreamToken {
    Data(Vec<u8>),
    Marker {
        nonce: String,
        stdout: bool,
        status: i32,
    },
}

#[cfg(unix)]
#[derive(Default)]
struct PersistentShellFrameParser {
    pending: Vec<u8>,
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(unix)]
fn prefix_suffix_len(bytes: &[u8], prefix: &[u8]) -> usize {
    (1..prefix.len())
        .rev()
        .find(|&length| bytes.ends_with(&prefix[..length]))
        .unwrap_or(0)
}

#[cfg(unix)]
async fn optional_cancellation(cancellation: Option<CancellationToken>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(unix)]
fn validate_persistent_shell_config(config: &PersistentShellConfig) -> Result<(), TessivumError> {
    let Some(program) = config.argv.first() else {
        return Err(persistent_shell_error(
            "PERSISTENT_SHELL_INVALID_ARGV",
            "persistent shell argv must contain a program",
            json!({}),
        ));
    };
    if program.is_empty() || program.contains('\0') || config.argv.iter().any(|arg| arg.contains('\0')) {
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

#[cfg(unix)]
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

#[cfg(unix)]
fn persistent_shell_error(code: &str, message: &str, details: serde_json::Value) -> TessivumError {
    TessivumError::new(code, message, "persistent-shell", details)
}

#[cfg(unix)]
fn persistent_shell_closed() -> TessivumError {
    persistent_shell_error(
        "PERSISTENT_SHELL_CLOSED",
        "persistent shell closed before command completion",
        json!({}),
    )
}

#[cfg(unix)]
fn persistent_shell_disposed() -> TessivumError {
    persistent_shell_error(
        "PERSISTENT_SHELL_DISPOSED",
        "persistent shell has been disposed",
        json!({}),
    )
}

#[cfg(unix)]
fn persistent_shell_cancelled() -> TessivumError {
    persistent_shell_error(
        "PERSISTENT_SHELL_CANCELLED",
        "persistent shell command was cancelled",
        json!({}),
    )
}

#[cfg(unix)]
fn persistent_shell_timed_out(timeout: Duration) -> TessivumError {
    persistent_shell_error(
        "PERSISTENT_SHELL_TIMEOUT",
        "persistent shell command timed out",
        json!({"timeoutMs": timeout.as_millis()}),
    )
}

#[cfg(unix)]
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

#[cfg(not(unix))]
fn signal_tree(_: u32, _: i32) -> std::io::Result<()> {
    Ok(())
}

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

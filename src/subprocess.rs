//! Shell-free subprocess ownership with bounded, replayable output.

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
use tessivum_core::{ContextHandle, CoreError, ServiceHandle, ServiceKey};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    process::{Child, ChildStderr, ChildStdout, Command},
    sync::Notify,
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

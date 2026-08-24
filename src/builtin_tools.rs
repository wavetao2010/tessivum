//! Explicitly enabled process-local tools for a headless composition.
//!
//! `bash` is a native, trusted capability, not a sandbox. Enabling it grants the
//! configured working directory's host permissions; callers must still apply
//! their normal approval and sandbox policy before allowing a model to invoke it.
mod model_tools;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{
    attachments::AttachmentStore,
    jobs::{JobOwner, JobStart},
    sandbox::{Sandbox, SandboxApproval, SandboxMode, SandboxReadPolicy, SandboxRequest},
    session::SessionStore,
    tools::{
        ToolApproval, ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    web::WebRuntime,
    workspace::{SessionResourceResolver, WorkspaceError, WorkspaceLease},
    ContentBlock, SessionId, TessivumError, ToolSchema,
};

/// Default upper bound for combined `bash` stdout and stderr.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Hard ceiling for combined `bash` stdout and stderr.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

pub(crate) type BashJobOwners = Arc<Mutex<BTreeMap<SessionId, JobOwner>>>;

pub(crate) struct HostToolServices {
    sessions: SessionStore,
    sandbox: Sandbox,
    approval: Arc<dyn ToolApproval>,
    job_owners: BashJobOwners,
    attachments: Arc<AttachmentStore>,
    web: WebRuntime,
}

impl HostToolServices {
    pub(crate) fn new(
        sessions: SessionStore,
        sandbox: Sandbox,
        approval: Arc<dyn ToolApproval>,
        job_owners: BashJobOwners,
        attachments: Arc<AttachmentStore>,
        web: WebRuntime,
    ) -> Self {
        Self {
            sessions,
            sandbox,
            approval,
            job_owners,
            attachments,
            web,
        }
    }
}

/// Configuration for [`BuiltinTools`].
#[derive(Clone, Debug)]
pub struct BuiltinToolsConfig {
    /// Enables the trusted native `bash` capability. It is disabled by default.
    pub enable_bash: bool,
    /// Fixed working directory for headless composition; ignored with a resolver.
    pub cwd: PathBuf,
    /// Resolves each trusted `bash` call from its durable session membership.
    pub resolver: Option<Arc<SessionResourceResolver>>,
    /// Maximum combined stdout and stderr bytes retained for one `bash` call.
    pub max_output_bytes: usize,
}

impl Default for BuiltinToolsConfig {
    fn default() -> Self {
        Self {
            enable_bash: false,
            cwd: PathBuf::from("."),
            resolver: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }
}

/// Owns registrations for the standard headless tools.
///
/// Dropping this value unregisters every tool it installed.
#[derive(Debug)]
pub struct BuiltinTools {
    _config: BuiltinToolsConfig,
    _registrations: Vec<ToolRegistration>,
}

impl BuiltinTools {
    /// Registers the minimal standalone tools. Host composition additionally
    /// installs workspace-confined filesystem and web tools.
    pub fn new(runtime: &ToolRuntime, config: BuiltinToolsConfig) -> Result<Self, TessivumError> {
        Self::build(runtime, config, None)
    }

    pub(crate) fn for_host(
        runtime: &ToolRuntime,
        config: BuiltinToolsConfig,
        services: HostToolServices,
    ) -> Result<Self, TessivumError> {
        Self::build(runtime, config, Some(services))
    }

    fn build(
        runtime: &ToolRuntime,
        config: BuiltinToolsConfig,
        services: Option<HostToolServices>,
    ) -> Result<Self, TessivumError> {
        let config = canonical_config(config)?;
        if config.enable_bash && !cfg!(unix) {
            return Err(config_error(
                "UNSUPPORTED_BUILTIN_BASH",
                "builtin bash is supported only on Unix",
                Value::Null,
            ));
        }
        let mut registrations = vec![runtime.register(ToolDefinition::new(
            "echo",
            "Returns the supplied text unchanged.",
            echo_schema(),
            Echo,
        ))?];
        if let Some(services) = services.as_ref() {
            registrations.extend(model_tools::register(runtime, &config, services)?);
        } else {
            registrations.push(runtime.register(ToolDefinition::new(
                "read",
                "Reads one UTF-8 text file inside the current workspace.",
                read_schema(),
                ReadFile {
                    cwd: config.cwd.clone(),
                    resolver: config.resolver.clone(),
                    max_output_bytes: config.max_output_bytes,
                },
            ))?);
        }
        if config.enable_bash {
            let (sessions, sandbox, approval, job_owners) = services
                .map(|services| {
                    (
                        Some(services.sessions),
                        Some(services.sandbox),
                        Some(services.approval),
                        Some(services.job_owners),
                    )
                })
                .unwrap_or_default();
            #[cfg(unix)]
            registrations.push(runtime.register(ToolDefinition::new(
                "bash",
                "Runs a shell command under the session's native sandbox policy.",
                bash_schema(),
                Bash {
                    cwd: config.cwd.clone(),
                    resolver: config.resolver.clone(),
                    sessions,
                    sandbox,
                    approval,
                    max_output_bytes: config.max_output_bytes,
                    job_owners,
                },
            ))?);
        }
        Ok(Self {
            _config: config,
            _registrations: registrations,
        })
    }

    /// Alias for [`Self::new`] when composing registrations declaratively.
    pub fn register(
        runtime: &ToolRuntime,
        config: BuiltinToolsConfig,
    ) -> Result<Self, TessivumError> {
        Self::new(runtime, config)
    }
}

fn canonical_config(mut config: BuiltinToolsConfig) -> Result<BuiltinToolsConfig, TessivumError> {
    if !(1..=MAX_OUTPUT_BYTES).contains(&config.max_output_bytes) {
        return Err(config_error(
            "INVALID_BUILTIN_TOOLS_CONFIG",
            "max_output_bytes must be between 1 and MAX_OUTPUT_BYTES",
            json!({"maxOutputBytes": config.max_output_bytes, "max": MAX_OUTPUT_BYTES}),
        ));
    }
    if config.resolver.is_none() {
        config.cwd = config.cwd.canonicalize().map_err(|error| {
            config_error(
                "INVALID_BUILTIN_TOOLS_CONFIG",
                "cwd must exist and be canonicalizable",
                json!({"cwd": config.cwd, "error": error.to_string()}),
            )
        })?;
        if !config.cwd.is_dir() {
            return Err(config_error(
                "INVALID_BUILTIN_TOOLS_CONFIG",
                "cwd must be a directory",
                json!({"cwd": config.cwd}),
            ));
        }
    }
    Ok(config)
}

fn config_error(code: &str, message: &str, details: Value) -> TessivumError {
    TessivumError::new(code, message, "tools", details)
}

fn invalid_arguments(message: &str, details: Value) -> TessivumError {
    TessivumError::new("INVALID_TOOL_ARGUMENTS", message, "tools", details)
}

fn bounded_job_label(value: &str) -> String {
    let mut end = value.len().min(256);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn echo_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
        "additionalProperties": false
    })
}

fn read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"file_path": {"type": "string"}},
        "required": ["file_path"],
        "additionalProperties": false
    })
}

fn bash_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string"},
            "description": {"type": "string"},
            "sandbox_permissions": {
                "type": "string",
                "enum": ["read-only", "workspace-write", "danger-full-access"]
            },
            "justification": {"type": "string"},
            "run_in_background": {"type": "boolean"},
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

struct Echo;

#[async_trait]
impl ToolHandler for Echo {
    async fn run(&self, _context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let text = arguments
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_arguments("text must be a string", json!({"path": "$.text"})))?;
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            false,
            Value::Null,
        ))
    }
}

struct ReadFile {
    cwd: PathBuf,
    resolver: Option<Arc<SessionResourceResolver>>,
    max_output_bytes: usize,
}

#[async_trait]
impl ToolHandler for ReadFile {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let file_path = arguments
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid_arguments("file_path must be a string", json!({"path": "$.file_path"}))
            })?;
        if file_path.trim().is_empty() {
            return Err(invalid_arguments(
                "file_path must not be blank",
                json!({"path": "$.file_path"}),
            ));
        }
        let lease = self
            .resolver
            .as_ref()
            .map(|resolver| resolver.resolve(&context.session))
            .transpose()
            .map_err(|error| workspace_error(&context, error))?;
        let root = match &lease {
            Some(lease) => lease
                .validate_current()
                .map_err(|error| workspace_error(&context, error))?,
            None => self.cwd.clone(),
        };
        let requested = root.join(file_path);
        let path = tokio::fs::canonicalize(&requested).await.map_err(|error| {
            let message = if error.kind() == std::io::ErrorKind::NotFound {
                format!("cannot read {:?}: not found", requested)
            } else {
                format!("cannot read {:?}: {error}", requested)
            };
            TessivumError::new("READ_FAILED", message, "tools", json!({"path": requested}))
        })?;
        if !path.starts_with(&root) {
            return Err(TessivumError::new(
                "READ_OUTSIDE_WORKSPACE",
                "cannot read outside the current workspace",
                "tools",
                json!({"path": file_path}),
            ));
        }
        let bytes = tokio::fs::read(&path).await.map_err(|error| {
            TessivumError::new(
                "READ_FAILED",
                format!("cannot read {:?}: {error}", path),
                "tools",
                json!({"path": path}),
            )
        })?;
        if bytes.len() > self.max_output_bytes {
            return Err(TessivumError::new(
                "READ_OUTPUT_LIMIT",
                format!(
                    "file exceeds the {} byte output limit",
                    self.max_output_bytes
                ),
                "tools",
                json!({"path": path, "maxOutputBytes": self.max_output_bytes}),
            ));
        }
        let text = String::from_utf8(bytes).map_err(|_| {
            TessivumError::new(
                "READ_NOT_UTF8",
                "file is not valid UTF-8 text",
                "tools",
                json!({"path": path}),
            )
        })?;
        Ok(ToolOutput::new(
            vec![ContentBlock::Text { text }],
            false,
            json!({"path": path}),
        ))
    }
}

#[cfg(unix)]
struct Bash {
    cwd: PathBuf,
    resolver: Option<Arc<SessionResourceResolver>>,
    sessions: Option<SessionStore>,
    sandbox: Option<Sandbox>,
    approval: Option<Arc<dyn ToolApproval>>,
    max_output_bytes: usize,
    job_owners: Option<BashJobOwners>,
}

#[cfg(unix)]
#[async_trait]
impl ToolHandler for Bash {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid_arguments("command must be a string", json!({"path": "$.command"}))
            })?;
        if command.trim().is_empty() {
            return Err(invalid_arguments(
                "command must not be blank",
                json!({"path": "$.command"}),
            ));
        }
        let lease = self
            .resolver
            .as_ref()
            .map(|resolver| resolver.resolve(&context.session))
            .transpose()
            .map_err(|error| workspace_error(&context, error))?;
        let current = session_sandbox_mode(self.sessions.as_ref(), &context.session);
        let requested = arguments
            .get("sandbox_permissions")
            .map(|value| serde_json::from_value::<SandboxMode>(value.clone()))
            .transpose()
            .map_err(|_| {
                invalid_arguments(
                    "sandbox_permissions is invalid",
                    json!({"path": "$.sandbox_permissions"}),
                )
            })?;
        let mode = requested.unwrap_or(current);
        if mode > current {
            let approved = match &self.approval {
                Some(approval) => approval
                    .approve(
                        &context,
                        &ToolSchema {
                            name: "bash".into(),
                            description: "Runs a shell command under the selected sandbox.".into(),
                            parameters: bash_schema(),
                        },
                        &arguments,
                    )
                    .await?
                    .unwrap_or(false),
                None => false,
            };
            if !approved {
                return Err(TessivumError::new(
                    "TOOL_APPROVAL_DENIED",
                    "sandbox escalation was not approved",
                    "tools",
                    json!({"name": "bash"}),
                ));
            }
        }
        let background = match arguments.get("run_in_background") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(invalid_arguments(
                    "run_in_background must be a boolean",
                    json!({"path": "$.run_in_background"}),
                ))
            }
        };
        if background {
            let owners = self.job_owners.as_ref().ok_or_else(|| {
                TessivumError::new(
                    "BACKGROUND_JOBS_UNAVAILABLE",
                    "background jobs are unavailable in this composition",
                    "tools",
                    Value::Null,
                )
            })?;
            let owner = owners
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(&context.session)
                .cloned()
                .ok_or_else(|| {
                    TessivumError::new(
                        "BACKGROUND_JOB_OWNER_NOT_FOUND",
                        "the session has no live background-job owner",
                        "tools",
                        json!({"sessionId": context.session}),
                    )
                })?;
            let cwd = self.cwd.clone();
            let resolver = self.resolver.clone();
            let sandbox = self.sandbox.clone();
            let session = context.session.clone();
            let call = context.call.clone();
            let command = command.to_owned();
            let label = bounded_job_label(&command);
            let max_output_bytes = self.max_output_bytes;
            let job = owner
                .start(
                    JobStart::new("bash", label, max_output_bytes, move |control| {
                        let cwd = cwd.clone();
                        let resolver = resolver.clone();
                        let sandbox = sandbox.clone();
                        let session = session.clone();
                        let call = call.clone();
                        let command = command.clone();
                        async move {
                            let job_context = ToolRunContext {
                                session,
                                call,
                                cancellation: control.cancellation(),
                            };
                            let lease = resolver
                                .as_ref()
                                .map(|resolver| resolver.resolve(&job_context.session))
                                .transpose()
                                .map_err(|error| error.to_string())?;
                            let output = run_bash(
                                &cwd,
                                lease.as_ref(),
                                sandbox.as_ref(),
                                mode,
                                max_output_bytes,
                                &job_context,
                                &command,
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                            for block in &output.content {
                                if let ContentBlock::Text { text } = block {
                                    control.write_text(text);
                                }
                            }
                            if output.is_error {
                                Err("bash exited unsuccessfully".into())
                            } else {
                                Ok(output.meta)
                            }
                        }
                    })
                    .with_cancel_detail("signal: SIGTERM"),
                )
                .map_err(|error| {
                    TessivumError::new(
                        "BACKGROUND_JOB_START_FAILED",
                        error.to_string(),
                        "tools",
                        Value::Null,
                    )
                })?;
            return Ok(ToolOutput::new(
                vec![ContentBlock::Text {
                    text: format!("Started background job {}.", job.id),
                }],
                false,
                json!({"jobId": job.id}),
            ));
        }
        run_bash(
            &self.cwd,
            lease.as_ref(),
            self.sandbox.as_ref(),
            mode,
            self.max_output_bytes,
            &context,
            command,
        )
        .await
    }
}

#[cfg(unix)]
async fn run_bash(
    cwd: &Path,
    lease: Option<&WorkspaceLease>,
    sandbox: Option<&Sandbox>,
    mode: SandboxMode,
    max_output_bytes: usize,
    context: &ToolRunContext,
    command: &str,
) -> ToolHandlerResult {
    use std::{io, os::unix::process::ExitStatusExt};

    use tokio::process::Command;

    let argv = vec!["/bin/sh".into(), "-lc".into(), "--".into(), command.into()];
    let argv = if let (Some(sandbox), Some(lease)) = (sandbox, lease) {
        let workspace = lease
            .validate_current()
            .map_err(|error| workspace_error(context, error))?;
        let write_roots = vec![workspace.clone()];
        sandbox
            .prepare(
                &SandboxRequest {
                    mode,
                    workspace,
                    read_policy: SandboxReadPolicy::Deny,
                    read_roots: Vec::new(),
                    write_roots,
                    approval: (mode == SandboxMode::WorkspaceWrite).then_some(SandboxApproval {
                        mode: Some(SandboxMode::WorkspaceWrite),
                        read_policy: None,
                    }),
                },
                &argv,
            )?
            .argv
    } else {
        argv
    };
    let mut shell = Command::new(&argv[0]);
    shell.args(&argv[1..]);
    if let Some(lease) = lease {
        use std::os::unix::process::CommandExt;
        let directory = lease
            .directory_fd()
            .map_err(|error| workspace_error(context, error))?;
        unsafe {
            shell.as_std_mut().pre_exec(move || {
                if libc::fchdir(directory) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
    } else {
        shell.current_dir(cwd);
    }
    let mut child = shell
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| bash_error("could not start shell", error))?;
    let stdout = child.stdout.take().expect("piped stdout is available");
    let stderr = child.stderr.take().expect("piped stderr is available");
    let output = Arc::new(Mutex::new(CapturedOutput::default()));
    let stdout_task = tokio::spawn(copy_bounded(
        stdout,
        Arc::clone(&output),
        Stream::Stdout,
        max_output_bytes,
    ));
    let stderr_task = tokio::spawn(copy_bounded(
        stderr,
        Arc::clone(&output),
        Stream::Stderr,
        max_output_bytes,
    ));

    let status = tokio::select! {
        status = child.wait() => status.map_err(|error| bash_error("could not wait for shell", error))?,
        _ = context.cancellation.cancelled() => {
            let killed = child.kill().await;
            let waited = child.wait().await;
            finish_copy(stdout_task).await?;
            finish_copy(stderr_task).await?;
            if let Err(error) = waited {
                return Err(bash_error("could not reap cancelled shell", error));
            }
            if let Err(error) = killed {
                // A completed process can race cancellation; successful wait proves it was reaped.
                if !error.kind().eq(&io::ErrorKind::InvalidInput) {
                    return Err(bash_error("could not kill cancelled shell", error));
                }
            }
            return Err(TessivumError::new("CANCELLED", "tool call was cancelled", "tools", Value::Null));
        }
    };
    finish_copy(stdout_task).await?;
    finish_copy(stderr_task).await?;

    let output = std::mem::take(&mut *output.lock().unwrap_or_else(|poison| poison.into_inner()));
    let output_bytes = output.bytes;
    let truncated = output.truncated;
    let exit_code = status.code();
    let signal = status.signal().map(bash_signal_name);
    let text = output.text(exit_code);
    Ok(ToolOutput::new(
        vec![ContentBlock::Text { text }],
        !status.success(),
        json!({
            "exitCode": exit_code,
            "signal": signal,
            "outputBytes": output_bytes,
            "truncated": truncated,
        }),
    ))
}

#[cfg(unix)]
fn bash_signal_name(signal: i32) -> std::borrow::Cow<'static, str> {
    match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGCHLD => "SIGCHLD",
        libc::SIGCONT => "SIGCONT",
        libc::SIGSTOP => "SIGSTOP",
        libc::SIGTSTP => "SIGTSTP",
        libc::SIGTTIN => "SIGTTIN",
        libc::SIGTTOU => "SIGTTOU",
        libc::SIGURG => "SIGURG",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGVTALRM => "SIGVTALRM",
        libc::SIGPROF => "SIGPROF",
        libc::SIGWINCH => "SIGWINCH",
        libc::SIGIO => "SIGIO",
        libc::SIGSYS => "SIGSYS",
        _ => return format!("SIG{signal}").into(),
    }
    .into()
}

#[cfg(unix)]
fn session_sandbox_mode(
    sessions: Option<&SessionStore>,
    session_id: &crate::SessionId,
) -> SandboxMode {
    sessions
        .and_then(|sessions| sessions.get(session_id))
        .and_then(|session| {
            session.events().into_iter().rev().find_map(|event| {
                (event.event_type == "sandbox/mode")
                    .then(|| event.data.get("mode").cloned())
                    .flatten()
                    .and_then(|value| serde_json::from_value(value).ok())
            })
        })
        .unwrap_or(SandboxMode::WorkspaceWrite)
}

#[cfg(unix)]
fn bash_error(message: &str, error: std::io::Error) -> TessivumError {
    TessivumError::new(
        "BASH_EXECUTION_FAILED",
        message,
        "tools",
        json!({"error": error.to_string()}),
    )
}

#[cfg(unix)]
fn workspace_error(context: &ToolRunContext, error: WorkspaceError) -> TessivumError {
    TessivumError::new(
        error.code(),
        error.to_string(),
        "tools",
        json!({"sessionId": context.session}),
    )
}

#[cfg(unix)]
async fn finish_copy(
    task: tokio::task::JoinHandle<std::io::Result<()>>,
) -> Result<(), TessivumError> {
    task.await
        .map_err(|error| {
            TessivumError::new(
                "BASH_EXECUTION_FAILED",
                "shell output task failed",
                "tools",
                json!({"error": error.to_string()}),
            )
        })?
        .map_err(|error| bash_error("could not read shell output", error))
}

#[cfg(unix)]
#[derive(Default)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    bytes: usize,
    truncated: bool,
}

#[cfg(unix)]
impl CapturedOutput {
    fn append(&mut self, stream: Stream, chunk: &[u8], max_output_bytes: usize) {
        let copied = chunk.len().min(max_output_bytes.saturating_sub(self.bytes));
        if copied < chunk.len() {
            self.truncated = true;
        }
        match stream {
            Stream::Stdout => self.stdout.extend_from_slice(&chunk[..copied]),
            Stream::Stderr => self.stderr.extend_from_slice(&chunk[..copied]),
        }
        self.bytes += copied;
    }

    fn text(self, exit_code: Option<i32>) -> String {
        let mut text = String::from_utf8_lossy(&self.stdout).into_owned();
        if !self.stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("[stderr]\n");
            text.push_str(&String::from_utf8_lossy(&self.stderr));
        }
        if text.is_empty() {
            text.push_str("(no output)");
        }
        if let Some(code) = exit_code.filter(|code| *code != 0) {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("[exit code: {code}]"));
        }
        text
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

#[cfg(unix)]
async fn copy_bounded<R>(
    mut reader: R,
    output: std::sync::Arc<std::sync::Mutex<CapturedOutput>>,
    stream: Stream,
    max_output_bytes: usize,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut chunk = [0; 8192];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        output
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .append(stream, &chunk[..read], max_output_bytes);
    }
}

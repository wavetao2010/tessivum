//! Explicitly enabled process-local tools for a headless composition.
//!
//! `bash` is a native, trusted capability, not a sandbox. Enabling it grants the
//! configured working directory's host permissions; callers must still apply
//! their normal approval and sandbox policy before allowing a model to invoke it.
mod model_tools;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use serde_json::{json, Value};
#[cfg(any(unix, windows))]
use std::path::Path;

#[cfg(unix)]
use crate::jobs::JobStart;
#[cfg(unix)]
use crate::subprocess::{
    PersistentShell, PersistentShellCommand, PersistentShellConfig, PersistentShellLeaseValidator,
    PersistentShellResult,
};
use crate::{
    attachments::AttachmentStore,
    jobs::JobOwner,
    sandbox::Sandbox,
    session::SessionStore,
    tools::{
        ToolApproval, ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    web::WebRuntime,
    workspace::{SessionResourceResolver, WorkspaceError},
    ContentBlock, SessionId, TessivumError,
};
#[cfg(any(unix, windows))]
use crate::{
    sandbox::{SandboxApproval, SandboxMode, SandboxReadPolicy, SandboxRequest},
    workspace::WorkspaceLease,
    ToolSchema,
};

/// Default upper bound for combined `bash` stdout and stderr.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Hard ceiling for combined `bash` stdout and stderr.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

pub(crate) type BashJobOwners = Arc<Mutex<BTreeMap<SessionId, JobOwner>>>;
/// Cloneable session ownership for opt-in persistent Bash shells.
#[derive(Clone, Default)]
pub struct PersistentShellSessions {
    inner: Arc<Mutex<BTreeMap<SessionId, Arc<PersistentShellSession>>>>,
}

impl std::fmt::Debug for PersistentShellSessions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistentShellSessions")
            .field("session_count", &lock(&self.inner).len())
            .finish()
    }
}

impl PersistentShellSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables lazy persistent-shell creation for one session.
    pub fn enable(&self, session: SessionId) {
        let mut sessions = lock(&self.inner);
        let entry = sessions
            .entry(session)
            .or_insert_with(|| Arc::new(PersistentShellSession::new()));
        entry.enabled.store(true, Ordering::Release);
    }

    /// Retires one session shell and waits until its process group is reaped.
    pub async fn disable(&self, session: &SessionId) {
        let entry = lock(&self.inner).remove(session);
        if let Some(entry) = entry {
            entry.enabled.store(false, Ordering::Release);
            entry.dispose().await;
        }
    }

    #[cfg(unix)]
    async fn retire(&self, session: &SessionId) {
        let entry = lock(&self.inner).get(session).cloned();
        if let Some(entry) = entry {
            entry.dispose().await;
        }
    }

    /// Retires every session shell and waits until every process group is reaped.
    pub async fn shutdown(&self) {
        let entries = std::mem::take(&mut *lock(&self.inner));
        for entry in entries.into_values() {
            entry.enabled.store(false, Ordering::Release);
            entry.dispose().await;
        }
    }

    #[cfg(any(unix, windows))]
    fn enabled(&self, session: &SessionId) -> bool {
        lock(&self.inner)
            .get(session)
            .is_some_and(|entry| entry.enabled.load(Ordering::Acquire))
    }

    #[cfg(unix)]
    async fn run<F>(
        &self,
        session: &SessionId,
        mode: SandboxMode,
        command: PersistentShellCommand,
        make_plan: F,
    ) -> Result<PersistentShellResult, TessivumError>
    where
        F: FnOnce() -> Result<PersistentBashPlan, TessivumError>,
    {
        let entry = lock(&self.inner)
            .get(session)
            .filter(|entry| entry.enabled.load(Ordering::Acquire))
            .cloned()
            .ok_or_else(|| {
                persistent_bash_error(
                    "PERSISTENT_SHELL_DISABLED",
                    "persistent shell is not enabled for this session",
                    json!({"sessionId": session}),
                )
            })?;
        let mut state = entry.shell.lock().await;
        if !entry.enabled.load(Ordering::Acquire) {
            return Err(persistent_bash_error(
                "PERSISTENT_SHELL_DISABLED",
                "persistent shell is not enabled for this session",
                json!({"sessionId": session}),
            ));
        }
        let shell = match state.as_ref() {
            Some(state) if state.mode == mode => state.shell.clone(),
            Some(state) => {
                return Err(persistent_bash_error(
                    "PERSISTENT_SHELL_SANDBOX_POLICY_MISMATCH",
                    "persistent shell cannot change its fixed sandbox policy",
                    json!({"current": state.mode, "requested": mode}),
                ));
            }
            None => {
                let plan = make_plan()?;
                let validator = Arc::clone(&plan.validator);
                let shell = PersistentShell::start(plan.config, move || validator()).await?;
                *state = Some(PersistentShellState {
                    mode: plan.mode,
                    shell: shell.clone(),
                });
                shell
            }
        };
        let result = shell.run(command).await;
        if result.is_err() {
            let retired = state.take();
            drop(state);
            if let Some(retired) = retired {
                retired.shell.dispose().await;
            }
        }
        result
    }
}

struct PersistentShellSession {
    enabled: AtomicBool,
    #[cfg(unix)]
    shell: tokio::sync::Mutex<Option<PersistentShellState>>,
}

impl PersistentShellSession {
    fn new() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            #[cfg(unix)]
            shell: tokio::sync::Mutex::new(None),
        }
    }

    async fn dispose(&self) {
        #[cfg(unix)]
        let shell = self.shell.lock().await.take();
        #[cfg(unix)]
        if let Some(shell) = shell {
            shell.shell.dispose().await;
        }
    }
}

#[cfg(unix)]
struct PersistentShellState {
    mode: SandboxMode,
    shell: PersistentShell,
}

#[cfg(unix)]
struct PersistentBashPlan {
    mode: SandboxMode,
    config: PersistentShellConfig,
    validator: PersistentShellLeaseValidator,
}

#[cfg(any(unix, windows))]
fn persistent_bash_error(code: &str, message: &str, details: Value) -> TessivumError {
    TessivumError::new(code, message, "tools", details)
}

pub(crate) struct HostToolServices {
    sessions: SessionStore,
    sandbox: Sandbox,
    approval: Arc<dyn ToolApproval>,
    #[cfg(unix)]
    job_owners: BashJobOwners,
    persistent_shells: PersistentShellSessions,

    attachments: Arc<AttachmentStore>,
    web: WebRuntime,
}

impl HostToolServices {
    pub(crate) fn new(
        sessions: SessionStore,
        sandbox: Sandbox,
        approval: Arc<dyn ToolApproval>,
        job_owners: BashJobOwners,
        persistent_shells: PersistentShellSessions,

        attachments: Arc<AttachmentStore>,
        web: WebRuntime,
    ) -> Self {
        #[cfg(not(unix))]
        let _ = job_owners;
        Self {
            sessions,
            sandbox,
            approval,
            #[cfg(unix)]
            job_owners,
            attachments,
            web,
            persistent_shells,
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
    persistent_shells: PersistentShellSessions,
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
    pub fn persistent_shell_sessions(&self) -> PersistentShellSessions {
        self.persistent_shells.clone()
    }

    fn build(
        runtime: &ToolRuntime,
        config: BuiltinToolsConfig,
        services: Option<HostToolServices>,
    ) -> Result<Self, TessivumError> {
        let config = canonical_config(config)?;
        if config.enable_bash && !cfg!(any(unix, windows)) {
            return Err(config_error(
                "UNSUPPORTED_BUILTIN_BASH",
                "builtin bash is supported only on Unix and Windows",
                Value::Null,
            ));
        }
        let persistent_shells = services
            .as_ref()
            .map(|services| services.persistent_shells.clone())
            .unwrap_or_default();

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
        #[cfg(unix)]
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
                    persistent_shells: persistent_shells.clone(),
                },
            ))?);
        }
        #[cfg(windows)]
        if config.enable_bash {
            let (sessions, sandbox, approval) = services
                .map(|services| {
                    (
                        Some(services.sessions),
                        Some(services.sandbox),
                        Some(services.approval),
                    )
                })
                .unwrap_or_default();
            registrations.push(runtime.register(ToolDefinition::new(
                "bash",
                "Runs a PowerShell command under the session's native sandbox policy.",
                bash_schema(),
                WindowsPowerShell {
                    cwd: config.cwd.clone(),
                    resolver: config.resolver.clone(),
                    sessions,
                    sandbox,
                    approval,
                    max_output_bytes: config.max_output_bytes,
                    persistent_shells: persistent_shells.clone(),
                },
            ))?);
        }
        Ok(Self {
            _config: config,
            _registrations: registrations,
            persistent_shells,
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
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn config_error(code: &str, message: &str, details: Value) -> TessivumError {
    TessivumError::new(code, message, "tools", details)
}

fn invalid_arguments(message: &str, details: Value) -> TessivumError {
    TessivumError::new("INVALID_TOOL_ARGUMENTS", message, "tools", details)
}

#[cfg(unix)]
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

#[cfg(any(unix, windows))]
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
    persistent_shells: PersistentShellSessions,
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
        let persistent = self.persistent_shells.enabled(&context.session);
        let lease = match self
            .resolver
            .as_ref()
            .map(|resolver| resolver.resolve(&context.session))
            .transpose()
        {
            Ok(lease) => lease,
            Err(error) => {
                if persistent {
                    self.persistent_shells.retire(&context.session).await;
                }
                return Err(workspace_error(&context, error));
            }
        };
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
        if background && persistent {
            return Err(persistent_bash_error(
                "PERSISTENT_SHELL_BACKGROUND_UNSUPPORTED",
                "persistent shell sessions do not support background Bash execution",
                json!({"sessionId": context.session}),
            ));
        }

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
        if persistent {
            let cwd = self.cwd.clone();
            let resolver = self.resolver.clone();
            let sandbox = self.sandbox.clone();
            let session = context.session.clone();
            let result = self
                .persistent_shells
                .run(
                    &context.session,
                    mode,
                    PersistentShellCommand::new(command).cancelled_by(context.cancellation.clone()),
                    move || {
                        persistent_bash_plan(
                            &cwd,
                            lease,
                            resolver,
                            sandbox.as_ref(),
                            mode,
                            self.max_output_bytes,
                            session,
                        )
                    },
                )
                .await?;
            return Ok(persistent_bash_output(result, self.max_output_bytes));
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

#[cfg(windows)]
struct WindowsPowerShell {
    cwd: PathBuf,
    resolver: Option<Arc<SessionResourceResolver>>,
    sessions: Option<SessionStore>,
    sandbox: Option<Sandbox>,
    approval: Option<Arc<dyn ToolApproval>>,
    max_output_bytes: usize,
    persistent_shells: PersistentShellSessions,
}

#[cfg(windows)]
#[async_trait]
impl ToolHandler for WindowsPowerShell {
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
        if self.persistent_shells.enabled(&context.session) {
            return Err(persistent_bash_error(
                "PERSISTENT_SHELL_UNAVAILABLE",
                "persistent PowerShell is not available yet",
                json!({"sessionId": context.session}),
            ));
        }
        if arguments.get("run_in_background") == Some(&Value::Bool(true)) {
            return Err(persistent_bash_error(
                "BACKGROUND_JOBS_UNAVAILABLE",
                "background PowerShell is not available yet",
                Value::Null,
            ));
        }
        if arguments
            .get("run_in_background")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(invalid_arguments(
                "run_in_background must be a boolean",
                json!({"path": "$.run_in_background"}),
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
                            description: "Runs a PowerShell command under the selected sandbox."
                                .into(),
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
        run_windows_powershell(
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

#[cfg(windows)]
async fn run_windows_powershell(
    cwd: &Path,
    lease: Option<&WorkspaceLease>,
    sandbox: Option<&Sandbox>,
    mode: SandboxMode,
    max_output_bytes: usize,
    context: &ToolRunContext,
    command: &str,
) -> ToolHandlerResult {
    use std::io;
    use tokio::process::Command;

    let script = format!(
        "$OutputEncoding = [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); \
         $global:LASTEXITCODE = $null; try {{ & {{ {command} }}; $ok = $?; $code = $LASTEXITCODE }} \
         catch {{ [Console]::Error.WriteLine($_); exit 1 }}; \
         if ($null -ne $code) {{ exit $code }}; if (-not $ok) {{ exit 1 }}"
    );
    let workspace = match lease {
        Some(lease) => lease
            .validate_current()
            .map_err(|error| workspace_error(context, error))?,
        None => cwd.to_path_buf(),
    };
    let mut last_not_found = None;
    let mut child = None;
    for program in ["pwsh.exe", "powershell.exe"] {
        let raw_argv = vec![
            program.into(),
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script.clone(),
        ];
        let argv = prepare_windows_shell_argv(raw_argv, lease, sandbox, mode, &context.session)?;
        let mut shell = Command::new(&argv[0]);
        shell
            .args(&argv[1..])
            .current_dir(&workspace)
            .env("NO_COLOR", "1")
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        match shell.spawn() {
            Ok(started) => {
                child = Some(started);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => last_not_found = Some(error),
            Err(error) => return Err(bash_error("could not start PowerShell", error)),
        }
    }
    let mut child = child.ok_or_else(|| {
        bash_error(
            "could not find PowerShell 7 or Windows PowerShell",
            last_not_found.unwrap_or_else(|| io::Error::from(io::ErrorKind::NotFound)),
        )
    })?;
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
        status = child.wait() => status.map_err(|error| bash_error("could not wait for PowerShell", error))?,
        _ = context.cancellation.cancelled() => {
            let killed = child.kill().await;
            let waited = child.wait().await;
            finish_copy(stdout_task).await?;
            finish_copy(stderr_task).await?;
            if let Err(error) = waited {
                return Err(bash_error("could not reap cancelled PowerShell", error));
            }
            if let Err(error) = killed {
                if error.kind() != io::ErrorKind::InvalidInput {
                    return Err(bash_error("could not kill cancelled PowerShell", error));
                }
            }
            return Err(TessivumError::new("CANCELLED", "tool call was cancelled", "tools", Value::Null));
        }
    };
    finish_copy(stdout_task).await?;
    finish_copy(stderr_task).await?;
    let mut output =
        std::mem::take(&mut *output.lock().unwrap_or_else(|poison| poison.into_inner()));
    let output_bytes = output.bytes;
    let truncated = output.truncated;
    normalize_windows_newlines(&mut output.stdout);
    normalize_windows_newlines(&mut output.stderr);
    let exit_code = status.code();
    let text = output.text(exit_code);
    Ok(ToolOutput::new(
        vec![ContentBlock::Text { text }],
        !status.success(),
        json!({
            "exitCode": exit_code,
            "signal": Value::Null,
            "outputBytes": output_bytes,
            "truncated": truncated,
        }),
    ))
}
#[cfg(windows)]
fn normalize_windows_newlines(bytes: &mut Vec<u8>) {
    let mut write = 0;
    for read in 0..bytes.len() {
        if bytes[read] == b'\r' && bytes.get(read + 1) == Some(&b'\n') {
            continue;
        }
        bytes[write] = bytes[read];
        write += 1;
    }
    bytes.truncate(write);
}

#[cfg(windows)]
fn prepare_windows_shell_argv(
    argv: Vec<String>,
    lease: Option<&WorkspaceLease>,
    sandbox: Option<&Sandbox>,
    mode: SandboxMode,
    session: &SessionId,
) -> Result<Vec<String>, TessivumError> {
    if let (Some(sandbox), Some(lease)) = (sandbox, lease) {
        let workspace = lease
            .validate_current()
            .map_err(|error| workspace_error_for_session(session, error))?;
        return Ok(sandbox
            .prepare(
                &SandboxRequest {
                    mode,
                    workspace: workspace.clone(),
                    read_policy: SandboxReadPolicy::Deny,
                    read_roots: Vec::new(),
                    write_roots: vec![workspace],
                    approval: (mode == SandboxMode::WorkspaceWrite).then_some(SandboxApproval {
                        mode: Some(SandboxMode::WorkspaceWrite),
                        read_policy: None,
                    }),
                },
                &argv,
            )?
            .argv);
    }
    Ok(argv)
}

#[cfg(unix)]
fn prepare_bash_argv(
    argv: Vec<String>,
    lease: Option<&WorkspaceLease>,
    sandbox: Option<&Sandbox>,
    mode: SandboxMode,
    session: &SessionId,
) -> Result<Vec<String>, TessivumError> {
    if let (Some(sandbox), Some(lease)) = (sandbox, lease) {
        let workspace = lease
            .validate_current()
            .map_err(|error| workspace_error_for_session(session, error))?;
        let write_roots = vec![workspace.clone()];
        return Ok(sandbox
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
            .argv);
    }
    Ok(argv)
}

#[cfg(unix)]
fn persistent_bash_plan(
    cwd: &Path,
    lease: Option<WorkspaceLease>,
    resolver: Option<Arc<SessionResourceResolver>>,
    sandbox: Option<&Sandbox>,
    mode: SandboxMode,
    max_output_bytes: usize,
    session: SessionId,
) -> Result<PersistentBashPlan, TessivumError> {
    let raw_argv = vec!["/bin/sh".into(), "-s".into()];
    let (workspace, cwd_fd, argv, validator) = if let Some(lease) = lease {
        let workspace = lease
            .validate_current()
            .map_err(|error| workspace_error_for_session(&session, error))?;
        let cwd_fd = lease
            .directory_fd()
            .map_err(|error| workspace_error_for_session(&session, error))?;
        let argv = prepare_bash_argv(raw_argv, Some(&lease), sandbox, mode, &session)?;
        let expected_workspace = lease.workspace_id().clone();
        let lease = Arc::new(lease);
        let validator: PersistentShellLeaseValidator = Arc::new(move || {
            lease
                .validate_current()
                .map_err(|error| workspace_error_for_session(&session, error))?;
            if let Some(resolver) = &resolver {
                let current = resolver
                    .resolve(&session)
                    .map_err(|error| workspace_error_for_session(&session, error))?;
                if current.workspace_id() != &expected_workspace {
                    return Err(TessivumError::new(
                        "STALE_WORKSPACE_LEASE",
                        "workspace lease is stale",
                        "tools",
                        json!({"sessionId": session}),
                    ));
                }
            }
            Ok(())
        });
        (workspace, Some(cwd_fd), argv, validator)
    } else {
        (
            cwd.to_path_buf(),
            None,
            raw_argv,
            Arc::new(|| Ok(())) as PersistentShellLeaseValidator,
        )
    };
    let mut config = PersistentShellConfig::new(workspace);
    config.argv = argv;
    config.cwd_fd = cwd_fd;
    config.max_output_bytes = max_output_bytes;
    Ok(PersistentBashPlan {
        mode,
        config,
        validator,
    })
}

#[cfg(unix)]
fn persistent_bash_output(result: PersistentShellResult, max_output_bytes: usize) -> ToolOutput {
    let mut output = CapturedOutput::default();
    let stdout = result.stdout.tail;
    let stderr = result.stderr.tail;
    let total_bytes = result
        .stdout
        .total_bytes
        .saturating_add(result.stderr.total_bytes);
    let stdout_len = stdout.len().min(max_output_bytes);
    output.stdout.extend_from_slice(&stdout[..stdout_len]);
    let stderr_len = stderr
        .len()
        .min(max_output_bytes.saturating_sub(stdout_len));
    output.stderr.extend_from_slice(&stderr[..stderr_len]);
    output.bytes = stdout_len + stderr_len;
    output.truncated = total_bytes > output.bytes as u64;
    let output_bytes = output.bytes;
    let truncated = output.truncated;
    let text = output.text(Some(result.exit_code));
    ToolOutput::new(
        vec![ContentBlock::Text { text }],
        result.exit_code != 0,
        json!({
            "exitCode": result.exit_code,
            "signal": Value::Null,
            "outputBytes": output_bytes,
            "truncated": truncated,
        }),
    )
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

    let argv = prepare_bash_argv(
        vec!["/bin/sh".into(), "-lc".into(), "--".into(), command.into()],
        lease,
        sandbox,
        mode,
        &context.session,
    )?;
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

#[cfg(any(unix, windows))]
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

#[cfg(any(unix, windows))]
fn bash_error(message: &str, error: std::io::Error) -> TessivumError {
    TessivumError::new(
        "BASH_EXECUTION_FAILED",
        message,
        "tools",
        json!({"error": error.to_string()}),
    )
}

fn workspace_error(context: &ToolRunContext, error: WorkspaceError) -> TessivumError {
    workspace_error_for_session(&context.session, error)
}

fn workspace_error_for_session(session: &SessionId, error: WorkspaceError) -> TessivumError {
    TessivumError::new(
        error.code(),
        error.to_string(),
        "tools",
        json!({"sessionId": session}),
    )
}

#[cfg(any(unix, windows))]
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

#[cfg(any(unix, windows))]
#[derive(Default)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    bytes: usize,
    truncated: bool,
}

#[cfg(any(unix, windows))]
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

#[cfg(any(unix, windows))]
#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

#[cfg(any(unix, windows))]
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

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, sync::Arc};

    use super::*;
    use crate::{
        sandbox::{
            EffectiveSandboxRequest, RunnerRules, SandboxEnforcement, SandboxPlan, SandboxProvider,
        },
        workspace::WorkspaceRegistry,
    };

    #[derive(Clone)]
    struct RecordingSandbox {
        argv: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl SandboxProvider for RecordingSandbox {
        fn confine(
            &self,
            _: &EffectiveSandboxRequest,
            argv: &[String],
        ) -> Result<SandboxPlan, TessivumError> {
            lock(&self.argv).push(argv.to_vec());
            Ok(SandboxPlan {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "exec \"$@\"".into(),
                    "tessivum-persistent-wrapper".into(),
                    "/bin/sh".into(),
                    "-s".into(),
                ],
                enforcement: SandboxEnforcement::Full,
                denial: None,
                runner_rules: RunnerRules::default(),
            })
        }
    }

    #[tokio::test]
    async fn persistent_bash_uses_the_sandbox_wrapped_shell_argv() {
        let root =
            std::env::temp_dir().join(format!("tessivum-persistent-bash-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let registry = WorkspaceRegistry::open(root.join("data"), &root, Vec::new()).unwrap();
        let session = SessionId::from("sandboxed-persistent-shell");
        let workspace = registry.list()[0].workspace_id.clone();
        registry.recognize_session(&session).unwrap();
        registry.attach_session(&workspace, &session, None).unwrap();
        let argv = Arc::new(Mutex::new(Vec::new()));
        let persistent_shells = PersistentShellSessions::new();
        persistent_shells.enable(session.clone());
        let runtime = ToolRuntime::new();
        let _registration = runtime
            .register(ToolDefinition::new(
                "bash",
                "test Bash",
                bash_schema(),
                Bash {
                    cwd: root.clone(),
                    resolver: Some(Arc::new(SessionResourceResolver::new(registry))),
                    sessions: None,
                    sandbox: Some(Sandbox::new(Some(Arc::new(RecordingSandbox {
                        argv: Arc::clone(&argv),
                    })))),
                    approval: None,
                    max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
                    job_owners: None,
                    persistent_shells: persistent_shells.clone(),
                },
            ))
            .unwrap();
        let root_context = tessivum_core::ContextHandle::root();
        let context = ToolRunContext {
            session,
            call: crate::ToolCallId::from("sandboxed-persistent-shell"),
            cancellation: root_context.scope().cancellation(),
        };
        let output = runtime
            .execute(context, "bash", json!({"command": "printf wrapped"}))
            .await;
        assert!(!output.is_error);
        assert!(matches!(
            output.content.as_slice(),
            [ContentBlock::Text { text }] if text == "wrapped"
        ));
        assert_eq!(
            lock(&argv).as_slice(),
            &[vec!["/bin/sh".to_owned(), "-s".to_owned()]],
        );
        persistent_shells.shutdown().await;
        let _ = fs::remove_dir_all(root);
    }
}

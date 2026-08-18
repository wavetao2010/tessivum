//! Explicitly enabled process-local tools for a headless composition.
//!
//! `bash` is a native, trusted capability, not a sandbox. Enabling it grants the
//! configured working directory's host permissions; callers must still apply
//! their normal approval and sandbox policy before allowing a model to invoke it.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    workspace::{SessionResourceResolver, WorkspaceError, WorkspaceLease},
    ContentBlock, TessivumError,
};

/// Default upper bound for combined `bash` stdout and stderr.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
/// Hard ceiling for combined `bash` stdout and stderr.
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

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
    /// Registers `echo` and, when explicitly enabled, trusted native `bash`.
    pub fn new(runtime: &ToolRuntime, config: BuiltinToolsConfig) -> Result<Self, TessivumError> {
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
        if config.enable_bash {
            #[cfg(unix)]
            registrations.push(runtime.register(ToolDefinition::new(
                "bash",
                "Runs a trusted native shell command in the configured directory; this is not a sandbox.",
                bash_schema(),
                Bash {
                    cwd: config.cwd.clone(),
                    resolver: config.resolver.clone(),
                    max_output_bytes: config.max_output_bytes,
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

fn echo_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"text": {"type": "string"}},
        "required": ["text"],
        "additionalProperties": false
    })
}

fn bash_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string"},
            "description": {"type": "string"}
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

#[cfg(unix)]
struct Bash {
    cwd: PathBuf,
    resolver: Option<Arc<SessionResourceResolver>>,
    max_output_bytes: usize,
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
        run_bash(
            &self.cwd,
            lease.as_ref(),
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
    max_output_bytes: usize,
    context: &ToolRunContext,
    command: &str,
) -> ToolHandlerResult {
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    use tokio::process::Command;

    let cwd = match lease {
        Some(lease) => lease
            .execution_cwd()
            .map_err(|error| workspace_error(context, error))?,
        None => cwd.to_path_buf(),
    };
    let mut child = Command::new("/bin/sh")
        .args(["-lc", "--", command])
        .current_dir(&cwd)
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
    let text = output.text();
    Ok(ToolOutput::new(
        vec![ContentBlock::Text { text }],
        !status.success(),
        json!({
            "exitCode": status.code(),
            "outputBytes": output_bytes,
            "truncated": truncated,
        }),
    ))
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

    fn text(mut self) -> String {
        self.stdout.append(&mut self.stderr);
        String::from_utf8_lossy(&self.stdout).into_owned()
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

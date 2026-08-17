//! One fresh child process per hostile code run.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::pending,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{stream::FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::Mutex as AsyncMutex,
    time::{sleep_until, Instant},
};

pub fn code_runtime_service_key() -> ServiceKey {
    ServiceKey::new("harness.code-runtime", "1")
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodeLanguage {
    JavaScript,
    Python,
}
impl CodeLanguage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JavaScript => "javascript",
            Self::Python => "python",
        }
    }
}

pub type CodeJsonValue = Value;

#[async_trait]
pub trait CodeBinding: Send + Sync {
    async fn call(&self, arguments: Value) -> Result<Value, String>;
}
#[async_trait]
impl<F, Fut> CodeBinding for F
where
    F: Send + Sync + Fn(Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>> + Send,
{
    async fn call(&self, arguments: Value) -> Result<Value, String> {
        (self)(arguments).await
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeBindingErrorClass {
    pub name: String,
    pub member_name_property: String,
}

#[derive(Clone)]
pub struct CodeBindingNamespace {
    pub global: String,
    pub functions: BTreeMap<String, Arc<dyn CodeBinding>>,
    pub error_class: Option<CodeBindingErrorClass>,
}
impl std::fmt::Debug for CodeBindingNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeBindingNamespace")
            .field("global", &self.global)
            .field("functions", &self.functions.keys())
            .field("error_class", &self.error_class)
            .finish()
    }
}
impl CodeBindingNamespace {
    pub fn new(global: impl Into<String>) -> Self {
        Self {
            global: global.into(),
            functions: BTreeMap::new(),
            error_class: None,
        }
    }
    pub fn function(
        mut self,
        name: impl Into<String>,
        binding: impl CodeBinding + 'static,
    ) -> Self {
        self.functions.insert(name.into(), Arc::new(binding));
        self
    }
    pub fn error_class(mut self, error_class: CodeBindingErrorClass) -> Self {
        self.error_class = Some(error_class);
        self
    }
}

#[derive(Clone, Debug)]
pub struct CodeRunRequest {
    pub program: String,
    pub bindings: Vec<CodeBindingNamespace>,
    pub cancellation: Option<CancellationToken>,
}
impl CodeRunRequest {
    pub fn new(program: impl Into<String>, bindings: Vec<CodeBindingNamespace>) -> Self {
        Self {
            program: program.into(),
            bindings,
            cancellation: None,
        }
    }
    pub fn cancelled_by(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodeRunFailureKind {
    Exception,
    Timeout,
    Abort,
    WorkerExit,
    InvalidOutput,
    OutputLimit,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeRunFailure {
    pub kind: CodeRunFailureKind,
    pub message: String,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeRunResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    pub logs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CodeRunFailure>,
}

#[derive(Debug, Error)]
pub enum CodeRuntimeError {
    #[error("code runtime is disposed")]
    Disposed,
    #[error("invalid code-runtime configuration: {0}")]
    InvalidConfiguration(String),
    #[error("invalid code binding: {0}")]
    InvalidBinding(String),
    #[error(transparent)]
    Core(#[from] CoreError),
}

#[derive(Clone, Debug)]
pub struct ProcessCodeRuntimeConfig {
    pub language: CodeLanguage,
    pub executable: PathBuf,
    pub isolation: String,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}
impl ProcessCodeRuntimeConfig {
    pub fn javascript(executable: impl Into<PathBuf>) -> Self {
        Self {
            language: CodeLanguage::JavaScript,
            executable: executable.into(),
            isolation: "process".into(),
            timeout: Duration::from_secs(60),
            max_output_bytes: 1_048_576,
        }
    }
    pub fn python(executable: impl Into<PathBuf>) -> Self {
        Self {
            language: CodeLanguage::Python,
            executable: executable.into(),
            isolation: "process".into(),
            timeout: Duration::from_secs(60),
            max_output_bytes: 1_048_576,
        }
    }
    fn validate(&mut self) -> Result<(), CodeRuntimeError> {
        if self.executable.as_os_str().is_empty() {
            return Err(CodeRuntimeError::InvalidConfiguration(
                "executable must not be empty".into(),
            ));
        }
        self.executable = resolve_executable(&self.executable)?;
        if self.timeout.is_zero() || self.max_output_bytes < 4 {
            return Err(CodeRuntimeError::InvalidConfiguration(
                "timeout must be positive and output cap must be at least four bytes".into(),
            ));
        }
        if self.isolation.is_empty()
            || !self
                .isolation
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'-')
        {
            return Err(CodeRuntimeError::InvalidConfiguration(
                "isolation must be a lowercase identifier".into(),
            ));
        }
        Ok(())
    }
}

fn resolve_executable(path: &std::path::Path) -> Result<PathBuf, CodeRuntimeError> {
    if path.components().count() > 1 || path.is_absolute() {
        return executable(path).then(|| path.to_path_buf()).ok_or_else(|| {
            CodeRuntimeError::InvalidConfiguration(format!("executable {path:?} is not executable"))
        });
    }
    let cwd = std::env::current_dir().map_err(|error| {
        CodeRuntimeError::InvalidConfiguration(format!(
            "could not resolve executable path: {error}"
        ))
    })?;
    let Some(search_path) = std::env::var_os("PATH") else {
        return Err(CodeRuntimeError::InvalidConfiguration(format!(
            "executable {path:?} is not executable on PATH"
        )));
    };
    for directory in std::env::split_paths(&search_path) {
        let candidate = directory.join(path);
        if executable(&candidate) {
            return Ok(if candidate.is_absolute() {
                candidate
            } else {
                cwd.join(candidate)
            });
        }
    }
    Err(CodeRuntimeError::InvalidConfiguration(format!(
        "executable {path:?} is not executable on PATH"
    )))
}

#[cfg(unix)]
fn executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &std::path::Path) -> bool {
    path.is_file()
}

#[async_trait]
pub trait CodeRuntime: Send + Sync {
    fn language(&self) -> &str;
    fn isolation(&self) -> &str;
    async fn run(&self, request: CodeRunRequest) -> Result<CodeRunResult, CodeRuntimeError>;
    async fn dispose(&self) -> Result<(), CodeRuntimeError>;
}

#[derive(Clone)]
pub struct ProcessCodeRuntime {
    inner: Arc<Inner>,
}
struct Inner {
    config: ProcessCodeRuntimeConfig,
    disposed: AtomicBool,
    next: AtomicU64,
    live: Mutex<BTreeMap<u64, CancellationToken>>,
    dispose_gate: AsyncMutex<()>,
}
impl ProcessCodeRuntime {
    pub fn new(mut config: ProcessCodeRuntimeConfig) -> Result<Self, CodeRuntimeError> {
        config.validate()?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                disposed: AtomicBool::new(false),
                next: AtomicU64::new(0),
                live: Mutex::new(BTreeMap::new()),
                dispose_gate: AsyncMutex::new(()),
            }),
        })
    }
    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(code_runtime_service_key(), self)
    }
    fn reserve(&self) -> Result<(u64, CancellationToken), CodeRuntimeError> {
        if self.inner.disposed.load(Ordering::Acquire) {
            return Err(CodeRuntimeError::Disposed);
        }
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let token = ContextHandle::root().scope().cancellation();
        let mut live = lock(&self.inner.live);
        if self.inner.disposed.load(Ordering::Acquire) {
            return Err(CodeRuntimeError::Disposed);
        }
        live.insert(id, token.clone());
        Ok((id, token))
    }
    async fn execute(
        &self,
        request: CodeRunRequest,
        bindings: Bindings,
        runtime_cancel: CancellationToken,
    ) -> CodeRunResult {
        let mut ledger = Ledger::new(self.inner.config.max_output_bytes);
        if request
            .cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return ledger.failure(failure(
                CodeRunFailureKind::Abort,
                "run cancelled before process start",
            ));
        }
        let mut command = Command::new(&self.inner.config.executable);
        match self.inner.config.language {
            CodeLanguage::JavaScript => {
                command.arg("--disable-proto=throw").arg("-e").arg(JS);
            }
            CodeLanguage::Python => {
                command.arg("-I").arg("-c").arg(PY);
            }
        }
        command
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ledger.failure(failure(
                    CodeRunFailureKind::WorkerExit,
                    format!("could not start process: {error}"),
                ))
            }
        };
        let Some(mut stdin) = child.stdin.take() else {
            return reap(child, ledger, "process did not provide stdin").await;
        };
        let Some(stdout) = child.stdout.take() else {
            return reap(child, ledger, "process did not provide stdout").await;
        };
        let Some(stderr) = child.stderr.take() else {
            return reap(child, ledger, "process did not provide stderr").await;
        };
        let boot = json!({"program":request.program,"bindings":bindings.manifest});
        let mut outcome = write(&mut stdin, &boot).await.err().map(|e| {
            Outcome::Failure(failure(
                CodeRunFailureKind::WorkerExit,
                format!("could not initialize process: {e}"),
            ))
        });
        let mut output = BufReader::new(stdout).lines();
        let mut errors = BufReader::new(stderr).lines();
        let mut output_open = true;
        let mut errors_open = true;
        let mut exited = false;
        let mut pending: FuturesUnordered<
            Pin<Box<dyn std::future::Future<Output = Reply> + Send>>,
        > = FuturesUnordered::new();
        let deadline = Instant::now() + self.inner.config.timeout;
        while outcome.is_none() && !(exited && !output_open && !errors_open) {
            tokio::select! {
                line = output.next_line(), if output_open => match line {
                    Ok(Some(line)) => match frame(&line) {
                        Ok(Frame::Log(text)) => if !ledger.log(text) { outcome = Some(Outcome::Failure(limit(ledger.max))); },
                        Ok(Frame::Call { id, global, name, arguments }) => {
                            let binding = bindings.names.get(&global).and_then(|n| n.get(&name)).cloned();
                            pending.push(Box::pin(async move { Reply { id, result: match binding { Some(binding) => binding.call(arguments).await, None => Err(format!("unknown binding {global:?}.{name:?}")) } } }));
                        }
                        Ok(Frame::Done(done)) => outcome = Some(done),
                        Err(message) => outcome = Some(Outcome::Failure(failure(CodeRunFailureKind::InvalidOutput, message))),
                    },
                    Ok(None) => output_open = false,
                    Err(error) => outcome = Some(Outcome::Failure(failure(CodeRunFailureKind::WorkerExit, format!("could not read process output: {error}")))),
                },
                line = errors.next_line(), if errors_open => match line {
                    Ok(Some(line)) => if !ledger.log(line) { outcome = Some(Outcome::Failure(limit(ledger.max))); },
                    Ok(None) => errors_open = false,
                    Err(error) => outcome = Some(Outcome::Failure(failure(CodeRunFailureKind::WorkerExit, format!("could not read process errors: {error}")))),
                },
                reply = pending.next(), if !pending.is_empty() => if let Some(reply) = reply {
                    if let Err(error) = reply.write_to(&mut stdin).await { outcome = Some(Outcome::Failure(failure(CodeRunFailureKind::WorkerExit, format!("could not reply to binding call: {error}")))); }
                },
                waited = child.wait(), if !exited => match waited { Ok(_) => exited = true, Err(error) => { exited = true; outcome = Some(Outcome::Failure(failure(CodeRunFailureKind::WorkerExit, format!("could not wait for process: {error}")))); } },
                _ = sleep_until(deadline) => outcome = Some(Outcome::Failure(failure(CodeRunFailureKind::Timeout, format!("wall-clock ceiling reached ({}ms)", self.inner.config.timeout.as_millis())))),
                _ = cancelled(request.cancellation.clone()) => outcome = Some(Outcome::Failure(failure(CodeRunFailureKind::Abort, "run cancelled"))),
                _ = runtime_cancel.cancelled() => outcome = Some(Outcome::Failure(failure(CodeRunFailureKind::Abort, "runtime disposed"))),
            }
        }
        if outcome.is_none() {
            outcome = Some(Outcome::Failure(failure(
                CodeRunFailureKind::WorkerExit,
                "process exited before completing",
            )));
        }
        if !exited {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        // Preserve logs written before termination, never a trailing forged terminal frame.
        while output_open {
            match output.next_line().await {
                Ok(Some(line)) => {
                    let _ = ledger.log(line);
                }
                Ok(None) | Err(_) => output_open = false,
            }
        }
        while errors_open {
            match errors.next_line().await {
                Ok(Some(line)) => {
                    let _ = ledger.log(line);
                }
                Ok(None) | Err(_) => errors_open = false,
            }
        }
        match outcome.expect("outcome set") {
            Outcome::Success(value) => ledger.success(value),
            Outcome::Failure(error) => ledger.failure(error),
        }
    }
}
#[async_trait]
impl CodeRuntime for ProcessCodeRuntime {
    fn language(&self) -> &str {
        self.inner.config.language.as_str()
    }
    fn isolation(&self) -> &str {
        &self.inner.config.isolation
    }
    async fn run(&self, request: CodeRunRequest) -> Result<CodeRunResult, CodeRuntimeError> {
        let bindings = bindings(request.bindings.clone())?;
        let (id, token) = self.reserve()?;
        let result = self.execute(request, bindings, token).await;
        lock(&self.inner.live).remove(&id);
        Ok(result)
    }
    async fn dispose(&self) -> Result<(), CodeRuntimeError> {
        let _gate = self.inner.dispose_gate.lock().await;
        self.inner.disposed.store(true, Ordering::Release);
        for token in lock(&self.inner.live).values() {
            token.cancel();
        }
        while !lock(&self.inner.live).is_empty() {
            tokio::task::yield_now().await;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct Bindings {
    names: BTreeMap<String, BTreeMap<String, Arc<dyn CodeBinding>>>,
    manifest: Vec<Value>,
}
fn bindings(input: Vec<CodeBindingNamespace>) -> Result<Bindings, CodeRuntimeError> {
    let mut names = BTreeMap::new();
    let mut injected = BTreeSet::new();
    let mut manifest = Vec::new();
    for namespace in input {
        identifier(&namespace.global, "binding global")?;
        if RESERVED_GLOBALS.contains(&namespace.global.as_str()) {
            return Err(CodeRuntimeError::InvalidBinding(format!(
                "reserved binding global {:?}",
                namespace.global
            )));
        }
        if !injected.insert(namespace.global.clone()) {
            return Err(CodeRuntimeError::InvalidBinding(format!(
                "duplicate injected global {:?}",
                namespace.global
            )));
        }
        if let Some(class) = &namespace.error_class {
            identifier(&class.name, "binding error class")?;
            if RESERVED_GLOBALS.contains(&class.name.as_str())
                || !injected.insert(class.name.clone())
            {
                return Err(CodeRuntimeError::InvalidBinding(format!(
                    "duplicate or reserved error class {:?}",
                    class.name
                )));
            }
            if class.member_name_property.is_empty()
                || RESERVED_MEMBERS.contains(&class.member_name_property.as_str())
                || dunder(&class.member_name_property)
            {
                return Err(CodeRuntimeError::InvalidBinding(
                    "unusable binding error member property".into(),
                ));
            }
        }
        manifest.push(json!({"global":namespace.global,"names":namespace.functions.keys().collect::<Vec<_>>(),"errorClass":namespace.error_class}));
        names.insert(namespace.global, namespace.functions);
    }
    Ok(Bindings { names, manifest })
}
fn identifier(name: &str, label: &str) -> Result<(), CodeRuntimeError> {
    let mut chars = name.bytes();
    let Some(first) = chars.next() else {
        return Err(CodeRuntimeError::InvalidBinding(format!(
            "{label} is empty"
        )));
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
        || RESERVED_WORDS.contains(&name)
    {
        return Err(CodeRuntimeError::InvalidBinding(format!(
            "{label} {name:?} is not portable"
        )));
    }
    Ok(())
}
fn dunder(name: &str) -> bool {
    name.starts_with("__") && name.ends_with("__") && name.len() > 4
}
const RESERVED_GLOBALS: [&str; 5] = [
    "console",
    "__dsh_main__",
    "__builtins__",
    "__name__",
    "__debug__",
];
const RESERVED_MEMBERS: [&str; 6] = [
    "name",
    "message",
    "stack",
    "args",
    "with_traceback",
    "add_note",
];
const RESERVED_WORDS: [&str; 71] = [
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "let",
    "static",
    "implements",
    "interface",
    "package",
    "private",
    "protected",
    "public",
    "arguments",
    "eval",
    "False",
    "None",
    "True",
    "and",
    "as",
    "assert",
    "async",
    "def",
    "del",
    "elif",
    "except",
    "from",
    "global",
    "is",
    "lambda",
    "nonlocal",
    "not",
    "or",
    "pass",
    "raise",
    "match",
    "type",
    "_",
];

enum Outcome {
    Success(Option<Value>),
    Failure(CodeRunFailure),
}
enum Frame {
    Log(String),
    Call {
        id: u64,
        global: String,
        name: String,
        arguments: Value,
    },
    Done(Outcome),
}
fn frame(line: &str) -> Result<Frame, String> {
    let value: Value = serde_json::from_str(line)
        .map_err(|_| "process emitted malformed JSON control output".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "process emitted non-object control output".to_owned())?;
    match object.get("type").and_then(Value::as_str) {
        Some("log") => object
            .get("text")
            .and_then(Value::as_str)
            .map(|s| Frame::Log(s.into()))
            .ok_or_else(|| "log has no string text".into()),
        Some("call") => Ok(Frame::Call {
            id: object
                .get("id")
                .and_then(Value::as_u64)
                .ok_or_else(|| "call has no unsigned id".to_owned())?,
            global: object
                .get("global")
                .and_then(Value::as_str)
                .ok_or_else(|| "call has no global".to_owned())?
                .into(),
            name: object
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "call has no name".to_owned())?
                .into(),
            arguments: object
                .get("args")
                .cloned()
                .ok_or_else(|| "call has no JSON args".to_owned())?,
        }),
        Some("done") => {
            if let Some(error) = object.get("error") {
                Ok(Frame::Done(Outcome::Failure(parse_failure(error)?)))
            } else {
                Ok(Frame::Done(Outcome::Success(object.get("value").cloned())))
            }
        }
        _ => Err("process emitted unknown control output".into()),
    }
}
fn parse_failure(value: &Value) -> Result<CodeRunFailure, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "failure is not an object".to_owned())?;
    let kind = match object.get("kind").and_then(Value::as_str) {
        Some("exception") => CodeRunFailureKind::Exception,
        Some("invalid-output") => CodeRunFailureKind::InvalidOutput,
        Some("output-limit") => CodeRunFailureKind::OutputLimit,
        _ => return Err("invalid process failure kind".into()),
    };
    Ok(CodeRunFailure {
        kind,
        message: object
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| "failure has no string message".to_owned())?
            .into(),
    })
}
struct Reply {
    id: u64,
    result: Result<Value, String>,
}
impl Reply {
    async fn write_to(self, stdin: &mut ChildStdin) -> std::io::Result<()> {
        write(
            stdin,
            &match self.result {
                Ok(value) => json!({"type":"reply","id":self.id,"ok":true,"value":value}),
                Err(message) => json!({"type":"reply","id":self.id,"ok":false,"message":message}),
            },
        )
        .await
    }
}
async fn write(stdin: &mut ChildStdin, value: &Value) -> std::io::Result<()> {
    stdin
        .write_all(&serde_json::to_vec(value).expect("JSON values serialize"))
        .await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}
async fn cancelled(token: Option<CancellationToken>) {
    match token {
        Some(token) => token.cancelled().await,
        None => pending::<()>().await,
    }
}
async fn reap(mut child: Child, ledger: Ledger, message: &str) -> CodeRunResult {
    let _ = child.kill().await;
    let _ = child.wait().await;
    ledger.failure(failure(CodeRunFailureKind::WorkerExit, message))
}
fn failure(kind: CodeRunFailureKind, message: impl Into<String>) -> CodeRunFailure {
    CodeRunFailure {
        kind,
        message: message.into(),
    }
}
fn limit(max: usize) -> CodeRunFailure {
    failure(
        CodeRunFailureKind::OutputLimit,
        format!("outer output exceeded {max} bytes"),
    )
}
struct Ledger {
    max: usize,
    bytes: usize,
    entries: usize,
    logs: Vec<String>,
    overflow: bool,
}
impl Ledger {
    fn new(max: usize) -> Self {
        Self {
            max,
            bytes: 2,
            entries: 0,
            logs: Vec::new(),
            overflow: false,
        }
    }
    fn log(&mut self, text: String) -> bool {
        let bytes = json_bytes(&Value::String(text.clone()));
        let separator = usize::from(self.entries > 0);
        if self.overflow || bytes > self.max.saturating_sub(self.bytes + separator) {
            self.overflow = true;
            return false;
        }
        self.bytes += bytes + separator;
        self.entries += 1;
        self.logs.push(text);
        true
    }
    fn success(self, value: Option<Value>) -> CodeRunResult {
        if self.overflow
            || value
                .as_ref()
                .is_some_and(|v| json_bytes(v) > self.max.saturating_sub(self.bytes))
        {
            return self.output_limit();
        }
        CodeRunResult {
            value,
            logs: self.logs,
            error: None,
        }
    }
    fn failure(self, error: CodeRunFailure) -> CodeRunResult {
        if self.overflow
            || json_bytes(&Value::String(error.message.clone()))
                > self.max.saturating_sub(self.bytes)
        {
            return self.output_limit();
        }
        CodeRunResult {
            value: None,
            logs: self.logs,
            error: Some(error),
        }
    }
    fn output_limit(self) -> CodeRunResult {
        CodeRunResult {
            value: None,
            logs: self.logs,
            error: Some(limit(self.max)),
        }
    }
}
fn json_bytes(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |v| v.len())
}
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}

const JS: &str = r#"'use strict';
const rl=require('node:readline'),out=process.stdout.write.bind(process.stdout),emit=x=>out(JSON.stringify(x)+'\n');
const boot=rl.createInterface({input:process.stdin,crlfDelay:Infinity});boot.once('line',async line=>{boot.close();try{const d=JSON.parse(line),pending=new Map(),input=rl.createInterface({input:process.stdin,crlfDelay:Infinity});let n=1;input.on('line',l=>{try{const r=JSON.parse(l),p=pending.get(r.id);if(!p)return;pending.delete(r.id);r.ok?p.resolve(r.value):p.reject(new Error(String(r.message)))}catch{}});const valid=(v,seen=new Set())=>v===null||['boolean','string'].includes(typeof v)||(typeof v==='number'&&Number.isFinite(v))||(typeof v==='object'&&!seen.has(v)&&(seen.add(v),Array.isArray(v)?v.every(x=>valid(x,seen)):Object.keys(v).every(k=>valid(v[k],seen)),seen.delete(v),true));const classes=new Map();for(const b of d.bindings)if(b.errorClass){const e=b.errorClass;classes.set(b.global,class extends Error{constructor(member,message){super(message);Object.defineProperty(this,'name',{value:e.name});Object.defineProperty(this,e.memberNameProperty,{value:member,enumerable:true})}})}const call=(g,m,a)=>{const E=classes.get(g);if(!valid(a))return Promise.reject(E?new E(m,'binding arguments must be lossless JSON'):new Error('binding arguments must be lossless JSON'));return new Promise((resolve,reject)=>{const id=n++;pending.set(id,{resolve,reject:e=>reject(E?new E(m,String(e.message||e)):e)});emit({type:'call',id,global:g,name:m,args:a})})};const ps=[],vs=[];for(const b of d.bindings){const o=Object.create(null);for(const m of b.names)Object.defineProperty(o,m,{enumerable:true,value:a=>call(b.global,m,a)});Object.freeze(o);ps.push(b.global);vs.push(o);if(b.errorClass){ps.push(b.errorClass.name);vs.push(classes.get(b.global))}}const render=a=>a.map(x=>typeof x==='string'?x:JSON.stringify(x)).join(' '),console={log:(...a)=>emit({type:'log',text:render(a)}),info:(...a)=>emit({type:'log',text:render(a)}),warn:(...a)=>emit({type:'log',text:render(a)}),error:(...a)=>emit({type:'log',text:render(a)}),debug:(...a)=>emit({type:'log',text:render(a)})};const F=Object.getPrototypeOf(async function(){}).constructor,v=await new F(...ps,'console',`'use strict';\n${d.program}`)(...vs,console);if(v===undefined)emit({type:'done'});else if(valid(v))emit({type:'done',value:v});else emit({type:'done',error:{kind:'invalid-output',message:'program completion must be lossless JSON'}})}catch(e){emit({type:'done',error:{kind:'exception',message:String(e&&e.message||e)}})}});"#;
const PY: &str = r#"import asyncio,builtins,json,math,sys
def emit(x): sys.stdout.write(json.dumps(x,separators=(',',':'))+'\n');sys.stdout.flush()
def valid(x,seen=None):
 if x is None or isinstance(x,(bool,str)): return True
 if isinstance(x,(int,float)) and not isinstance(x,bool): return isinstance(x,int) or math.isfinite(x)
 if not isinstance(x,(list,dict)): return False
 seen=set() if seen is None else seen
 if id(x) in seen:return False
 seen.add(id(x));r=all(valid(v,seen) for v in x) if isinstance(x,list) else all(isinstance(k,str) and valid(v,seen) for k,v in x.items());seen.remove(id(x));return r
try:
 d=json.loads(sys.stdin.readline());lock=asyncio.Lock();next_id=[1]
 async def call(g,m,a,E):
  if not valid(a): raise Exception('binding arguments must be lossless JSON')
  async with lock:
   i=next_id[0];next_id[0]+=1;emit({'type':'call','id':i,'global':g,'name':m,'args':a});r=json.loads(await asyncio.to_thread(sys.stdin.readline))
  if r.get('id')!=i or not r.get('ok'):
   e=E(str(r.get('message','binding call failed'))) if E else Exception(str(r.get('message','binding call failed')))
   if E:setattr(e,E.member,m)
   raise e
  return r.get('value')
 env={'__builtins__':builtins}
 for b in d['bindings']:
  E=None
  if b.get('errorClass'):
   q=b['errorClass'];E=type(q['name'],(Exception,),{'member':q['memberNameProperty']});env[q['name']]=E
  o={}
  for m in b['names']:
   async def fn(a,g=b['global'],n=m,e=E):return await call(g,n,a,e)
   o[m]=fn
  env[b['global']]=o
 def captured(*a,sep=' ',end='\n',**k):emit({'type':'log','text':sep.join(map(str,a))+end.rstrip('\n')})
 env['print']=captured;body='\n'.join('    '+x for x in d['program'].splitlines()) or '    pass';exec(compile('async def __dsh_main__():\n'+body+'\n','<program>','exec'),env,env);v=asyncio.run(env['__dsh_main__']());emit({'type':'done','value':v} if valid(v) else {'type':'done','error':{'kind':'invalid-output','message':'program completion must be lossless JSON'}})
except Exception as e:emit({'type':'done','error':{'kind':'exception','message':str(e)}})"#;

//! Bounded, workspace-confined LSP requests over owned stdio JSON-RPC.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex as AsyncMutex,
    time,
};
use url::Url;

use crate::TessivumError;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_MAX_RESULT_BYTES: usize = 1_048_576;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const DISPOSE_GRACE: Duration = Duration::from_millis(500);

/// Stable key for the language-server capability.
pub fn lsp_service_key() -> ServiceKey {
    ServiceKey::new("harness.lsp", "1")
}

/// A location expressed in the UTF-16 code units mandated by LSP.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

impl LspPosition {
    /// Converts a byte offset in one UTF-8 source line into an LSP UTF-16 column.
    pub fn from_utf8(line: u32, text: &str, byte_column: usize) -> Result<Self, TessivumError> {
        if byte_column > text.len() || !text.is_char_boundary(byte_column) {
            return Err(lsp_error(
                "INVALID_LSP_POSITION",
                "LSP position must be a UTF-8 character boundary within its line",
                json!({"byteColumn": byte_column}),
            ));
        }
        let character = text[..byte_column]
            .encode_utf16()
            .count()
            .try_into()
            .map_err(|_| {
                lsp_error(
                    "INVALID_LSP_POSITION",
                    "LSP UTF-16 column exceeds the protocol range",
                    Value::Null,
                )
            })?;
        Ok(Self { line, character })
    }
}

/// The closed set of LSP queries exposed by this capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LspOperation {
    Definition,
    Hover,
    References { include_declaration: bool },
}

impl LspOperation {
    fn method(&self) -> &'static str {
        match self {
            Self::Definition => "textDocument/definition",
            Self::Hover => "textDocument/hover",
            Self::References { .. } => "textDocument/references",
        }
    }

    fn parameters(&self, uri: &Url, position: LspPosition) -> Value {
        let mut parameters = json!({
            "textDocument": {"uri": uri.as_str()},
            "position": position,
        });
        if let Self::References {
            include_declaration,
        } = self
        {
            parameters["context"] = json!({"includeDeclaration": include_declaration});
        }
        parameters
    }
}

/// One workspace document query. There is intentionally no raw-method escape hatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspRequest {
    pub path: PathBuf,
    pub position: LspPosition,
    pub operation: LspOperation,
}

impl LspRequest {
    pub fn definition(path: impl Into<PathBuf>, position: LspPosition) -> Self {
        Self {
            path: path.into(),
            position,
            operation: LspOperation::Definition,
        }
    }

    pub fn hover(path: impl Into<PathBuf>, position: LspPosition) -> Self {
        Self {
            path: path.into(),
            position,
            operation: LspOperation::Hover,
        }
    }

    pub fn references(
        path: impl Into<PathBuf>,
        position: LspPosition,
        include_declaration: bool,
    ) -> Self {
        Self {
            path: path.into(),
            position,
            operation: LspOperation::References {
                include_declaration,
            },
        }
    }
}

/// Per-request resource ceilings enforced by the registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LspLimits {
    pub request_timeout: Duration,
    pub max_result_bytes: usize,
}

impl Default for LspLimits {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
        }
    }
}

impl LspLimits {
    fn validate(&self) -> Result<(), TessivumError> {
        if self.request_timeout.is_zero() || self.max_result_bytes < 4 {
            return Err(lsp_error(
                "INVALID_LSP_LIMITS",
                "LSP request timeout must be positive and result limit must be at least four bytes",
                Value::Null,
            ));
        }
        Ok(())
    }
}

/// A provider receives only closed LSP operations and owns its own transport lifecycle.
#[async_trait]
pub trait LspProvider: Send + Sync {
    async fn request(
        &self,
        request: LspRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError>;

    async fn dispose(&self) -> Result<(), TessivumError> {
        Ok(())
    }
}

struct ProviderSlot {
    id: u64,
    source: Arc<dyn LspProvider>,
    extensions: BTreeSet<String>,
}

#[derive(Default)]
struct LspState {
    next_id: u64,
    providers: BTreeMap<String, ProviderSlot>,
    extensions: BTreeMap<String, String>,
}

struct LspInner {
    state: Mutex<LspState>,
    limits: LspLimits,
    closed: AtomicBool,
}

/// Registry whose extensions are atomically and exclusively owned by one provider.
#[derive(Clone)]
pub struct LspRuntime {
    inner: Arc<LspInner>,
}

impl Default for LspRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for LspRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.inner.state);
        formatter
            .debug_struct("LspRuntime")
            .field("providers", &state.providers.keys().collect::<Vec<_>>())
            .field("closed", &self.inner.closed.load(Ordering::Acquire))
            .finish()
    }
}

impl LspRuntime {
    pub fn new() -> Self {
        Self::with_limits(LspLimits::default()).expect("default LSP limits are valid")
    }

    pub fn with_limits(limits: LspLimits) -> Result<Self, TessivumError> {
        limits.validate()?;
        Ok(Self {
            inner: Arc::new(LspInner {
                state: Mutex::new(LspState::default()),
                limits,
                closed: AtomicBool::new(false),
            }),
        })
    }

    pub fn publish(&self, context: &ContextHandle) -> Result<ServiceHandle<LspRuntime>, CoreError> {
        context.provide(lsp_service_key(), self.clone())
    }

    /// Atomically admits every requested extension, or none of them.
    pub fn register(
        &self,
        provider: impl Into<String>,
        extensions: impl IntoIterator<Item = impl Into<String>>,
        source: Arc<dyn LspProvider>,
    ) -> Result<LspProviderRegistration, TessivumError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(lsp_closed());
        }
        let provider = provider.into();
        if provider.trim().is_empty() {
            return Err(lsp_error(
                "INVALID_LSP_PROVIDER",
                "LSP provider name must not be blank",
                Value::Null,
            ));
        }
        let extensions = extensions
            .into_iter()
            .map(Into::into)
            .map(|extension| normalize_extension(&extension))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if extensions.is_empty() {
            return Err(lsp_error(
                "INVALID_LSP_EXTENSION",
                "LSP provider must support at least one extension",
                Value::Null,
            ));
        }

        let id = {
            let mut state = lock(&self.inner.state);
            if self.inner.closed.load(Ordering::Acquire) {
                return Err(lsp_closed());
            }
            if state.providers.contains_key(&provider) {
                return Err(lsp_error(
                    "DUPLICATE_LSP_PROVIDER",
                    "an LSP provider is already registered for this name",
                    json!({"provider": provider}),
                ));
            }
            if let Some(extension) = extensions
                .iter()
                .find(|extension| state.extensions.contains_key(*extension))
            {
                return Err(lsp_error(
                    "DUPLICATE_LSP_EXTENSION",
                    "an LSP extension is already owned by another provider",
                    json!({"extension": extension}),
                ));
            }
            state.next_id = state.next_id.wrapping_add(1);
            let id = state.next_id;
            for extension in &extensions {
                state.extensions.insert(extension.clone(), provider.clone());
            }
            state.providers.insert(
                provider.clone(),
                ProviderSlot {
                    id,
                    source,
                    extensions,
                },
            );
            id
        };
        Ok(LspProviderRegistration {
            inner: Arc::downgrade(&self.inner),
            provider,
            id,
            closed: AtomicBool::new(false),
        })
    }

    pub async fn definition(
        &self,
        path: impl Into<PathBuf>,
        position: LspPosition,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError> {
        self.request(LspRequest::definition(path, position), cancellation)
            .await
    }

    pub async fn request(
        &self,
        request: LspRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError> {
        check_cancelled(&cancellation)?;
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(lsp_closed());
        }
        let extension = request
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .ok_or_else(lsp_unavailable)?;
        let extension = normalize_extension(extension)?;
        let source = {
            let state = lock(&self.inner.state);
            let provider = state
                .extensions
                .get(&extension)
                .ok_or_else(lsp_unavailable)?;
            state
                .providers
                .get(provider)
                .map(|slot| Arc::clone(&slot.source))
                .ok_or_else(lsp_unavailable)?
        };

        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(lsp_cancelled()),
            result = time::timeout(self.inner.limits.request_timeout, source.request(request, cancellation.clone())) => {
                result.map_err(|_| lsp_error(
                    "LSP_TIMEOUT",
                    "LSP request exceeded its configured time limit",
                    json!({"timeoutMs": self.inner.limits.request_timeout.as_millis()}),
                ))??
            }
        };
        check_cancelled(&cancellation)?;
        ensure_result_bound(&response, self.inner.limits.max_result_bytes)?;
        Ok(response)
    }

    /// Closes the registry before disposing every admitted provider exactly once.
    pub async fn dispose(&self) -> Result<(), TessivumError> {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let providers = {
            let mut state = lock(&self.inner.state);
            state.extensions.clear();
            std::mem::take(&mut state.providers)
                .into_values()
                .map(|slot| slot.source)
                .collect::<Vec<_>>()
        };
        for provider in providers {
            provider.dispose().await?;
        }
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }
}

/// Lifetime owner for an admitted LSP provider.
pub struct LspProviderRegistration {
    inner: Weak<LspInner>,
    provider: String,
    id: u64,
    closed: AtomicBool,
}

impl fmt::Debug for LspProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LspProviderRegistration")
            .field("provider", &self.provider)
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish()
    }
}

impl LspProviderRegistration {
    /// Atomically removes this registration and releases all its exclusive extensions.
    pub fn close(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let mut state = lock(&inner.state);
        let Some(slot) = state.providers.get(&self.provider) else {
            return false;
        };
        if slot.id != self.id {
            return false;
        }
        let extensions = slot.extensions.clone();
        state.providers.remove(&self.provider);
        for extension in extensions {
            state.extensions.remove(&extension);
        }
        true
    }

    pub async fn dispose(&self) -> Result<bool, TessivumError> {
        let Some(inner) = self.inner.upgrade() else {
            return Ok(false);
        };
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(false);
        }
        let source = {
            let mut state = lock(&inner.state);
            let Some(slot) = state.providers.get(&self.provider) else {
                return Ok(false);
            };
            if slot.id != self.id {
                return Ok(false);
            }
            let slot = state
                .providers
                .remove(&self.provider)
                .expect("provider checked above");
            for extension in &slot.extensions {
                state.extensions.remove(extension);
            }
            slot.source
        };
        source.dispose().await?;
        Ok(true)
    }

    pub fn is_active(&self) -> bool {
        self.inner.upgrade().is_some_and(|inner| {
            lock(&inner.state)
                .providers
                .get(&self.provider)
                .is_some_and(|slot| slot.id == self.id)
        })
    }
}

impl Drop for LspProviderRegistration {
    fn drop(&mut self) {
        self.close();
    }
}

/// Configuration for one owned stdio language-server process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StdioLspConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub workspace: PathBuf,
    pub request_timeout: Duration,
    pub max_result_bytes: usize,
}

impl StdioLspConfig {
    pub fn new(program: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            workspace: workspace.into(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
        }
    }

    fn validate(&mut self) -> Result<(), TessivumError> {
        if self.program.as_os_str().is_empty()
            || self.request_timeout.is_zero()
            || self.max_result_bytes < 4
        {
            return Err(lsp_error(
                "INVALID_LSP_CONFIGURATION",
                "LSP program, positive timeout, and result limit of at least four bytes are required",
                Value::Null,
            ));
        }
        self.workspace = std::fs::canonicalize(&self.workspace).map_err(|error| {
            lsp_error(
                "INVALID_LSP_WORKSPACE",
                "LSP workspace must be an existing directory",
                json!({"error": error.to_string()}),
            )
        })?;
        if !self.workspace.is_dir() {
            return Err(lsp_error(
                "INVALID_LSP_WORKSPACE",
                "LSP workspace must be a directory",
                Value::Null,
            ));
        }
        Ok(())
    }
}

struct StdioSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

struct StdioInner {
    config: StdioLspConfig,
    session: AsyncMutex<Option<StdioSession>>,
    closed: AtomicBool,
}

/// An owned LSP server process. It is initialized before it becomes usable.
#[derive(Clone)]
pub struct StdioLspProvider {
    inner: Arc<StdioInner>,
}

impl fmt::Debug for StdioLspProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StdioLspProvider")
            .field("workspace", &self.inner.config.workspace)
            .field("closed", &self.inner.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl StdioLspProvider {
    /// Spawns and initializes a server. Initialization failures reap the owned child.
    pub async fn spawn(mut config: StdioLspConfig) -> Result<Self, TessivumError> {
        config.validate()?;
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .current_dir(&config.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            lsp_error(
                "LSP_UNAVAILABLE",
                "could not start the configured language server",
                json!({"error": error.to_string()}),
            )
        })?;
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill().await;
            return Err(lsp_error(
                "LSP_UNAVAILABLE",
                "language server did not provide stdin",
                Value::Null,
            ));
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill().await;
            return Err(lsp_error(
                "LSP_UNAVAILABLE",
                "language server did not provide stdout",
                Value::Null,
            ));
        };
        let provider = Self {
            inner: Arc::new(StdioInner {
                config,
                session: AsyncMutex::new(Some(StdioSession {
                    child,
                    stdin,
                    stdout: BufReader::new(stdout),
                    next_id: 1,
                })),
                closed: AtomicBool::new(false),
            }),
        };
        if let Err(error) = provider.initialize().await {
            let _ = provider.dispose().await;
            return Err(error);
        }
        Ok(provider)
    }

    pub fn workspace(&self) -> &Path {
        &self.inner.config.workspace
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    async fn initialize(&self) -> Result<(), TessivumError> {
        let root_uri = workspace_uri(&self.inner.config.workspace)?;
        let result = self
            .rpc(
                "initialize",
                json!({
                    "processId": Value::Null,
                    "clientInfo": {"name": "tessivum", "version": "1"},
                    "rootUri": root_uri.as_str(),
                    "workspaceFolders": [{"uri": root_uri.as_str(), "name": "workspace"}],
                    "capabilities": {"general": {"positionEncodings": ["utf-16"]}},
                }),
                ContextHandle::root().scope().cancellation(),
            )
            .await?;
        if result
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("positionEncoding"))
            .and_then(Value::as_str)
            .is_some_and(|encoding| !encoding.eq_ignore_ascii_case("utf-16"))
        {
            return Err(lsp_error(
                "LSP_UNSUPPORTED_POSITION_ENCODING",
                "language server did not accept UTF-16 positions",
                Value::Null,
            ));
        }
        self.notification("initialized", json!({})).await
    }

    async fn notification(&self, method: &str, params: Value) -> Result<(), TessivumError> {
        let mut guard = self.session().await?;
        let session = guard.as_mut().expect("session checked before use");
        send_frame(
            &mut session.stdin,
            &json!({"jsonrpc": "2.0", "method": method, "params": params}),
        )
        .await
    }

    async fn rpc(
        &self,
        method: &str,
        params: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError> {
        check_cancelled(&cancellation)?;
        let response = {
            let mut guard = self.session().await?;
            let session = guard.as_mut().expect("session checked before use");
            check_cancelled(&cancellation)?;
            let id = session.next_id;
            session.next_id = session.next_id.wrapping_add(1);
            send_frame(
                &mut session.stdin,
                &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            )
            .await?;
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(lsp_cancelled()),
                result = time::timeout(
                    self.inner.config.request_timeout,
                    read_response(&mut session.stdout, id, self.inner.config.max_result_bytes),
                ) => result.map_err(|_| lsp_error(
                    "LSP_TIMEOUT",
                    "language server response exceeded its configured time limit",
                    json!({"timeoutMs": self.inner.config.request_timeout.as_millis()}),
                ))?,
            }
        };
        if response.is_err() {
            let _ = self.dispose().await;
        }
        let response = response?;
        ensure_result_bound(&response, self.inner.config.max_result_bytes)?;
        Ok(response)
    }

    async fn session(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<StdioSession>>, TessivumError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(lsp_closed());
        }
        let session = self.inner.session.lock().await;
        if self.inner.closed.load(Ordering::Acquire) || session.is_none() {
            return Err(lsp_closed());
        }
        Ok(session)
    }

    fn document_uri(&self, path: &Path) -> Result<Url, TessivumError> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.inner.config.workspace.join(path)
        };
        let path = std::fs::canonicalize(&path).map_err(|error| {
            lsp_error(
                "LSP_WORKSPACE_DENIED",
                "LSP document must resolve inside the configured workspace",
                json!({"error": error.to_string()}),
            )
        })?;
        if !path.is_file() || !path.starts_with(&self.inner.config.workspace) {
            return Err(lsp_error(
                "LSP_WORKSPACE_DENIED",
                "LSP document must be a regular file inside the configured workspace",
                Value::Null,
            ));
        }
        Url::from_file_path(path).map_err(|_| {
            lsp_error(
                "LSP_WORKSPACE_DENIED",
                "LSP document cannot be represented by a file URI",
                Value::Null,
            )
        })
    }
}

#[async_trait]
impl LspProvider for StdioLspProvider {
    async fn request(
        &self,
        request: LspRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError> {
        let uri = self.document_uri(&request.path)?;
        self.rpc(
            request.operation.method(),
            request.operation.parameters(&uri, request.position),
            cancellation,
        )
        .await
    }

    async fn dispose(&self) -> Result<(), TessivumError> {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let session = self.inner.session.lock().await.take();
        if let Some(mut session) = session {
            let _ = graceful_shutdown(&mut session, self.inner.config.max_result_bytes).await;
            let _ = time::timeout(DISPOSE_GRACE, session.child.wait()).await;
            if session.child.try_wait().ok().flatten().is_none() {
                let _ = session.child.kill().await;
                let _ = session.child.wait().await;
            }
        }
        Ok(())
    }
}

async fn graceful_shutdown(
    session: &mut StdioSession,
    max_result_bytes: usize,
) -> Result<(), TessivumError> {
    let id = session.next_id;
    session.next_id = session.next_id.wrapping_add(1);
    send_frame(
        &mut session.stdin,
        &json!({"jsonrpc": "2.0", "id": id, "method": "shutdown", "params": {}}),
    )
    .await?;
    let _ = time::timeout(
        DISPOSE_GRACE,
        read_response(&mut session.stdout, id, max_result_bytes),
    )
    .await;
    send_frame(
        &mut session.stdin,
        &json!({"jsonrpc": "2.0", "method": "exit", "params": {}}),
    )
    .await
}

async fn send_frame(stdin: &mut ChildStdin, value: &Value) -> Result<(), TessivumError> {
    let body = serde_json::to_vec(value).map_err(|error| {
        lsp_error(
            "LSP_PROTOCOL_ERROR",
            "could not encode LSP JSON-RPC request",
            json!({"error": error.to_string()}),
        )
    })?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin
        .write_all(header.as_bytes())
        .await
        .map_err(lsp_unavailable_with)?;
    stdin.write_all(&body).await.map_err(lsp_unavailable_with)?;
    stdin.flush().await.map_err(lsp_unavailable_with)
}

async fn read_response(
    stdout: &mut BufReader<ChildStdout>,
    expected_id: u64,
    max_result_bytes: usize,
) -> Result<Value, TessivumError> {
    loop {
        let frame = read_frame(stdout, max_result_bytes).await?;
        if frame.get("id").is_none() && frame.get("method").is_some() {
            continue;
        }
        if frame.get("id").and_then(Value::as_u64) != Some(expected_id) {
            return Err(lsp_error(
                "LSP_PROTOCOL_ERROR",
                "language server returned an unexpected JSON-RPC response id",
                Value::Null,
            ));
        }
        if let Some(error) = frame.get("error") {
            return Err(lsp_error(
                "LSP_REQUEST_FAILED",
                "language server rejected the request",
                json!({"server": error}),
            ));
        }
        return frame.get("result").cloned().ok_or_else(|| {
            lsp_error(
                "LSP_PROTOCOL_ERROR",
                "language server response did not contain a result",
                Value::Null,
            )
        });
    }
}

async fn read_frame(
    stdout: &mut BufReader<ChildStdout>,
    max_result_bytes: usize,
) -> Result<Value, TessivumError> {
    let mut content_length = None;
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        let count = stdout
            .read_line(&mut line)
            .await
            .map_err(lsp_unavailable_with)?;
        if count == 0 {
            return Err(lsp_error(
                "LSP_UNAVAILABLE",
                "language server exited before returning a response",
                Value::Null,
            ));
        }
        header_bytes = header_bytes.saturating_add(count);
        if header_bytes > MAX_HEADER_BYTES {
            return Err(lsp_error(
                "LSP_PROTOCOL_ERROR",
                "language server sent an oversized JSON-RPC header",
                Value::Null,
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let Some((name, value)) = line.trim_end().split_once(':') else {
            return Err(lsp_error(
                "LSP_PROTOCOL_ERROR",
                "language server sent a malformed JSON-RPC header",
                Value::Null,
            ));
        };
        if name.eq_ignore_ascii_case("content-length") {
            let length = value.trim().parse::<usize>().map_err(|_| {
                lsp_error(
                    "LSP_PROTOCOL_ERROR",
                    "language server sent an invalid JSON-RPC content length",
                    Value::Null,
                )
            })?;
            if content_length.replace(length).is_some() {
                return Err(lsp_error(
                    "LSP_PROTOCOL_ERROR",
                    "language server sent duplicate JSON-RPC content lengths",
                    Value::Null,
                ));
            }
        }
    }
    let length = content_length.ok_or_else(|| {
        lsp_error(
            "LSP_PROTOCOL_ERROR",
            "language server response did not contain a content length",
            Value::Null,
        )
    })?;
    if length > max_result_bytes {
        return Err(lsp_error(
            "LSP_RESULT_LIMIT",
            "language server response exceeds its configured result limit",
            json!({"maxBytes": max_result_bytes}),
        ));
    }
    let mut body = vec![0; length];
    stdout
        .read_exact(&mut body)
        .await
        .map_err(lsp_unavailable_with)?;
    serde_json::from_slice(&body).map_err(|error| {
        lsp_error(
            "LSP_PROTOCOL_ERROR",
            "language server returned invalid JSON-RPC JSON",
            json!({"error": error.to_string()}),
        )
    })
}

fn workspace_uri(workspace: &Path) -> Result<Url, TessivumError> {
    Url::from_directory_path(workspace).map_err(|_| {
        lsp_error(
            "INVALID_LSP_WORKSPACE",
            "LSP workspace cannot be represented by a file URI",
            Value::Null,
        )
    })
}

fn normalize_extension(extension: &str) -> Result<String, TessivumError> {
    let extension = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if extension.is_empty()
        || !extension
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(lsp_error(
            "INVALID_LSP_EXTENSION",
            "LSP extension must be a non-empty alphanumeric identifier",
            json!({"extension": extension}),
        ));
    }
    Ok(extension)
}

fn ensure_result_bound(value: &Value, max_bytes: usize) -> Result<(), TessivumError> {
    let size = serde_json::to_vec(value)
        .expect("JSON values always serialize")
        .len();
    if size > max_bytes {
        Err(lsp_error(
            "LSP_RESULT_LIMIT",
            "LSP response exceeds its configured result limit",
            json!({"maxBytes": max_bytes}),
        ))
    } else {
        Ok(())
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), TessivumError> {
    if cancellation.is_cancelled() {
        Err(lsp_cancelled())
    } else {
        Ok(())
    }
}

fn lsp_cancelled() -> TessivumError {
    lsp_error("LSP_CANCELLED", "LSP request was cancelled", Value::Null)
}

fn lsp_closed() -> TessivumError {
    lsp_error("LSP_CLOSED", "LSP capability is closed", Value::Null)
}

fn lsp_unavailable() -> TessivumError {
    lsp_error(
        "LSP_UNAVAILABLE",
        "no LSP provider is available for this document",
        Value::Null,
    )
}

fn lsp_unavailable_with(error: std::io::Error) -> TessivumError {
    lsp_error(
        "LSP_UNAVAILABLE",
        "language server transport is unavailable",
        json!({"error": error.to_string()}),
    )
}

fn lsp_error(code: impl Into<String>, message: impl Into<String>, details: Value) -> TessivumError {
    TessivumError::new(code, message, "lsp", details)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::{self, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use tokio::{sync::Mutex as AsyncMutex, time};

use crate::{
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolRegistration, ToolRunContext,
        ToolRuntime,
    },
    ContentBlock, TessivumError,
};

/// Stable capability key for supervised Model Context Protocol connections.
pub fn mcp_service_key() -> ServiceKey {
    ServiceKey::new("harness.mcp", "1")
}

/// Connection-wide bounds and reconnect policy for one MCP server.
#[derive(Clone, Debug)]
pub struct McpClientConfig {
    pub server_name: String,
    pub timeout: Duration,
    pub reconnect: McpReconnectPolicy,
    pub max_decoded_json_bytes: usize,
    pub max_pages: usize,
    pub max_tools_per_page: usize,
    pub max_total_tools: usize,
    pub max_cursor_bytes: usize,
    pub max_name_bytes: usize,
    pub max_description_bytes: usize,
    pub max_schema_bytes: usize,
    pub max_args_bytes: usize,
    pub max_result_bytes: usize,
}

impl McpClientConfig {
    pub fn new(server_name: impl Into<String>) -> Result<Self, TessivumError> {
        let config = Self {
            server_name: server_name.into(),
            timeout: Duration::from_secs(60),
            reconnect: McpReconnectPolicy::default(),
            max_decoded_json_bytes: 1_048_576,
            max_pages: 1_024,
            max_tools_per_page: 128,
            max_total_tools: 1_024,
            max_cursor_bytes: 1_024,
            max_name_bytes: 256,
            max_description_bytes: 16_384,
            max_schema_bytes: 65_536,
            max_args_bytes: 1_048_576,
            max_result_bytes: 1_048_576,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), TessivumError> {
        let valid_name = !self.server_name.is_empty()
            && self.server_name.len() <= 32
            && self
                .server_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
        if !valid_name {
            return Err(mcp_error(
                "INVALID_MCP_SERVER_NAME",
                "MCP server names must be 1–32 ASCII letters, digits, '_' or '-'",
                json!({"serverName": self.server_name}),
            ));
        }
        if self.timeout.is_zero() {
            return Err(mcp_error(
                "INVALID_MCP_TIMEOUT",
                "MCP timeout must be positive",
                Value::Null,
            ));
        }
        if self.max_decoded_json_bytes == 0
            || self.max_pages == 0
            || self.max_tools_per_page == 0
            || self.max_total_tools == 0
            || self.max_tools_per_page > self.max_total_tools
            || self.max_cursor_bytes == 0
            || self.max_name_bytes == 0
            || self.max_description_bytes == 0
            || self.max_schema_bytes == 0
            || self.max_args_bytes == 0
            || self.max_result_bytes == 0
        {
            return Err(mcp_error(
                "INVALID_MCP_CONFIG",
                "MCP limits must be positive and tools per page cannot exceed total tools",
                Value::Null,
            ));
        }
        self.reconnect.validate()
    }
}

/// Bounded exponential reconnect policy. Reconnect attempts exclude initial startup.
#[derive(Clone, Debug)]
pub struct McpReconnectPolicy {
    pub enabled: bool,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub stability_window: Duration,
    pub max_attempts: u32,
}

impl Default for McpReconnectPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            initial_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            stability_window: Duration::from_secs(30),
            max_attempts: 10,
        }
    }
}

impl McpReconnectPolicy {
    fn validate(&self) -> Result<(), TessivumError> {
        if self.initial_delay.is_zero()
            || self.max_delay.is_zero()
            || self.stability_window.is_zero()
            || self.initial_delay > self.max_delay
            || self.max_attempts == 0
        {
            return Err(mcp_error(
                "INVALID_MCP_RECONNECT_POLICY",
                "MCP reconnect delays must be positive, ordered, and have at least one attempt",
                Value::Null,
            ));
        }
        Ok(())
    }
}

/// One page from MCP `tools/list`. Transport implementations must return decoded JSON only.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolPage {
    #[serde(default)]
    pub tools: Vec<McpTool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// One remote MCP tool, retaining its raw wire name privately in the bridge.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub task_support: McpTaskSupport,
}

/// Whether a remote tool requires MCP task support, which this synchronous bridge rejects.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpTaskSupport {
    Forbidden,
    #[default]
    Optional,
    Required,
}

/// A fresh transport factory. A reconnect always obtains a new transport generation.
#[async_trait]
pub trait McpConnector: Send + Sync {
    async fn connect(
        &self,
        config: &McpClientConfig,
    ) -> Result<Arc<dyn McpTransport>, TessivumError>;
}

/// Decoded MCP transport seam. Implementations own their protocol framing and process lifecycle.
#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn list_tools(
        &self,
        cursor: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<McpToolPage, TessivumError>;

    async fn call_tool(
        &self,
        raw_name: &str,
        arguments: Value,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError>;

    /// Resolves when this generation has closed. The default keeps simple request-only seams alive.
    async fn wait_closed(&self) -> Result<(), TessivumError> {
        std::future::pending::<Result<(), TessivumError>>().await
    }

    /// Makes best effort to stop this transport generation.
    async fn close(&self) -> Result<(), TessivumError>;
}

struct McpState {
    generation: u64,
    transport: Option<Arc<dyn McpTransport>>,
    registrations: Vec<ToolRegistration>,
    raw_names: BTreeMap<String, String>,
    reconnect_attempts: u32,
    connected_at: Option<Instant>,
    last_error: Option<TessivumError>,
}

struct McpInner {
    config: McpClientConfig,
    tools: ToolRuntime,
    connector: Arc<dyn McpConnector>,
    state: Mutex<McpState>,
    lifecycle: AsyncMutex<()>,
    registration_gate: Mutex<()>,
    cancellation: CancellationToken,
    disposed: AtomicBool,
}

/// A supervised MCP connection which owns exactly the tools installed for its server.
#[derive(Clone)]
pub struct McpConnection {
    inner: Arc<McpInner>,
}

impl fmt::Debug for McpConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.inner.state);
        formatter
            .debug_struct("McpConnection")
            .field("server_name", &self.inner.config.server_name)
            .field("generation", &state.generation)
            .field("ready", &state.transport.is_some())
            .field("tools", &state.raw_names.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl McpConnection {
    /// Connects, synchronizes its first tool snapshot, and starts close supervision.
    pub async fn connect(
        config: McpClientConfig,
        tools: ToolRuntime,
        connector: Arc<dyn McpConnector>,
    ) -> Result<Self, TessivumError> {
        config.validate()?;
        let connection = Self {
            inner: Arc::new(McpInner {
                config,
                tools,
                connector,
                state: Mutex::new(McpState {
                    generation: 0,
                    transport: None,
                    registrations: Vec::new(),
                    raw_names: BTreeMap::new(),
                    reconnect_attempts: 0,
                    connected_at: None,
                    last_error: None,
                }),
                lifecycle: AsyncMutex::new(()),
                registration_gate: Mutex::new(()),
                cancellation: ContextHandle::root().scope().cancellation(),
                disposed: AtomicBool::new(false),
            }),
        };
        connection.reconnect().await?;
        Ok(connection)
    }

    /// Publishes this connection under the stable MCP service key.
    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(mcp_service_key(), self)
    }

    /// Fails unless a current transport generation has completed its tool synchronization.
    pub async fn ready(&self) -> Result<(), TessivumError> {
        if self.inner.disposed.load(Ordering::Acquire) {
            return Err(disposed_error());
        }
        let state = lock(&self.inner.state);
        if state.transport.is_some() {
            return Ok(());
        }
        Err(state.last_error.clone().unwrap_or_else(|| {
            mcp_error(
                "MCP_NOT_READY",
                "MCP connection has no ready transport generation",
                Value::Null,
            )
        }))
    }

    /// Returns the model name's raw wire name without ever parsing model-controlled text.
    pub fn raw_name(&self, public_name: &str) -> Option<String> {
        lock(&self.inner.state).raw_names.get(public_name).cloned()
    }

    /// Rebuilds the complete remote tool snapshot and atomically swaps only this connection's tools.
    pub async fn sync_tools(&self) -> Result<(), TessivumError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.sync_current().await
    }

    /// Connects a fresh generation. Call this after a transport establishment failure.
    pub async fn reconnect(&self) -> Result<(), TessivumError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.connect_current(false).await
    }

    /// May be called by transports that expose close callbacks instead of [`McpTransport::wait_closed`].
    pub async fn closed(&self, generation: u64) {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if self.inner.disposed.load(Ordering::Acquire) {
            return;
        }
        let reconnect_enabled = self.inner.config.reconnect.enabled;
        {
            let mut state = lock(&self.inner.state);
            if state.generation != generation || state.transport.is_none() {
                return;
            }
            if state.connected_at.is_some_and(|connected| {
                connected.elapsed() >= self.inner.config.reconnect.stability_window
            }) {
                state.reconnect_attempts = 0;
            }
            state.transport = None;
        }
        self.remove_current_tools();
        if reconnect_enabled {
            let _ = self.connect_current(true).await;
        }
    }

    /// Cancels future work, removes every owned tool, and bounds transport close to five seconds.
    pub async fn dispose(&self) -> Result<(), TessivumError> {
        if self.inner.disposed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.inner.cancellation.cancel();
        self.remove_current_tools();
        let transport = {
            let mut state = lock(&self.inner.state);
            state.generation = next_generation(state.generation);
            state.connected_at = None;
            state.transport.take()
        };
        if let Some(transport) = transport {
            match time::timeout(Duration::from_secs(5), transport.close()).await {
                Ok(result) => result?,
                Err(_) => {
                    return Err(mcp_error(
                        "MCP_CLOSE_TIMEOUT",
                        "MCP transport did not close within five seconds",
                        Value::Null,
                    ))
                }
            }
        }
        Ok(())
    }

    async fn connect_current(&self, after_close: bool) -> Result<(), TessivumError> {
        if self.inner.disposed.load(Ordering::Acquire) {
            return Err(disposed_error());
        }
        let policy = &self.inner.config.reconnect;
        let start_attempt = if after_close {
            lock(&self.inner.state).reconnect_attempts
        } else {
            0
        };
        let end_attempt = if after_close { policy.max_attempts } else { 1 };
        let mut last_error = None;

        for attempt in start_attempt..end_attempt {
            if after_close {
                tokio::select! {
                    _ = self.inner.cancellation.cancelled() => return Err(disposed_error()),
                    _ = time::sleep(backoff(policy, attempt)) => {}
                }
            }
            if self.inner.disposed.load(Ordering::Acquire) {
                return Err(disposed_error());
            }
            let deadline = time::Instant::now() + self.inner.config.timeout;
            let transport = match tokio::select! {
                _ = self.inner.cancellation.cancelled() => return Err(disposed_error()),
                result = time::timeout_at(deadline, self.inner.connector.connect(&self.inner.config)) => match result {
                    Ok(result) => result,
                    Err(_) => Err(mcp_error("MCP_CONNECT_TIMEOUT", "MCP connector timed out", Value::Null)),
                },
            } {
                Ok(transport) => transport,
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };
            let installed = {
                let mut state = lock(&self.inner.state);
                if self.inner.disposed.load(Ordering::Acquire) {
                    None
                } else {
                    state.generation = next_generation(state.generation);
                    let previous = state.transport.replace(Arc::clone(&transport));
                    state.connected_at = Some(Instant::now());
                    state.last_error = None;
                    state.reconnect_attempts = if after_close { attempt + 1 } else { 0 };
                    Some((state.generation, previous))
                }
            };
            let Some((generation, previous)) = installed else {
                let _ = time::timeout(Duration::from_secs(5), transport.close()).await;
                return Err(disposed_error());
            };
            if let Some(previous) = previous {
                let _ = time::timeout(Duration::from_secs(5), previous.close()).await;
            }
            match self.sync_current().await {
                Ok(()) => {
                    self.monitor_close(generation, transport);
                    return Ok(());
                }
                Err(error) => {
                    self.remove_current_tools();
                    {
                        let mut state = lock(&self.inner.state);
                        if state.generation == generation
                            && state
                                .transport
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &transport))
                        {
                            state.transport = None;
                        }
                    }
                    let _ = time::timeout(Duration::from_secs(5), transport.close()).await;
                    last_error = Some(error);
                }
            }
        }

        let error = last_error.unwrap_or_else(|| {
            mcp_error(
                "MCP_RECONNECT_EXHAUSTED",
                "MCP reconnect attempts were exhausted",
                json!({"serverName": self.inner.config.server_name}),
            )
        });
        if !after_close {
            let mut state = lock(&self.inner.state);
            state.transport = None;
            state.last_error = Some(error.clone());
            return Err(error);
        }
        let exhausted = mcp_error(
            "MCP_RECONNECT_EXHAUSTED",
            "MCP reconnect attempts were exhausted",
            json!({"serverName": self.inner.config.server_name, "cause": error.code}),
        );
        let mut state = lock(&self.inner.state);
        state.transport = None;
        state.last_error = Some(exhausted.clone());
        Err(exhausted)
    }

    async fn sync_current(&self) -> Result<(), TessivumError> {
        if self.inner.disposed.load(Ordering::Acquire) {
            return Err(disposed_error());
        }
        let (generation, transport) = {
            let state = lock(&self.inner.state);
            (
                state.generation,
                state.transport.clone().ok_or_else(|| {
                    mcp_error(
                        "MCP_NOT_READY",
                        "MCP connection has no active transport",
                        Value::Null,
                    )
                })?,
            )
        };
        let tools =
            list_all_tools(&transport, &self.inner.config, &self.inner.cancellation).await?;
        if self.inner.disposed.load(Ordering::Acquire) {
            return Err(disposed_error());
        }
        let definitions = definitions_for(&self.inner, generation, &transport, &tools)?;
        let raw_names: BTreeMap<_, _> = tools
            .iter()
            .map(|tool| {
                (
                    public_tool_name(&self.inner.config.server_name, &tool.name),
                    tool.name.clone(),
                )
            })
            .collect();

        let _registration_gate = lock(&self.inner.registration_gate);
        let prior = {
            let mut state = lock(&self.inner.state);
            if self.inner.disposed.load(Ordering::Acquire) {
                return Err(disposed_error());
            }
            if state.generation != generation
                || !state
                    .transport
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &transport))
            {
                return Err(generation_replaced_error());
            }
            std::mem::take(&mut state.registrations)
        };
        let registrations = match self.inner.tools.replace(&prior, definitions) {
            Ok(registrations) => registrations,
            Err(error) => {
                let mut state = lock(&self.inner.state);
                if !self.inner.disposed.load(Ordering::Acquire)
                    && state.generation == generation
                    && state
                        .transport
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &transport))
                {
                    state.registrations = prior;
                }
                return Err(error);
            }
        };
        let stale = {
            let mut state = lock(&self.inner.state);
            if !self.inner.disposed.load(Ordering::Acquire)
                && state.generation == generation
                && state
                    .transport
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &transport))
            {
                state.registrations = registrations;
                state.raw_names = raw_names;
                None
            } else {
                Some(registrations)
            }
        };
        match stale {
            None => Ok(()),
            Some(registrations) => {
                let _ = self.inner.tools.replace(&registrations, Vec::new());
                if self.inner.disposed.load(Ordering::Acquire) {
                    Err(disposed_error())
                } else {
                    Err(generation_replaced_error())
                }
            }
        }
    }

    fn remove_current_tools(&self) {
        let _registration_gate = lock(&self.inner.registration_gate);
        let prior = {
            let mut state = lock(&self.inner.state);
            std::mem::take(&mut state.registrations)
        };
        match self.inner.tools.replace(&prior, Vec::new()) {
            Ok(registrations) => {
                let mut state = lock(&self.inner.state);
                state.registrations = registrations;
                state.raw_names.clear();
            }
            Err(error) => {
                let mut state = lock(&self.inner.state);
                state.registrations = prior;
                state.last_error = Some(error);
            }
        }
    }

    fn monitor_close(&self, generation: u64, transport: Arc<dyn McpTransport>) {
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let _ = transport.wait_closed().await;
            let Some(inner) = weak.upgrade() else {
                return;
            };
            McpConnection { inner }.closed(generation).await;
        });
    }

    async fn call_raw(
        &self,
        generation: u64,
        expected_transport: &Arc<dyn McpTransport>,
        raw_name: &str,
        arguments: Value,
        cancellation: CancellationToken,
        task_support: McpTaskSupport,
    ) -> ToolHandlerResult {
        if task_support == McpTaskSupport::Required {
            return Err(mcp_error(
                "MCP_TASK_REQUIRED",
                "the remote MCP tool requires task support that this bridge does not implement",
                json!({"name": raw_name}),
            ));
        }
        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        ensure_json_bound(
            &arguments,
            self.inner.config.max_args_bytes,
            "MCP_ARGUMENTS_LIMIT",
            "MCP tool arguments exceed the configured limit",
        )?;
        let transport = {
            let state = lock(&self.inner.state);
            if self.inner.disposed.load(Ordering::Acquire) {
                return Err(disposed_error());
            }
            if state.generation != generation
                || !state
                    .transport
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, expected_transport))
            {
                return Err(generation_replaced_error());
            }
            Arc::clone(expected_transport)
        };
        let deadline = time::Instant::now() + self.inner.config.timeout;
        let call = transport.call_tool(raw_name, arguments, cancellation.clone());
        let response = tokio::select! {
            _ = self.inner.cancellation.cancelled() => return Err(disposed_error()),
            _ = cancellation.cancelled() => return Err(cancelled_error()),
            result = time::timeout_at(deadline, call) => match result {
                Ok(result) => result?,
                Err(_) => return Err(mcp_error("MCP_TOOL_TIMEOUT", "MCP tool call timed out", json!({"name": raw_name}))),
            },
        };
        ensure_json_bound(
            &response,
            self.inner.config.max_result_bytes,
            "MCP_RESULT_LIMIT",
            "MCP tool result exceeds the configured limit",
        )?;
        normalize_result(response)
    }
}

struct McpToolHandler {
    connection: Weak<McpInner>,
    generation: u64,
    transport: Arc<dyn McpTransport>,
    raw_name: String,
    task_support: McpTaskSupport,
}

#[async_trait]
impl ToolHandler for McpToolHandler {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let Some(inner) = self.connection.upgrade() else {
            return Err(mcp_error(
                "MCP_DISPOSED",
                "MCP connection was disposed",
                Value::Null,
            ));
        };
        McpConnection { inner }
            .call_raw(
                self.generation,
                &self.transport,
                &self.raw_name,
                arguments,
                context.cancellation,
                self.task_support,
            )
            .await
    }
}

fn definitions_for(
    inner: &Arc<McpInner>,
    generation: u64,
    transport: &Arc<dyn McpTransport>,
    tools: &[McpTool],
) -> Result<Vec<ToolDefinition>, TessivumError> {
    let mut raw_names = BTreeSet::new();
    let mut public_names = BTreeSet::new();
    let mut definitions = Vec::with_capacity(tools.len());
    for tool in tools {
        if tool.name.trim().is_empty() {
            return Err(mcp_error(
                "INVALID_MCP_TOOL",
                "MCP tools need a non-empty name and object input schema",
                json!({"name": tool.name}),
            ));
        }
        let input_schema = normalize_input_schema(&tool.input_schema);
        if !raw_names.insert(tool.name.clone()) {
            return Err(mcp_error(
                "DUPLICATE_MCP_TOOL",
                "MCP tools/list returned a duplicate raw name",
                json!({"name": tool.name}),
            ));
        }
        let public_name = public_tool_name(&inner.config.server_name, &tool.name);
        if !public_names.insert(public_name.clone()) {
            return Err(mcp_error(
                "MCP_PUBLIC_NAME_COLLISION",
                "distinct MCP tool names mapped to one public name",
                json!({"name": tool.name}),
            ));
        }
        definitions.push(ToolDefinition::new(
            public_name,
            if tool.description.is_empty() {
                "MCP tool".to_owned()
            } else {
                tool.description.clone()
            },
            input_schema,
            McpToolHandler {
                connection: Arc::downgrade(inner),
                generation,
                transport: Arc::clone(transport),
                raw_name: tool.name.clone(),
                task_support: tool.task_support,
            },
        ));
    }
    Ok(definitions)
}
fn normalize_input_schema(input_schema: &Value) -> Value {
    let Value::Object(mut schema) = input_schema.clone() else {
        return json!({"type": "object", "properties": {}, "additionalProperties": true});
    };
    if schema.get("type").and_then(Value::as_str) == Some("object")
        && !schema.contains_key("properties")
    {
        schema.insert("properties".to_owned(), Value::Object(Default::default()));
    }
    Value::Object(schema)
}

async fn list_all_tools(
    transport: &Arc<dyn McpTransport>,
    config: &McpClientConfig,
    cancellation: &CancellationToken,
) -> Result<Vec<McpTool>, TessivumError> {
    let deadline = time::Instant::now() + config.timeout;
    let mut cursor = None;
    let mut cursors = BTreeSet::new();
    let mut tools = Vec::new();
    for _ in 0..config.max_pages {
        let page = tokio::select! {
            _ = cancellation.cancelled() => return Err(disposed_error()),
            result = time::timeout_at(deadline, transport.list_tools(cursor.clone(), cancellation.clone())) => match result {
                Ok(result) => result?,
                Err(_) => return Err(mcp_error("MCP_SYNC_TIMEOUT", "MCP tools synchronization timed out", Value::Null)),
            },
        };
        validate_page(&page, config)?;
        if page.tools.len() > config.max_total_tools.saturating_sub(tools.len()) {
            return Err(mcp_error(
                "MCP_TOTAL_TOOLS_LIMIT",
                "MCP tools/list exceeded the configured total tool limit",
                Value::Null,
            ));
        }
        tools.extend(page.tools);
        match page.next_cursor {
            None => return Ok(tools),
            Some(next) if next.is_empty() || !cursors.insert(next.clone()) => {
                return Err(mcp_error(
                    "INVALID_MCP_PAGINATION",
                    "MCP tools/list repeated or returned an empty cursor",
                    json!({"cursor": next}),
                ))
            }
            Some(next) => cursor = Some(next),
        }
    }
    Err(mcp_error(
        "MCP_PAGE_LIMIT",
        "MCP tools/list exceeded the configured page limit",
        Value::Null,
    ))
}

fn validate_page(page: &McpToolPage, config: &McpClientConfig) -> Result<(), TessivumError> {
    if page.tools.len() > config.max_tools_per_page {
        return Err(mcp_error(
            "MCP_TOOLS_PER_PAGE_LIMIT",
            "MCP tools/list page exceeds the configured tool limit",
            Value::Null,
        ));
    }
    ensure_json_bound(
        page,
        config.max_decoded_json_bytes,
        "MCP_JSON_LIMIT",
        "decoded MCP JSON exceeds the configured limit",
    )?;
    if page
        .next_cursor
        .as_ref()
        .is_some_and(|cursor| cursor.len() > config.max_cursor_bytes)
    {
        return Err(mcp_error(
            "MCP_CURSOR_LIMIT",
            "MCP pagination cursor exceeds the configured limit",
            Value::Null,
        ));
    }
    for tool in &page.tools {
        if tool.name.len() > config.max_name_bytes {
            return Err(mcp_error(
                "MCP_NAME_LIMIT",
                "MCP tool name exceeds the configured limit",
                Value::Null,
            ));
        }
        if tool.description.len() > config.max_description_bytes {
            return Err(mcp_error(
                "MCP_DESCRIPTION_LIMIT",
                "MCP tool description exceeds the configured limit",
                Value::Null,
            ));
        }
        ensure_json_bound(
            &tool.input_schema,
            config.max_schema_bytes,
            "MCP_SCHEMA_LIMIT",
            "MCP tool schema exceeds the configured limit",
        )?;
    }
    Ok(())
}

struct ByteLimitWriter {
    remaining: usize,
    exceeded: bool,
}

impl Write for ByteLimitWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            self.exceeded = true;
            return Err(io::Error::other("JSON byte limit exceeded"));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn ensure_json_bound<T: Serialize>(
    value: &T,
    maximum: usize,
    code: &str,
    message: &str,
) -> Result<(), TessivumError> {
    let mut writer = ByteLimitWriter {
        remaining: maximum,
        exceeded: false,
    };
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(()),
        Err(_) if writer.exceeded => Err(mcp_error(code, message, Value::Null)),
        Err(error) => Err(mcp_error(
            "MCP_JSON_SERIALIZATION",
            "MCP JSON could not be measured",
            json!({"error": error.to_string()}),
        )),
    }
}

/// Produces a stable, model-safe public tool name. Callers must retain the raw mapping instead of
/// attempting to reverse this lossy representation.
pub fn public_tool_name(server_name: &str, raw_name: &str) -> String {
    let prefix = format!("mcp__{}__", server_name);
    let sanitized: String = raw_name
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || byte == b'_' {
                char::from(byte)
            } else {
                '_'
            }
        })
        .collect();
    let changed = sanitized != raw_name || sanitized.is_empty();
    let hash = short_hash(raw_name);
    let suffix = if changed {
        format!("_{hash}")
    } else {
        String::new()
    };
    let maximum = 64usize.saturating_sub(prefix.len());
    let base_limit = maximum.saturating_sub(suffix.len()).max(1);
    let mut base: String = sanitized.chars().take(base_limit).collect();
    if base.is_empty() {
        base.push('_');
    }
    let needs_hash = changed || sanitized.len() > base.len();
    let suffix = if needs_hash {
        format!("_{hash}")
    } else {
        String::new()
    };
    let available = maximum.saturating_sub(suffix.len()).max(1);
    base.truncate(available);
    format!("{prefix}{base}{suffix}")
}

fn short_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn backoff(policy: &McpReconnectPolicy, attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX);
    policy
        .initial_delay
        .checked_mul(factor)
        .unwrap_or(policy.max_delay)
        .min(policy.max_delay)
}

fn next_generation(generation: u64) -> u64 {
    generation.wrapping_add(1).max(1)
}

fn normalize_result(value: Value) -> ToolHandlerResult {
    let object = value.as_object();
    if object
        .and_then(|object| object.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(mcp_error(
            "MCP_TOOL_ERROR",
            "remote MCP tool returned an error result",
            json!({"result": value}),
        ));
    }
    let content = match object.and_then(|object| object.get("content")) {
        Some(Value::Array(content)) => content.iter().map(content_block).collect(),
        Some(Value::String(text)) => vec![ContentBlock::Text { text: text.clone() }],
        Some(_) => vec![placeholder("malformed content")],
        None => vec![placeholder("no content")],
    };
    Ok(crate::tools::ToolOutput::new(content, false, Value::Null))
}

fn content_block(value: &Value) -> ContentBlock {
    let Some(object) = value.as_object() else {
        return placeholder("malformed content item");
    };
    match object.get("type").and_then(Value::as_str) {
        Some("text") => object
            .get("text")
            .and_then(Value::as_str)
            .map(|text| ContentBlock::Text {
                text: text.to_owned(),
            })
            .unwrap_or_else(|| placeholder("malformed text content")),
        Some("image") => ContentBlock::Image {
            attachment: value.clone(),
        },
        Some(kind) => placeholder(&format!("{kind} content omitted")),
        None => placeholder("untyped content"),
    }
}

fn placeholder(reason: &str) -> ContentBlock {
    ContentBlock::Text {
        text: format!("[MCP {reason}]"),
    }
}

fn mcp_error(code: impl Into<String>, message: impl Into<String>, details: Value) -> TessivumError {
    TessivumError::new(code, message, "mcp", details)
}

fn cancelled_error() -> TessivumError {
    mcp_error("CANCELLED", "MCP tool call was cancelled", Value::Null)
}

fn disposed_error() -> TessivumError {
    mcp_error("MCP_DISPOSED", "MCP connection was disposed", Value::Null)
}

fn generation_replaced_error() -> TessivumError {
    mcp_error(
        "MCP_GENERATION_REPLACED",
        "MCP transport generation was replaced",
        Value::Null,
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

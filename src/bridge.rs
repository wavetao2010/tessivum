//! Typed, bounded product-domain dispatch for legacy Node and WASM plugins.
//!
//! This module deliberately exposes only the product services listed in
//! [`DomainRequest`]; it does not expose a generic `ContextHandle` lookup.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Component, Path},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::stream;
use parking_lot::{Condvar, Mutex, MutexGuard};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::CancellationToken;
use tessivum_extism::{Capability, CapabilityHandler, CapabilityRequest, PluginError, WasmResult};
use tessivum_node_bridge::{
    BridgeClient, BridgeError, BridgeHandler, BridgeResult, Frame, FrameKind, RemoteError,
};
use tokio::{
    runtime::Handle,
    sync::{oneshot, Semaphore},
    task::JoinHandle,
};

use crate::{
    agent::{AgentAuthority, AgentRegistry, InboxTarget},
    credentials::{CredentialRef, Credentials},
    llm::{LlmAdapter, LlmProviderRegistration, LlmRuntime, LlmStream},
    plugins::ServiceMethodPermission,
    protocol::{GenerateRequest, Message, SessionEvent, SessionId, StreamChunk, ToolCallId},
    session::SessionStore,
    settings::Settings,
    system_prompt::{PromptRegistration, PromptSection, SystemPrompt},
    tools::{
        ToolDefinition, ToolHandler, ToolOutput, ToolRegistration, ToolRunContext, ToolRuntime,
    },
    TessivumError,
};

/// Stable domain service identifiers. Versioning is part of the wire contract.
pub const TOOLS_SERVICE: &str = "tools@1";
pub const SYSTEM_PROMPT_SERVICE: &str = "systemPrompt@1";
pub const LLM_SERVICE: &str = "llm@1";
pub const SESSIONS_SERVICE: &str = "sessions@1";
pub const AGENTS_SERVICE: &str = "agents@1";
pub const LOGGER_SERVICE: &str = "logger@1";
pub const TIMERS_SERVICE: &str = "timers@1";
pub const SETTINGS_SERVICE: &str = "settings@1";
pub const CREDENTIALS_SERVICE: &str = "credentials@1";
const MAX_WEB_ROUTES: usize = 256;
const MAX_WEB_HEADERS: usize = 64;
const MAX_WEB_HEADER_BYTES: usize = 16 * 1024;
const MAX_WEB_PATH_BYTES: usize = 8 * 1024;
const MAX_WEB_QUERY_BYTES: usize = 8 * 1024;
const MAX_PNPM_OUTPUT_CHUNK: usize = 64 * 1024;
const MAX_PNPM_RESULT_BYTES: usize = 512 * 1024;
pub const WEB_REQUEST_BODY_LIMIT: usize = 2 * 1024 * 1024;
pub const WEB_RESPONSE_BODY_LIMIT: usize = 8 * 1024 * 1024;
const WEB_ROUTE_TIMEOUT: Duration = Duration::from_secs(16 * 60);
const MAX_WEB_CALLBACK_BYTES: usize = 12 * 1024 * 1024;

/// Limits applied before decoding or sending a product-domain envelope.
#[derive(Clone, Debug)]
pub struct BridgeLimits {
    /// Largest Node/WASM service envelope accepted after transport framing.
    pub max_json_bytes: usize,
    /// Largest callback envelope sent from a native contribution to Node.
    pub max_callback_bytes: usize,
    /// Maximum concurrent native-to-Node callback requests across generations.
    pub max_callback_concurrency: usize,
    /// Maximum owned timer tasks per Node connection generation.
    pub max_timers_per_generation: usize,
    /// Deadline for one Node-to-native or WASM-to-native service call.
    pub request_timeout: Duration,
    /// Deadline used for all native-to-Node callback requests.
    pub callback_timeout: Duration,
}

impl Default for BridgeLimits {
    fn default() -> Self {
        Self {
            max_json_bytes: 256 * 1024,
            max_callback_bytes: 256 * 1024,
            max_callback_concurrency: 16,
            max_timers_per_generation: 64,
            request_timeout: Duration::from_secs(5),
            callback_timeout: Duration::from_secs(5),
        }
    }
}

impl BridgeLimits {
    fn validate(&self) -> BridgeResult<()> {
        if self.max_json_bytes == 0
            || self.max_callback_bytes == 0
            || self.max_callback_concurrency == 0
            || self.max_timers_per_generation == 0
            || self.request_timeout.is_zero()
            || self.callback_timeout.is_zero()
        {
            return Err(invalid("bridge limits must be greater than zero"));
        }
        Ok(())
    }
}

/// One typed service request. `params` belongs to the selected method only.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DomainRequest {
    pub service: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// The two bounded path matching modes accepted from the compatibility host.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebRouteKind {
    Exact,
    Prefix,
}

/// Node-owned route registration. Its identifier is scoped to one bridge generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebRouteRegistration {
    pub route_id: String,
    pub kind: WebRouteKind,
    pub path: String,
}

/// A generation-owned route removal request.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WebRouteRemove {
    route_id: String,
}
/// The bounded HTTP request delivered to a registered Node route.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebRouteRequest {
    pub route_id: String,
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body_base64: String,
}

/// The bounded HTTP response returned by a registered Node route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebRouteResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_base64: String,
}

/// A profile-owned package-operation boundary.
#[async_trait]
pub trait PnpmBoundary: Send + Sync {
    /// Runs only the prevalidated operation while observing cancellation and output bounds.
    async fn run(
        &self,
        request: PnpmRunRequest,
        cancellation: CancellationToken,
        output: PnpmOutputSink,
    ) -> BridgeResult<PnpmRunResult>;
}

/// The only live package-output streams exposed to the Node facade.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PnpmOutputStream {
    Stdout,
    Stderr,
}

/// A package operation requested by the Node desktop facade.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PnpmRunRequest {
    pub operation_id: String,
    pub args: Vec<String>,
    pub invoking_dir: String,
}

/// A bounded package-operation settlement consumed by the Node desktop facade.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PnpmRunResult {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub stdout: String,
    pub stderr: String,
}

/// A generation-owned live output sink with bounded transport frames.
#[derive(Clone)]
pub struct PnpmOutputSink {
    client: BridgeClient,
    operation_id: String,
    cancellation: CancellationToken,
}

impl PnpmOutputSink {
    /// Sends one bounded base64 output notification to the owning compatibility host.
    pub fn emit(&self, stream: PnpmOutputStream, chunk: &[u8]) -> BridgeResult<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        if chunk.len() > MAX_PNPM_OUTPUT_CHUNK {
            return Err(remote(
                "PNPM_OUTPUT_LIMIT",
                "pnpm output chunk exceeds 64 KiB",
            ));
        }
        if self.cancellation.is_cancelled() {
            return Err(BridgeError::Cancelled);
        }
        // The stream is not accumulated here: dshmarket retains its own rolling
        // stdout/stderr tails. A cumulative transport cap would abort valid
        // dependency-heavy installs once pnpm's NDJSON progress exceeds it.
        self.client.notify(
            FrameKind::PnpmOutput,
            json!({
                "operationId": self.operation_id,
                "stream": stream,
                "chunkBase64": encode_web_body(chunk),
            }),
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WasmServiceRequest {
    service: String,
    method: String,
    #[serde(default)]
    payload: Value,
}

/// Product authorization installed for one running WASM plugin instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmEffectivePolicy {
    pub plugin_id: String,
    pub instance_id: String,
    pub entry_id: String,
    pub methods: BTreeSet<ServiceMethodPermission>,
}

impl WasmEffectivePolicy {
    pub fn new(
        plugin_id: impl Into<String>,
        instance_id: impl Into<String>,
        entry_id: impl Into<String>,
        methods: impl IntoIterator<Item = ServiceMethodPermission>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            instance_id: instance_id.into(),
            entry_id: entry_id.into(),
            methods: methods.into_iter().collect(),
        }
    }
}

/// Cloneable authorization registry shared by WASM lifecycle owners and the bridge.
#[derive(Clone, Default)]
pub struct WasmPolicyRegistry {
    inner: Arc<WasmPolicyRegistryInner>,
}

#[derive(Default)]
struct WasmPolicyRegistryInner {
    maps: Mutex<WasmPolicyMaps>,
}

#[derive(Default)]
struct WasmPolicyMaps {
    policies: BTreeMap<String, Arc<WasmPolicyEntry>>,
    owners: BTreeMap<String, WasmPluginOwner>,
}

struct WasmPluginOwner {
    entry_id: String,
    instances: usize,
}

struct WasmPolicyEntry {
    state: Mutex<WasmPolicyState>,
    drained: Condvar,
}

struct WasmPolicyState {
    plugin_id: String,
    methods: BTreeSet<ServiceMethodPermission>,
    revoked: bool,
    active: usize,
}

/// The sole policy owner for one plugin instance. Dropping it revokes admission only.
pub struct WasmPolicyRegistration {
    plugin_id: String,
    instance_id: String,
    entry_id: String,
    entry: Arc<WasmPolicyEntry>,
    registry: Weak<WasmPolicyRegistryInner>,
}

/// An admitted service call. Its lifetime prevents policy drain from completing.
pub struct WasmPolicyLease {
    entry: Arc<WasmPolicyEntry>,
}

impl WasmPolicyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install(&self, policy: WasmEffectivePolicy) -> WasmResult<WasmPolicyRegistration> {
        validate_policy(&policy)?;
        let entry = Arc::new(WasmPolicyEntry {
            state: Mutex::new(WasmPolicyState {
                plugin_id: policy.plugin_id.clone(),
                methods: policy.methods,
                revoked: false,
                active: 0,
            }),
            drained: Condvar::new(),
        });
        let mut maps = lock(&self.inner.maps);
        if maps.policies.contains_key(&policy.instance_id) {
            return Err(policy_error(
                "PLUGIN_POLICY_ALREADY_REGISTERED",
                "a policy is already installed for this plugin instance",
            ));
        }
        if maps
            .owners
            .get(&policy.plugin_id)
            .is_some_and(|owner| owner.entry_id != policy.entry_id)
        {
            return Err(policy_error(
                "PLUGIN_POLICY_ALREADY_REGISTERED",
                "plugin id belongs to another loader entry",
            ));
        }
        let owner = maps
            .owners
            .entry(policy.plugin_id.clone())
            .or_insert_with(|| WasmPluginOwner {
                entry_id: policy.entry_id.clone(),
                instances: 0,
            });
        owner.instances = owner
            .instances
            .checked_add(1)
            .ok_or_else(|| policy_error("RESOURCE_LIMIT", "too many plugin instances"))?;
        maps.policies
            .insert(policy.instance_id.clone(), Arc::clone(&entry));
        Ok(WasmPolicyRegistration {
            plugin_id: policy.plugin_id,
            instance_id: policy.instance_id,
            entry_id: policy.entry_id,
            entry,
            registry: Arc::downgrade(&self.inner),
        })
    }

    /// Admits an exact service/method call and returns a guard held through dispatch.
    pub fn authorize(
        &self,
        instance_id: &str,
        plugin_id: &str,
        service: &str,
        method: &str,
    ) -> WasmResult<WasmPolicyLease> {
        let maps = lock(&self.inner.maps);
        let entry = maps.policies.get(instance_id).cloned().ok_or_else(|| {
            policy_error("PLUGIN_POLICY_NOT_FOUND", "plugin policy is not installed")
        })?;
        let mut state = lock(&entry.state);
        if state.revoked || state.plugin_id != plugin_id {
            return Err(policy_error(
                "PLUGIN_POLICY_NOT_FOUND",
                "plugin policy is not installed",
            ));
        }
        if !state.methods.contains(&ServiceMethodPermission {
            service: service.into(),
            method: method.into(),
        }) {
            return Err(policy_error(
                "SERVICE_PERMISSION_DENIED",
                "service method is not permitted",
            ));
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| policy_error("RESOURCE_LIMIT", "too many active service calls"))?;
        drop(state);
        drop(maps);
        Ok(WasmPolicyLease { entry })
    }

    pub fn active_instances(&self, plugin_id: &str) -> Vec<String> {
        let maps = lock(&self.inner.maps);
        maps.policies
            .iter()
            .filter_map(|(instance_id, entry)| {
                let state = lock(&entry.state);
                (!state.revoked && state.plugin_id == plugin_id).then(|| instance_id.clone())
            })
            .collect()
    }
}

impl WasmPolicyRegistration {
    /// Synchronously rejects future calls. Existing leases remain valid until dropped.
    pub fn revoke(&self) -> bool {
        let mut revoked = false;
        if let Some(registry) = self.registry.upgrade() {
            let mut maps = lock(&registry.maps);
            if maps
                .policies
                .get(&self.instance_id)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
            {
                let mut state = lock(&self.entry.state);
                state.revoked = true;
                maps.policies.remove(&self.instance_id);
                let remove_owner = if let Some(owner) = maps.owners.get_mut(&self.plugin_id) {
                    if owner.entry_id == self.entry_id {
                        owner.instances = owner.instances.saturating_sub(1);
                        owner.instances == 0
                    } else {
                        false
                    }
                } else {
                    false
                };
                if remove_owner {
                    maps.owners.remove(&self.plugin_id);
                }
                revoked = true;
            }
        }
        let mut state = lock(&self.entry.state);
        if !state.revoked {
            state.revoked = true;
            revoked = true;
        }
        revoked
    }

    /// Waits no longer than `timeout` for calls admitted before revocation to finish.
    pub fn drain(&self, timeout: Duration) -> WasmResult<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let mut state = lock(&self.entry.state);
        while state.active != 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if (remaining.is_zero()
                || self
                    .entry
                    .drained
                    .wait_for(&mut state, remaining)
                    .timed_out())
                && state.active != 0
            {
                return Err(policy_error(
                    "RESOURCE_LIMIT",
                    "service calls did not drain in time",
                ));
            }
        }
        Ok(())
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn is_revoked(&self) -> bool {
        lock(&self.entry.state).revoked
    }

    pub fn active_calls(&self) -> usize {
        lock(&self.entry.state).active
    }
}

impl Drop for WasmPolicyRegistration {
    fn drop(&mut self) {
        self.revoke();
    }
}

impl Drop for WasmPolicyLease {
    fn drop(&mut self) {
        let mut state = lock(&self.entry.state);
        debug_assert!(state.active != 0, "policy lease underflow");
        state.active = state.active.saturating_sub(1);
        if state.active == 0 {
            self.entry.drained.notify_all();
        }
    }
}

/// A level accepted by the logger proxy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Native logger sink. The bridge owns validation; implementations receive no untyped envelope.
pub trait DomainLogger: Send + Sync {
    fn log(&self, level: LogLevel, message: &str, fields: &Value);
}

/// Named, explicitly permitted native event sink. This avoids arbitrary Context events.
pub trait DomainEventSink: Send + Sync {
    fn emit(&self, event: &str, payload: &Value) -> Result<Value, TessivumError>;
}

#[derive(Default)]
struct NoopLogger;
impl DomainLogger for NoopLogger {
    fn log(&self, _: LogLevel, _: &str, _: &Value) {}
}

/// Product services attached to one bridge. Optional services are unavailable rather than inferred.
#[derive(Clone)]
pub struct BridgeServices {
    tools: ToolRuntime,
    system_prompt: SystemPrompt,
    llm: LlmRuntime,
    sessions: SessionStore,
    agents: AgentRegistry,
    owner: Option<AgentAuthority>,
    logger: Arc<dyn DomainLogger>,
    settings: Option<Arc<Settings>>,
    credentials: Option<Arc<Credentials>>,
    event_sink: Option<Arc<dyn DomainEventSink>>,
    permitted_events: Arc<BTreeSet<String>>,
    pnpm: Option<Arc<dyn PnpmBoundary>>,
}

impl BridgeServices {
    /// Creates the required product service set. Optional services start unavailable.
    pub fn new(
        tools: ToolRuntime,
        system_prompt: SystemPrompt,
        llm: LlmRuntime,
        sessions: SessionStore,
        agents: AgentRegistry,
    ) -> Self {
        Self {
            tools,
            system_prompt,
            llm,
            sessions,
            agents,
            owner: None,
            logger: Arc::new(NoopLogger),
            settings: None,
            credentials: None,
            event_sink: None,
            permitted_events: Arc::new(BTreeSet::new()),
            pnpm: None,
        }
    }

    /// Binds every session-bearing method to one exact live agent generation.
    pub fn with_owner(mut self, owner: AgentAuthority) -> Self {
        self.owner = Some(owner);
        self
    }

    pub fn with_logger(mut self, logger: Arc<dyn DomainLogger>) -> Self {
        self.logger = logger;
        self
    }

    pub fn with_settings(mut self, settings: Arc<Settings>) -> Self {
        self.settings = Some(settings);
        self
    }

    pub fn with_credentials(mut self, credentials: Arc<Credentials>) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Installs the only events a plugin may emit through this bridge.
    pub fn with_event_sink<I, S>(mut self, sink: Arc<dyn DomainEventSink>, permitted: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.event_sink = Some(sink);
        self.permitted_events = Arc::new(permitted.into_iter().map(Into::into).collect());
        self
    }

    /// Installs the profile-owned package-operation boundary.
    pub fn with_pnpm_boundary(mut self, boundary: Arc<dyn PnpmBoundary>) -> Self {
        self.pnpm = Some(boundary);
        self
    }
}

/// Shared Node/WASM product-domain bridge.
#[derive(Clone)]
pub struct DomainBridge {
    inner: Arc<BridgeInner>,
}

impl std::fmt::Debug for DomainBridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DomainBridge")
            .finish_non_exhaustive()
    }
}

struct BridgeInner {
    services: BridgeServices,
    limits: BridgeLimits,
    policy_registry: WasmPolicyRegistry,
    handle: Handle,
    callbacks: Arc<Semaphore>,
    generations: Mutex<BTreeMap<u64, GenerationState>>,
    web_routes: Mutex<WebRouteRegistry>,
    retired_generations: Mutex<BTreeSet<u64>>,
}

struct GenerationState {
    client: BridgeClient,
    cancellation: CancellationToken,
    registrations: BTreeMap<String, NativeRegistration>,
    timers: BTreeMap<String, TimerEntry>,
    next_timer_token: u64,
    routes: BTreeMap<String, WebRouteKey>,

    web_routes_supported: bool,
    operations: BTreeMap<u64, PnpmOperation>,
}

impl Drop for BridgeInner {
    fn drop(&mut self) {
        let states = std::mem::take(self.generations.get_mut());
        for (_, state) in states {
            cleanup_state(self, state);
        }
    }
}

struct TimerEntry {
    token: u64,
    task: Option<JoinHandle<()>>,
}

enum NativeRegistration {
    Tool {
        _registration: ToolRegistration,
    },
    Prompt {
        _registration: PromptRegistration,
    },
    Adapter {
        _registration: LlmProviderRegistration,
    },
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct WebRouteKey {
    kind: WebRouteKind,
    path: String,
}

struct PendingRequestGuard {
    client: BridgeClient,
    request_id: u64,
    armed: bool,
}

impl PendingRequestGuard {
    fn new(client: BridgeClient, request_id: u64) -> Self {
        Self {
            client,
            request_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.client.cancel(self.request_id);
        }
    }
}
#[derive(Clone, Debug)]
struct WebRouteEntry {
    generation: u64,
    route_id: String,
}

struct PnpmOperation {
    operation_id: String,
    cancellation: CancellationToken,
}

#[derive(Default)]
struct WebRouteRegistry {
    entries: BTreeMap<WebRouteKey, WebRouteEntry>,
}
struct PnpmOperationGuard {
    inner: Weak<BridgeInner>,
    generation: u64,
    request_id: u64,
    cancellation: CancellationToken,
    armed: bool,
}

impl PnpmOperationGuard {
    fn new(
        inner: Weak<BridgeInner>,
        generation: u64,
        request_id: u64,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            inner,
            generation,
            request_id,
            cancellation,
            armed: true,
        }
    }

    fn finish(&mut self) {
        self.remove();
        self.armed = false;
    }

    fn remove(&self) {
        if let Some(inner) = self.inner.upgrade() {
            if let Some(state) = lock(&inner.generations).get_mut(&self.generation) {
                state.operations.remove(&self.request_id);
            }
        }
    }
}

impl Drop for PnpmOperationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
            self.remove();
        }
    }
}

impl DomainBridge {
    /// Creates a bridge with conservative bounded defaults and no WASM policies.
    pub fn new(services: BridgeServices) -> BridgeResult<Self> {
        Self::with_limits(services, BridgeLimits::default())
    }

    pub fn with_limits(services: BridgeServices, limits: BridgeLimits) -> BridgeResult<Self> {
        Self::with_limits_and_policy_registry(services, limits, WasmPolicyRegistry::new())
    }

    /// Creates a bridge whose WASM calls are authorized by `policy_registry`.
    pub fn with_policy_registry(
        services: BridgeServices,
        policy_registry: WasmPolicyRegistry,
    ) -> BridgeResult<Self> {
        Self::with_limits_and_policy_registry(services, BridgeLimits::default(), policy_registry)
    }

    pub fn with_limits_and_policy_registry(
        services: BridgeServices,
        limits: BridgeLimits,
        policy_registry: WasmPolicyRegistry,
    ) -> BridgeResult<Self> {
        limits.validate()?;
        let handle = Handle::try_current()
            .map_err(|_| invalid("DomainBridge requires construction inside a Tokio runtime"))?;
        Ok(Self {
            inner: Arc::new(BridgeInner {
                services,
                callbacks: Arc::new(Semaphore::new(limits.max_callback_concurrency)),
                limits,
                policy_registry,
                handle,
                generations: Mutex::new(BTreeMap::new()),
                web_routes: Mutex::new(WebRouteRegistry::default()),
                retired_generations: Mutex::new(BTreeSet::new()),
            }),
        })
    }

    /// Attaches one generation-scoped Node client and installs this bridge as its handler.
    pub fn attach_client(&self, client: BridgeClient, generation: u64) -> BridgeResult<()> {
        if generation == 0 || client.generation() != generation {
            return Err(BridgeError::Generation {
                expected: generation.max(1),
                received: client.generation(),
            });
        }
        let web_routes_supported = client.supports_extension("web.route/v1");
        let mut states = lock(&self.inner.generations);
        if states.contains_key(&generation)
            || lock(&self.inner.retired_generations).contains(&generation)
        {
            return Err(remote(
                "DUPLICATE_GENERATION",
                "bridge generations cannot be attached twice",
            ));
        }
        client.set_handler(Arc::new(self.clone()));
        states.insert(
            generation,
            GenerationState {
                client,
                cancellation: fresh_cancellation(),
                registrations: BTreeMap::new(),
                timers: BTreeMap::new(),
                next_timer_token: 0,
                routes: BTreeMap::new(),
                web_routes_supported,
                operations: BTreeMap::new(),
            },
        );
        Ok(())
    }

    fn register_web_route(
        &self,
        generation: u64,
        request: WebRouteRegistration,
    ) -> BridgeResult<Value> {
        validate_web_route_registration(&request)?;
        let key = WebRouteKey {
            kind: request.kind,
            path: request.path,
        };
        let mut states = lock(&self.inner.generations);
        let state = states
            .get_mut(&generation)
            .ok_or_else(|| stale_generation(generation))?;
        if state.cancellation.is_cancelled() {
            return Err(stale_generation(generation));
        }
        if !state.web_routes_supported {
            return Err(remote(
                "WEB_ROUTE_UNSUPPORTED",
                "the connected compatibility host did not negotiate web.route/v1",
            ));
        }
        if state.routes.contains_key(&request.route_id) {
            return Err(remote(
                "DUPLICATE_ROUTE",
                "routeId is already active for this generation",
            ));
        }
        let mut routes = lock(&self.inner.web_routes);
        if routes.entries.len() >= MAX_WEB_ROUTES {
            return Err(remote(
                "ROUTE_LIMIT",
                "the bridge route limit has been reached",
            ));
        }
        if routes.entries.contains_key(&key) {
            return Err(remote(
                "DUPLICATE_ROUTE",
                "an identical route is already active",
            ));
        }
        routes.entries.insert(
            key.clone(),
            WebRouteEntry {
                generation,
                route_id: request.route_id.clone(),
            },
        );
        state.routes.insert(request.route_id, key);
        Ok(json!({"registered": true}))
    }

    fn remove_web_route(&self, generation: u64, route_id: &str) -> BridgeResult<Value> {
        nonblank("routeId", route_id)?;
        let mut states = lock(&self.inner.generations);
        let state = states
            .get_mut(&generation)
            .ok_or_else(|| stale_generation(generation))?;
        if state.cancellation.is_cancelled() {
            return Err(stale_generation(generation));
        }
        let Some(key) = state.routes.remove(route_id) else {
            return Ok(json!({"removed": false}));
        };
        lock(&self.inner.web_routes).entries.remove(&key);
        Ok(json!({"removed": true}))
    }

    /// Dispatches a bounded HTTP request to a matching live Node route.
    /// `None` lets the Axum integration continue to its static fallback.
    pub async fn dispatch_web_route(
        &self,
        mut request: WebRouteRequest,
    ) -> BridgeResult<Option<WebRouteResponse>> {
        let Some(entry) = self.select_web_route(&request.path) else {
            return Ok(None);
        };
        validate_web_request(&request)?;
        request.route_id = entry.route_id.clone();
        let (client, cancellation) = {
            let states = lock(&self.inner.generations);
            let state = states
                .get(&entry.generation)
                .ok_or_else(|| stale_generation(entry.generation))?;
            if state.cancellation.is_cancelled() {
                return Err(stale_generation(entry.generation));
            }
            (state.client.clone(), state.cancellation.clone())
        };
        let permit = Arc::clone(&self.inner.callbacks)
            .try_acquire_owned()
            .map_err(|_| BridgeError::QueueFull)?;
        let payload = serde_json::to_value(request).map_err(serialize_error)?;
        bounded_json(&payload, MAX_WEB_CALLBACK_BYTES)?;
        let pending = client.begin_request(FrameKind::WebRouteInvoke, payload)?;
        let request_id = pending.request_id();
        let mut cancel = PendingRequestGuard::new(client.clone(), request_id);
        let timeout = WEB_ROUTE_TIMEOUT;
        let wait = self
            .inner
            .handle
            .spawn_blocking(move || pending.wait(timeout));
        let result = tokio::select! {
            _ = cancellation.cancelled() => {
                return Err(stale_generation(entry.generation));
            }
            result = wait => result
                .map_err(|error| invalid(format!("route callback worker failed: {error}")))?,
        };
        let value = result?;
        cancel.disarm();
        drop(permit);
        bounded_json(&value, MAX_WEB_CALLBACK_BYTES)?;
        let response: WebRouteResponse = decode(value)?;
        validate_web_response(&response)?;
        Ok(Some(response))
    }

    pub(crate) fn has_web_route(&self, path: &str) -> bool {
        self.select_web_route(path).is_some()
    }

    fn select_web_route(&self, path: &str) -> Option<WebRouteEntry> {
        let routes = lock(&self.inner.web_routes);
        let exact = WebRouteKey {
            kind: WebRouteKind::Exact,
            path: path.into(),
        };
        routes.entries.get(&exact).cloned().or_else(|| {
            routes
                .entries
                .iter()
                .filter(|(key, _)| {
                    key.kind == WebRouteKind::Prefix && web_prefix_matches(path, &key.path)
                })
                .max_by_key(|(key, _)| key.path.len())
                .map(|(_, entry)| entry.clone())
        })
    }

    async fn pnpm_run(
        &self,
        generation: u64,
        request_id: u64,
        request: PnpmRunRequest,
    ) -> BridgeResult<Value> {
        validate_pnpm_run(&request)?;
        self.ensure_generation(generation)?;
        let runner = self.inner.services.pnpm.as_ref().ok_or_else(|| {
            remote(
                "PNPM_UNAVAILABLE",
                "package operations are unavailable for this profile",
            )
        })?;
        let (cancellation, output) = {
            let mut states = lock(&self.inner.generations);
            let state = states
                .get_mut(&generation)
                .ok_or_else(|| stale_generation(generation))?;
            if state.cancellation.is_cancelled() {
                return Err(stale_generation(generation));
            }
            if state
                .operations
                .values()
                .any(|operation| operation.operation_id == request.operation_id)
            {
                return Err(remote(
                    "DUPLICATE_OPERATION",
                    "operationId is already active for this generation",
                ));
            }
            let cancellation = fresh_cancellation();
            let output = PnpmOutputSink {
                client: state.client.clone(),
                operation_id: request.operation_id.clone(),
                cancellation: cancellation.clone(),
            };
            state.operations.insert(
                request_id,
                PnpmOperation {
                    operation_id: request.operation_id.clone(),
                    cancellation: cancellation.clone(),
                },
            );
            (cancellation, output)
        };
        let mut operation = PnpmOperationGuard::new(
            Arc::downgrade(&self.inner),
            generation,
            request_id,
            cancellation.clone(),
        );
        let result = runner.run(request, cancellation, output).await;
        operation.finish();
        let result = result?;
        validate_pnpm_result(&result, MAX_PNPM_RESULT_BYTES)?;
        serde_json::to_value(result).map_err(serialize_error)
    }

    fn cancel_pnpm_request(&self, generation: u64, request_id: u64) -> BridgeResult<bool> {
        let mut states = lock(&self.inner.generations);
        let state = states
            .get_mut(&generation)
            .ok_or_else(|| stale_generation(generation))?;
        let Some(operation) = state.operations.remove(&request_id) else {
            return Ok(false);
        };
        if operation.cancellation.is_cancelled() {
            return Ok(false);
        }
        operation.cancellation.cancel();
        Ok(true)
    }
    /// Cancels every callback and timer and drops all registration lifetime handles for `generation`.
    pub fn cleanup_generation(&self, generation: u64) {
        let state = lock(&self.inner.generations).remove(&generation);
        if let Some(state) = state {
            lock(&self.inner.retired_generations).insert(generation);
            cleanup_state(&self.inner, state);
        }
    }

    /// Direct deterministic entrypoint for in-memory tests and non-Node hosts.
    pub fn dispatch(&self, generation: u64, request: DomainRequest) -> BridgeResult<Value> {
        self.validate_request(&request, self.inner.limits.max_json_bytes)?;
        self.ensure_generation(generation)?;
        let value = self.block_on(async {
            tokio::time::timeout(
                self.inner.limits.request_timeout,
                self.dispatch_async(Some(generation), request),
            )
            .await
            .map_err(|_| BridgeError::Timeout)?
        })?;
        self.validate_result(value, self.inner.limits.max_json_bytes)
    }

    /// Native-only dispatch used by the WASM capability adapter. Registration methods are denied.
    pub fn dispatch_native(&self, request: DomainRequest) -> BridgeResult<Value> {
        self.validate_request(&request, self.inner.limits.max_json_bytes)?;
        let value = self.block_on(async {
            tokio::time::timeout(
                self.inner.limits.request_timeout,
                self.dispatch_async(None, request),
            )
            .await
            .map_err(|_| BridgeError::Timeout)?
        })?;
        self.validate_result(value, self.inner.limits.max_json_bytes)
    }

    fn block_on<T>(&self, future: impl Future<Output = BridgeResult<T>>) -> BridgeResult<T> {
        if Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| self.inner.handle.block_on(future))
        } else {
            self.inner.handle.block_on(future)
        }
    }

    fn validate_request(&self, request: &DomainRequest, limit: usize) -> BridgeResult<()> {
        if request.service.trim().is_empty() || request.method.trim().is_empty() {
            return Err(invalid("service and method must not be blank"));
        }
        let envelope = serde_json::to_value(request).map_err(serialize_error)?;
        bounded_json(&envelope, limit)
    }

    fn validate_result(&self, value: Value, limit: usize) -> BridgeResult<Value> {
        bounded_json(&value, limit)?;
        Ok(value)
    }

    async fn dispatch_async(
        &self,
        generation: Option<u64>,
        request: DomainRequest,
    ) -> BridgeResult<Value> {
        match request.service.as_str() {
            TOOLS_SERVICE => {
                self.tools(generation, &request.method, request.params)
                    .await
            }
            SYSTEM_PROMPT_SERVICE => {
                self.system_prompt(generation, &request.method, request.params)
            }
            LLM_SERVICE => self.llm(generation, &request.method, request.params).await,
            SESSIONS_SERVICE => {
                self.sessions(generation, &request.method, request.params)
                    .await
            }
            AGENTS_SERVICE => self.agents(&request.method, request.params).await,
            LOGGER_SERVICE => self.logger(&request.method, request.params),
            TIMERS_SERVICE => self.timers(generation, &request.method, request.params),
            SETTINGS_SERVICE => self.settings(&request.method, request.params),
            CREDENTIALS_SERVICE => self.credentials(&request.method, request.params).await,
            _ => Err(remote(
                "UNKNOWN_SERVICE",
                "service is not exposed by DomainBridge",
            )),
        }
    }

    async fn tools(
        &self,
        generation: Option<u64>,
        method: &str,
        params: Value,
    ) -> BridgeResult<Value> {
        match method {
            "schemas" => {
                decode_empty(params)?;
                Ok(json!({"tools": self.inner.services.tools.schemas()}))
            }
            "execute" => {
                let request: ToolExecute = decode(params)?;
                self.require_owner(&request.session)?;
                nonblank("name", &request.name)?;
                let output = self
                    .inner
                    .services
                    .tools
                    .execute(
                        ToolRunContext {
                            session: request.session,
                            call: request.call,
                            cancellation: generation_cancellation(&self.inner, generation)?,
                        },
                        request.name,
                        request.arguments,
                    )
                    .await;
                serde_json::to_value(output).map_err(serialize_error)
            }
            "register" => {
                let generation = require_node_generation(generation)?;
                let request: ToolRegister = decode(params)?;
                validate_registration(&request.registration_id, &request.callback_id)?;
                nonblank("name", &request.name)?;
                nonblank("description", &request.description)?;
                let definition = ToolDefinition::new(
                    request.name,
                    request.description,
                    request.parameters,
                    NodeToolHandler {
                        bridge: Arc::downgrade(&self.inner),
                        generation,
                        callback_id: request.callback_id,
                    },
                );
                let registration = self
                    .inner
                    .services
                    .tools
                    .register(definition)
                    .map_err(tessivum_error)?;
                self.add_registration(
                    generation,
                    request.registration_id,
                    NativeRegistration::Tool {
                        _registration: registration,
                    },
                )?;
                Ok(json!({"registered": true}))
            }
            _ => unknown_method(TOOLS_SERVICE, method),
        }
    }

    fn system_prompt(
        &self,
        generation: Option<u64>,
        method: &str,
        params: Value,
    ) -> BridgeResult<Value> {
        match method {
            "assemble" => {
                let request: PromptAssemble = decode(params)?;
                let sections = request
                    .sections
                    .into_iter()
                    .map(|section| PromptSection::new(section.id, section.order, section.text));
                let assembly = self
                    .inner
                    .services
                    .system_prompt
                    .assemble(sections, request.tools)
                    .map_err(tessivum_error)?;
                Ok(json!({"text": assembly.text, "tools": assembly.tools}))
            }
            "register" => {
                let generation = require_node_generation(generation)?;
                let request: PromptRegister = decode(params)?;
                validate_registration(&request.registration_id, "section")?;
                let registration = self
                    .inner
                    .services
                    .system_prompt
                    .register(PromptSection::new(request.id, request.order, request.text))
                    .map_err(tessivum_error)?;
                self.add_registration(
                    generation,
                    request.registration_id,
                    NativeRegistration::Prompt {
                        _registration: registration,
                    },
                )?;
                Ok(json!({"registered": true}))
            }
            _ => unknown_method(SYSTEM_PROMPT_SERVICE, method),
        }
    }

    async fn llm(
        &self,
        generation: Option<u64>,
        method: &str,
        params: Value,
    ) -> BridgeResult<Value> {
        match method {
            "generate" => {
                let request: LlmGenerate = decode(params)?;
                let generation_result = self
                    .inner
                    .services
                    .llm
                    .complete(
                        request.request,
                        generation_cancellation(&self.inner, generation)?,
                    )
                    .await
                    .map_err(tessivum_error)?;
                Ok(json!({
                    "message": generation_result.message,
                    "usage": generation_result.usage,
                    "finishReason": generation_result.finish_reason,
                    "chunks": generation_result.chunks,
                }))
            }
            "register" => {
                let generation = require_node_generation(generation)?;
                let request: LlmRegister = decode(params)?;
                validate_registration(&request.registration_id, &request.callback_id)?;
                nonblank("provider", &request.provider)?;
                let registration = self
                    .inner
                    .services
                    .llm
                    .register(
                        request.provider,
                        Arc::new(NodeLlmAdapter {
                            bridge: Arc::downgrade(&self.inner),
                            generation,
                            callback_id: request.callback_id,
                        }),
                    )
                    .map_err(tessivum_error)?;
                self.add_registration(
                    generation,
                    request.registration_id,
                    NativeRegistration::Adapter {
                        _registration: registration,
                    },
                )?;
                Ok(json!({"registered": true}))
            }
            _ => unknown_method(LLM_SERVICE, method),
        }
    }

    async fn sessions(
        &self,
        generation: Option<u64>,
        method: &str,
        params: Value,
    ) -> BridgeResult<Value> {
        match method {
            "append" => {
                let request: SessionAppend = decode(params)?;
                self.require_owner(&request.session)?;
                let session = self
                    .inner
                    .services
                    .sessions
                    .get(&request.session)
                    .ok_or_else(|| remote("SESSION_NOT_FOUND", "session is not live"))?;
                session
                    .append(
                        request.event,
                        generation_cancellation(&self.inner, generation)?,
                    )
                    .await
                    .map_err(session_error)?;
                Ok(json!({"appended": true}))
            }
            "read" => {
                let request: SessionRead = decode(params)?;
                self.require_owner(&request.session)?;
                let session = self
                    .inner
                    .services
                    .sessions
                    .get(&request.session)
                    .ok_or_else(|| remote("SESSION_NOT_FOUND", "session is not live"))?;
                Ok(json!({"events": session.events()}))
            }
            _ => unknown_method(SESSIONS_SERVICE, method),
        }
    }

    async fn agents(&self, method: &str, params: Value) -> BridgeResult<Value> {
        match method {
            "get" => {
                let request: AgentGet = decode(params)?;
                self.require_owner(&request.session)?;
                let agent = self.inner.services.agents.get(&request.session);
                Ok(json!({
                    "sessionId": request.session,
                    "live": agent.is_some(),
                    "status": agent.as_ref().map(|agent| agent.status()),
                }))
            }
            "send" => {
                let request: AgentSend = decode(params)?;
                self.require_owner(&request.session)?;
                self.inner
                    .services
                    .agents
                    .send(
                        &request.session,
                        request.message,
                        request.target,
                        request.wakeup,
                    )
                    .await
                    .map_err(agent_error)?;
                Ok(json!({"sent": true}))
            }
            "cancel" => {
                let request: AgentCancel = decode(params)?;
                self.require_owner(&request.session)?;
                let cancelled = self
                    .inner
                    .services
                    .agents
                    .cancel(&request.session, request.cause, request.keep_inbox)
                    .map_err(agent_error)?;
                Ok(json!({"cancelled": cancelled}))
            }
            _ => unknown_method(AGENTS_SERVICE, method),
        }
    }

    fn logger(&self, method: &str, params: Value) -> BridgeResult<Value> {
        if method != "log" {
            return unknown_method(LOGGER_SERVICE, method);
        }
        let request: LoggerCall = decode(params)?;
        nonblank("message", &request.message)?;
        self.inner
            .services
            .logger
            .log(request.level, &request.message, &request.fields);
        Ok(json!({"logged": true}))
    }

    fn timers(&self, generation: Option<u64>, method: &str, params: Value) -> BridgeResult<Value> {
        match method {
            "schedule" => {
                let generation = require_node_generation(generation)?;
                let request: TimerSchedule = decode(params)?;
                validate_registration(&request.registration_id, &request.callback_id)?;
                let (timer_token, cancellation) =
                    self.reserve_timer(generation, &request.registration_id)?;
                let (start, started) = oneshot::channel();
                let weak = Arc::downgrade(&self.inner);
                let registration_id = request.registration_id;
                let completion_id = registration_id.clone();
                let callback_id = request.callback_id;
                let payload = request.payload;
                let delay = Duration::from_millis(request.delay_ms);
                let task = self.inner.handle.spawn(async move {
                    if started.await.is_err() {
                        return;
                    }
                    tokio::time::sleep(delay).await;
                    if let Some(inner) = weak.upgrade() {
                        let _ = callback_to_node(
                            Arc::clone(&inner),
                            generation,
                            callback_id,
                            json!({"service": TIMERS_SERVICE, "method": "fire", "payload": payload}),
                            cancellation,
                        )
                        .await;
                        let _ = remove_timer_if_exact(&inner, generation, &completion_id, timer_token);
                    }
                });
                match self.install_timer_task(generation, &registration_id, timer_token, task) {
                    Ok(()) => {
                        if start.send(()).is_err() {
                            if let Some(entry) = remove_timer_if_exact(
                                &self.inner,
                                generation,
                                &registration_id,
                                timer_token,
                            ) {
                                if let Some(task) = entry.task {
                                    task.abort();
                                }
                            }
                        }
                    }
                    Err(task) => task.abort(),
                }
                Ok(json!({"timerId": registration_id}))
            }
            "cancel" => {
                let generation = require_node_generation(generation)?;
                let request: RegistrationDispose = decode(params)?;
                let removed = lock(&self.inner.generations)
                    .get_mut(&generation)
                    .and_then(|state| state.timers.remove(&request.registration_id))
                    .map(|entry| {
                        if let Some(task) = entry.task {
                            task.abort();
                        }
                        true
                    })
                    .unwrap_or(false);
                Ok(json!({"removed": removed}))
            }
            _ => unknown_method(TIMERS_SERVICE, method),
        }
    }
    fn settings(&self, method: &str, params: Value) -> BridgeResult<Value> {
        if !matches!(method, "get" | "describe") {
            return unknown_method(SETTINGS_SERVICE, method);
        }
        let settings = self
            .inner
            .services
            .settings
            .as_ref()
            .ok_or_else(|| remote("SERVICE_UNAVAILABLE", "settings is not configured"))?;
        let request: SettingsRead = decode(params)?;
        match method {
            "get" => {
                let descriptor = settings
                    .describe(&request.namespace)
                    .map_err(settings_error)?;
                Ok(json!({
                    "namespace": descriptor.namespace,
                    "revision": descriptor.revision,
                    "value": descriptor.resolved,
                }))
            }
            "describe" => serde_json::to_value(
                settings
                    .describe(&request.namespace)
                    .map_err(settings_error)?,
            )
            .map_err(serialize_error),
            _ => unreachable!("settings method was allowlisted"),
        }
    }

    async fn credentials(&self, method: &str, params: Value) -> BridgeResult<Value> {
        if method != "describe" {
            return unknown_method(CREDENTIALS_SERVICE, method);
        }
        let credentials = self
            .inner
            .services
            .credentials
            .as_ref()
            .ok_or_else(|| remote("SERVICE_UNAVAILABLE", "credentials is not configured"))?;
        let request: CredentialDescribe = decode(params)?;
        let reference = CredentialRef::new(request.reference).map_err(credential_error)?;
        serde_json::to_value(
            credentials
                .describe(&reference)
                .await
                .map_err(credential_error)?,
        )
        .map_err(serialize_error)
    }

    fn require_owner(&self, requested: &SessionId) -> BridgeResult<()> {
        let owner = self.inner.services.owner.as_ref().ok_or_else(|| {
            remote(
                "OWNER_REQUIRED",
                "session-bearing calls require an AgentAuthority owner",
            )
        })?;
        if !owner.is_live() {
            return Err(remote("OWNER_STALE", "AgentAuthority is no longer live"));
        }
        if owner.id() != *requested {
            return Err(remote("OWNER_DENIED", "request is outside the bound owner"));
        }
        Ok(())
    }

    fn reserve_timer(
        &self,
        generation: u64,
        registration_id: &str,
    ) -> BridgeResult<(u64, CancellationToken)> {
        let mut states = lock(&self.inner.generations);
        let state = states
            .get_mut(&generation)
            .ok_or_else(|| stale_generation(generation))?;
        if state.timers.len() >= self.inner.limits.max_timers_per_generation {
            return Err(remote(
                "CONCURRENCY_LIMIT",
                "timer limit for this generation is reached",
            ));
        }
        if state.timers.contains_key(registration_id)
            || state.registrations.contains_key(registration_id)
        {
            return Err(remote(
                "DUPLICATE_REGISTRATION",
                "registrationId is already active",
            ));
        }
        let token = state.next_timer_token;
        state.next_timer_token = state.next_timer_token.checked_add(1).ok_or_else(|| {
            remote(
                "TIMER_ID_EXHAUSTED",
                "timer generation cannot allocate more timers",
            )
        })?;
        state
            .timers
            .insert(registration_id.into(), TimerEntry { token, task: None });
        Ok((token, state.cancellation.clone()))
    }

    fn install_timer_task(
        &self,
        generation: u64,
        registration_id: &str,
        token: u64,
        task: JoinHandle<()>,
    ) -> Result<(), JoinHandle<()>> {
        let mut task = Some(task);
        if let Some(entry) = lock(&self.inner.generations)
            .get_mut(&generation)
            .and_then(|state| state.timers.get_mut(registration_id))
            .filter(|entry| entry.token == token && entry.task.is_none())
        {
            entry.task = task.take();
        }
        task.map_or(Ok(()), Err)
    }

    fn add_registration(
        &self,
        generation: u64,
        registration_id: String,
        registration: NativeRegistration,
    ) -> BridgeResult<()> {
        let mut states = lock(&self.inner.generations);
        let state = states
            .get_mut(&generation)
            .ok_or_else(|| stale_generation(generation))?;
        if state.registrations.contains_key(&registration_id)
            || state.timers.contains_key(&registration_id)
        {
            return Err(remote(
                "DUPLICATE_REGISTRATION",
                "registrationId is already active",
            ));
        }
        state.registrations.insert(registration_id, registration);
        Ok(())
    }

    fn remove_registration(&self, generation: u64, registration_id: &str) -> BridgeResult<bool> {
        let mut states = lock(&self.inner.generations);
        let state = states
            .get_mut(&generation)
            .ok_or_else(|| stale_generation(generation))?;
        if let Some(registration) = state.registrations.remove(registration_id) {
            drop(registration);
            return Ok(true);
        }
        if let Some(timer) = state.timers.remove(registration_id) {
            if let Some(task) = timer.task {
                task.abort();
            }
            return Ok(true);
        }
        Ok(false)
    }
    fn event_emit(&self, params: Value) -> BridgeResult<Value> {
        let request: EventEmit = decode(params)?;
        let sink = self
            .inner
            .services
            .event_sink
            .as_ref()
            .ok_or_else(|| remote("EVENT_DENIED", "no event sink is configured"))?;
        if !self
            .inner
            .services
            .permitted_events
            .contains(&request.event)
        {
            return Err(remote("EVENT_DENIED", "event is not permitted"));
        }
        sink.emit(&request.event, &request.payload)
            .map_err(tessivum_error)
    }

    async fn handle_frame(&self, frame: Frame) -> BridgeResult<Value> {
        frame.validate()?;
        bounded_json(&frame.payload, self.inner.limits.max_json_bytes)?;
        match frame.kind {
            FrameKind::ServiceCall | FrameKind::ServiceProvide => {
                let request: DomainRequest = decode(frame.payload)?;
                self.validate_request(&request, self.inner.limits.max_json_bytes)?;
                self.ensure_generation(frame.connection_generation)?;
                if frame.kind == FrameKind::ServiceProvide && request.method != "register" {
                    return Err(remote(
                        "INVALID_PROVIDE",
                        "service.provide only admits typed registrations",
                    ));
                }
                self.dispatch_async(Some(frame.connection_generation), request)
                    .await
            }
            FrameKind::ServiceRemove | FrameKind::RegistrationDispose => {
                self.ensure_generation(frame.connection_generation)?;
                let request: RegistrationDispose = decode(frame.payload)?;
                Ok(json!({
                    "removed": self.remove_registration(frame.connection_generation, &request.registration_id)?
                }))
            }
            FrameKind::EventEmit => {
                self.ensure_generation(frame.connection_generation)?;
                self.event_emit(frame.payload)
            }
            FrameKind::WebRouteRegister => {
                self.ensure_generation(frame.connection_generation)?;
                self.register_web_route(frame.connection_generation, decode(frame.payload)?)
            }
            FrameKind::WebRouteRemove => {
                self.ensure_generation(frame.connection_generation)?;
                let request: WebRouteRemove = decode(frame.payload)?;
                self.remove_web_route(frame.connection_generation, &request.route_id)
            }
            FrameKind::PnpmRun => {
                self.ensure_generation(frame.connection_generation)?;
                let request_id = frame
                    .request_id
                    .ok_or_else(|| invalid("pnpm.run requires a request id"))?;
                self.pnpm_run(
                    frame.connection_generation,
                    request_id,
                    decode(frame.payload)?,
                )
                .await
            }
            FrameKind::Cancel => {
                self.ensure_generation(frame.connection_generation)?;
                let request_id = frame
                    .request_id
                    .ok_or_else(|| invalid("cancel requires a request id"))?;
                Ok(json!({
                    "cancelled": self.cancel_pnpm_request(frame.connection_generation, request_id)?
                }))
            }
            _ => Err(remote(
                "UNSUPPORTED_FRAME",
                "frame kind is not a product-domain request",
            )),
        }
    }

    fn ensure_generation(&self, generation: u64) -> BridgeResult<()> {
        let states = lock(&self.inner.generations);
        let state = states
            .get(&generation)
            .ok_or_else(|| stale_generation(generation))?;
        if state.cancellation.is_cancelled() {
            return Err(stale_generation(generation));
        }
        Ok(())
    }
}

impl BridgeHandler for DomainBridge {
    fn handle(&self, frame: Frame) -> BridgeResult<Value> {
        let is_pnpm = frame.kind == FrameKind::PnpmRun;
        let result_limit = if is_pnpm {
            MAX_PNPM_RESULT_BYTES
        } else {
            self.inner.limits.max_json_bytes
        };
        let request_timeout = if is_pnpm {
            WEB_ROUTE_TIMEOUT
        } else {
            self.inner.limits.request_timeout
        };
        let value = self.block_on(async {
            tokio::time::timeout(request_timeout, self.handle_frame(frame))
                .await
                .map_err(|_| BridgeError::Timeout)?
        })?;
        self.validate_result(value, result_limit)
    }
}

impl CapabilityHandler for DomainBridge {
    fn call(&self, request: CapabilityRequest) -> WasmResult<Value> {
        if request.capability != Capability::ServiceCall {
            return Err(plugin_error(
                "CAPABILITY_DENIED",
                "only cordis.service.call is handled",
            ));
        }
        bounded_json(&request.payload, self.inner.limits.max_json_bytes)
            .map_err(bridge_to_plugin_error)?;
        let plugin_id = request.plugin_id;
        let instance_id = request.instance_id;
        let wire: WasmServiceRequest = decode(request.payload).map_err(bridge_to_plugin_error)?;
        let domain = DomainRequest {
            service: wire.service,
            method: wire.method,
            params: wire.payload,
        };
        self.validate_request(&domain, self.inner.limits.max_json_bytes)
            .map_err(bridge_to_plugin_error)?;
        let _lease = self.inner.policy_registry.authorize(
            &instance_id,
            &plugin_id,
            &domain.service,
            &domain.method,
        )?;
        self.dispatch_native(domain).map_err(bridge_to_plugin_error)
    }
}

#[async_trait]
impl ToolHandler for NodeToolHandler {
    async fn run(
        &self,
        context: ToolRunContext,
        arguments: Value,
    ) -> Result<ToolOutput, TessivumError> {
        let Some(inner) = self.bridge.upgrade() else {
            return Err(callback_error(
                "STALE_GENERATION",
                "bridge has been dropped",
            ));
        };
        let value = callback_to_node(
            inner,
            self.generation,
            self.callback_id.clone(),
            json!({
                "service": TOOLS_SERVICE,
                "method": "execute",
                "context": {"session": context.session, "call": context.call},
                "arguments": arguments,
            }),
            context.cancellation,
        )
        .await
        .map_err(bridge_to_tessivum)?;
        serde_json::from_value(value)
            .map_err(|error| callback_error("INVALID_CALLBACK_RESPONSE", error.to_string()))
    }
}
struct NodeToolHandler {
    bridge: Weak<BridgeInner>,
    generation: u64,
    callback_id: String,
}

#[async_trait]
impl LlmAdapter for NodeLlmAdapter {
    async fn generate(
        &self,
        request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        let Some(inner) = self.bridge.upgrade() else {
            return Err(callback_error(
                "STALE_GENERATION",
                "bridge has been dropped",
            ));
        };
        let value = callback_to_node(
            inner,
            self.generation,
            self.callback_id.clone(),
            json!({"service": LLM_SERVICE, "method": "generate", "request": request}),
            cancellation,
        )
        .await
        .map_err(bridge_to_tessivum)?;
        let response: LlmCallbackResponse = serde_json::from_value(value)
            .map_err(|error| callback_error("INVALID_CALLBACK_RESPONSE", error.to_string()))?;
        Ok(Box::pin(stream::iter(response.chunks.into_iter().map(Ok))))
    }
}

struct NodeLlmAdapter {
    bridge: Weak<BridgeInner>,
    generation: u64,
    callback_id: String,
}

async fn callback_to_node(
    inner: Arc<BridgeInner>,
    generation: u64,
    callback_id: String,
    payload: Value,
    cancellation: CancellationToken,
) -> BridgeResult<Value> {
    if cancellation.is_cancelled() {
        return Err(BridgeError::Cancelled);
    }
    bounded_json(&payload, inner.limits.max_callback_bytes)?;
    let (client, generation_cancellation) = {
        let states = lock(&inner.generations);
        let state = states
            .get(&generation)
            .ok_or_else(|| stale_generation(generation))?;
        (state.client.clone(), state.cancellation.clone())
    };
    if generation_cancellation.is_cancelled() {
        return Err(stale_generation(generation));
    }
    let permit = Arc::clone(&inner.callbacks)
        .try_acquire_owned()
        .map_err(|_| BridgeError::QueueFull)?;
    let payload = json!({"callbackId": callback_id, "payload": payload});
    bounded_json(&payload, inner.limits.max_callback_bytes)?;
    let pending = client.begin_request(FrameKind::EventCallback, payload)?;
    let request_id = pending.request_id();
    let timeout = inner.limits.callback_timeout;
    let wait = inner.handle.spawn_blocking(move || pending.wait(timeout));
    tokio::select! {
        _ = cancellation.cancelled() => {
            client.cancel(request_id);
            drop(permit);
            Err(BridgeError::Cancelled)
        }
        _ = generation_cancellation.cancelled() => {
            client.cancel(request_id);
            drop(permit);
            Err(stale_generation(generation))
        }
        result = wait => {
            drop(permit);
            let value = result.map_err(|error| invalid(format!("callback worker failed: {error}")))??;
            bounded_json(&value, inner.limits.max_callback_bytes)?;
            Ok(value)
        }
    }
}

fn remove_timer_if_exact(
    inner: &BridgeInner,
    generation: u64,
    registration_id: &str,
    token: u64,
) -> Option<TimerEntry> {
    let mut states = lock(&inner.generations);
    let state = states.get_mut(&generation)?;
    if state
        .timers
        .get(registration_id)
        .is_some_and(|entry| entry.token == token)
    {
        state.timers.remove(registration_id)
    } else {
        None
    }
}

fn cleanup_state(inner: &BridgeInner, mut state: GenerationState) {
    state.cancellation.cancel();
    for (_, operation) in std::mem::take(&mut state.operations) {
        operation.cancellation.cancel();
    }
    for (_, key) in std::mem::take(&mut state.routes) {
        inner.web_routes.lock().entries.remove(&key);
    }
    for (_, timer) in std::mem::take(&mut state.timers) {
        if let Some(task) = timer.task {
            task.abort();
        }
    }
    // Dropping these handles is the registration removal operation.
    state.registrations.clear();
}

fn generation_cancellation(
    inner: &BridgeInner,
    generation: Option<u64>,
) -> BridgeResult<CancellationToken> {
    match generation {
        Some(generation) => lock(&inner.generations)
            .get(&generation)
            .map(|state| state.cancellation.clone())
            .ok_or_else(|| stale_generation(generation)),
        None => Ok(fresh_cancellation()),
    }
}

fn require_node_generation(generation: Option<u64>) -> BridgeResult<u64> {
    generation.ok_or_else(|| {
        remote(
            "REGISTRATION_DENIED",
            "native registrations require a Node generation",
        )
    })
}

fn fresh_cancellation() -> CancellationToken {
    tessivum_core::ContextHandle::root().scope().cancellation()
}

fn decode<T: for<'de> Deserialize<'de>>(value: Value) -> BridgeResult<T> {
    serde_json::from_value(value).map_err(|error| remote("INVALID_SCHEMA", error.to_string()))
}

fn decode_empty(value: Value) -> BridgeResult<()> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    let _: Empty = decode(value)?;
    Ok(())
}

fn bounded_json(value: &Value, limit: usize) -> BridgeResult<()> {
    let bytes = serde_json::to_vec(value).map_err(serialize_error)?;
    if bytes.len() > limit {
        return Err(remote(
            "PAYLOAD_TOO_LARGE",
            "JSON envelope exceeds its configured limit",
        ));
    }
    Ok(())
}
fn validate_web_route_registration(request: &WebRouteRegistration) -> BridgeResult<()> {
    validate_identifier("routeId", &request.route_id, 128)?;
    validate_product_path(&request.path)
}

fn validate_web_request(request: &WebRouteRequest) -> BridgeResult<()> {
    if request.method.is_empty()
        || request.method.len() > 16
        || !request.method.bytes().all(is_http_token)
    {
        return Err(remote(
            "INVALID_HTTP_REQUEST",
            "method is not a valid HTTP token",
        ));
    }
    validate_product_path(&request.path)?;
    if request.query.len() > MAX_WEB_QUERY_BYTES
        || request.query.contains('\0')
        || request.query.contains('#')
    {
        return Err(remote(
            "INVALID_HTTP_REQUEST",
            "query is invalid or too large",
        ));
    }
    validate_web_headers(&request.headers, false)?;
    base64_decoded_len(&request.body_base64, WEB_REQUEST_BODY_LIMIT)?;
    Ok(())
}

fn validate_web_response(response: &WebRouteResponse) -> BridgeResult<()> {
    if !(100..=599).contains(&response.status) {
        return Err(remote(
            "INVALID_HTTP_RESPONSE",
            "status is outside the HTTP range",
        ));
    }
    validate_web_headers(&response.headers, true)?;
    base64_decoded_len(&response.body_base64, WEB_RESPONSE_BODY_LIMIT)?;
    Ok(())
}
fn validate_product_path(path: &str) -> BridgeResult<()> {
    if path.len() > MAX_WEB_PATH_BYTES
        || (path != "/dsh-market" && !path.starts_with("/dsh-market/"))
        || path.contains('\0')
        || path.contains('%')
        || path.contains('?')
        || path.contains('#')
        || path.split('/').any(|segment| matches!(segment, "." | ".."))
    {
        return Err(remote(
            "INVALID_ROUTE_PATH",
            "routes must stay under /dsh-market without traversal or escapes",
        ));
    }
    Ok(())
}

fn validate_web_headers(headers: &[(String, String)], response: bool) -> BridgeResult<()> {
    if headers.len() > MAX_WEB_HEADERS {
        return Err(remote("HEADER_LIMIT", "too many HTTP headers"));
    }
    let mut bytes = 0usize;
    let mut seen = BTreeSet::new();
    for (name, value) in headers {
        if name.is_empty() || !name.bytes().all(is_http_token) {
            return Err(remote("INVALID_HTTP_HEADER", "header name is invalid"));
        }
        let normalized = name.to_ascii_lowercase();
        if is_hop_header(&normalized)
            || (response && normalized == "content-length")
            || (response && !seen.insert(normalized))
        {
            return Err(remote(
                "INVALID_HTTP_HEADER",
                "header is forbidden or duplicated",
            ));
        }
        if value.contains('\0') || value.contains('\r') || value.contains('\n') {
            return Err(remote("INVALID_HTTP_HEADER", "header value is invalid"));
        }
        bytes = bytes
            .checked_add(name.len() + value.len())
            .ok_or_else(|| remote("HEADER_LIMIT", "headers are too large"))?;
        if bytes > MAX_WEB_HEADER_BYTES {
            return Err(remote("HEADER_LIMIT", "headers are too large"));
        }
    }
    Ok(())
}

fn web_prefix_matches(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| prefix.ends_with('/') || suffix.starts_with('/'))
}

fn validate_pnpm_run(request: &PnpmRunRequest) -> BridgeResult<()> {
    validate_identifier("operationId", &request.operation_id, 128)?;
    let invoking_dir = Path::new(&request.invoking_dir);
    if request.invoking_dir.is_empty()
        || request.invoking_dir.len() > MAX_WEB_PATH_BYTES
        || request.invoking_dir.contains('\0')
        || !invoking_dir.is_absolute()
        || invoking_dir
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(remote(
            "INVALID_PNPM_REQUEST",
            "invokingDir must be an absolute traversal-free path",
        ));
    }
    crate::plugin_manager::validate_market_pnpm_args(&request.args)
        .map_err(|message| remote("INVALID_PNPM_REQUEST", message))
}

fn validate_pnpm_result(result: &PnpmRunResult, limit: usize) -> BridgeResult<()> {
    if result
        .signal
        .as_deref()
        .is_some_and(|signal| signal.is_empty() || signal.len() > 64 || signal.contains('\0'))
    {
        return Err(remote("INVALID_PNPM_RESULT", "signal is invalid"));
    }
    let value = serde_json::to_value(result).map_err(serialize_error)?;
    bounded_json(&value, limit)
}

fn validate_identifier(field: &str, value: &str, max: usize) -> BridgeResult<()> {
    if value.is_empty()
        || value.len() > max
        || value.contains('\0')
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(remote("INVALID_SCHEMA", format!("{field} is invalid")));
    }
    Ok(())
}

fn is_http_token(byte: u8) -> bool {
    matches!(byte, b'!' | b'#'..=b'\'' | b'*'..=b'+' | b'-' | b'.' | b'^'..=b'`' | b'|' | b'~')
        || byte.is_ascii_alphanumeric()
}

pub(crate) fn is_hop_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Encodes a Web route body without allocating any intermediate text buffers.
pub fn encode_web_body(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        encoded.push(TABLE[(first >> 2) as usize] as char);
        encoded.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            TABLE[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

/// Decodes a validated Web route response body.
pub fn decode_web_body(encoded: &str) -> BridgeResult<Vec<u8>> {
    decode_base64(encoded, WEB_RESPONSE_BODY_LIMIT)
}

fn base64_decoded_len(encoded: &str, limit: usize) -> BridgeResult<usize> {
    let bytes = encoded.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(remote(
            "INVALID_BASE64",
            "base64 must use complete quartets",
        ));
    }
    let padding = bytes.ends_with(b"==") as usize * 2
        + (bytes.ends_with(b"=") && !bytes.ends_with(b"==")) as usize;
    let length = bytes
        .len()
        .checked_div(4)
        .and_then(|groups| groups.checked_mul(3))
        .and_then(|length| length.checked_sub(padding))
        .ok_or_else(|| remote("INVALID_BASE64", "base64 padding is invalid"))?;
    if length > limit {
        return Err(remote(
            "PAYLOAD_TOO_LARGE",
            "body exceeds its configured limit",
        ));
    }
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte == b'=' {
            if index < bytes.len().saturating_sub(padding) {
                return Err(remote("INVALID_BASE64", "base64 padding is misplaced"));
            }
        } else if base64_value(byte).is_none() {
            return Err(remote("INVALID_BASE64", "body is not base64"));
        }
    }
    if padding != 0 && bytes.len() < 4 {
        return Err(remote("INVALID_BASE64", "base64 padding is invalid"));
    }
    Ok(length)
}

fn decode_base64(encoded: &str, limit: usize) -> BridgeResult<Vec<u8>> {
    let length = base64_decoded_len(encoded, limit)?;
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(length);
    for chunk in bytes.chunks_exact(4) {
        let first = base64_value(chunk[0]).expect("validated base64");
        let second = base64_value(chunk[1]).expect("validated base64");
        let third = if chunk[2] == b'=' {
            0
        } else {
            base64_value(chunk[2]).expect("validated base64")
        };
        let fourth = if chunk[3] == b'=' {
            0
        } else {
            base64_value(chunk[3]).expect("validated base64")
        };
        decoded.push((first << 2) | (second >> 4));
        if chunk[2] != b'=' {
            decoded.push((second << 4) | (third >> 2));
        }
        if chunk[3] != b'=' {
            decoded.push((third << 6) | fourth);
        }
    }
    Ok(decoded)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn validate_policy(policy: &WasmEffectivePolicy) -> WasmResult<()> {
    if policy.plugin_id.trim().is_empty()
        || policy.instance_id.trim().is_empty()
        || policy.entry_id.trim().is_empty()
    {
        return Err(policy_error(
            "MANIFEST_PERMISSION_INVALID",
            "plugin, instance, and entry ids must not be blank",
        ));
    }
    for permission in &policy.methods {
        if permission.service.trim().is_empty()
            || permission.method.trim().is_empty()
            || permission.service.contains('*')
            || permission.method.contains('*')
        {
            return Err(policy_error(
                "MANIFEST_PERMISSION_INVALID",
                "service permissions must be exact nonblank names",
            ));
        }
    }
    Ok(())
}

fn validate_registration(registration_id: &str, callback_id: &str) -> BridgeResult<()> {
    nonblank("registrationId", registration_id)?;
    nonblank("callbackId", callback_id)
}

fn nonblank(field: &str, value: &str) -> BridgeResult<()> {
    if value.trim().is_empty() {
        Err(remote(
            "INVALID_SCHEMA",
            format!("{field} must not be blank"),
        ))
    } else {
        Ok(())
    }
}

fn stale_generation(generation: u64) -> BridgeError {
    remote(
        "STALE_GENERATION",
        format!("generation {generation} is not attached"),
    )
}

fn unknown_method(service: &str, method: &str) -> BridgeResult<Value> {
    Err(remote(
        "UNKNOWN_METHOD",
        format!("{method} is not allowed for {service}"),
    ))
}

fn invalid(message: impl Into<String>) -> BridgeError {
    BridgeError::InvalidFrame(message.into())
}

fn remote(code: impl Into<String>, message: impl Into<String>) -> BridgeError {
    BridgeError::Remote(RemoteError::new(code, message))
}

fn tessivum_error(error: TessivumError) -> BridgeError {
    BridgeError::Remote(RemoteError::new(error.code, error.message).with_details(error.details))
}

fn bridge_to_tessivum(error: BridgeError) -> TessivumError {
    let (code, message, details) = match error {
        BridgeError::Remote(error) => (
            error.code,
            error.message,
            error.details.unwrap_or(Value::Null),
        ),
        BridgeError::Cancelled => (
            "CANCELLED".into(),
            "bridge callback was cancelled".into(),
            Value::Null,
        ),
        BridgeError::Timeout => (
            "CALLBACK_TIMEOUT".into(),
            "bridge callback timed out".into(),
            Value::Null,
        ),
        other => (
            "BRIDGE_CALLBACK_FAILED".into(),
            other.to_string(),
            Value::Null,
        ),
    };
    TessivumError::new(code, message, "bridge", details)
}

fn bridge_to_plugin_error(error: BridgeError) -> PluginError {
    let code = match error {
        BridgeError::Remote(error) => error.code,
        BridgeError::Cancelled => "CANCELLED".into(),
        BridgeError::Timeout => "RESOURCE_LIMIT".into(),
        BridgeError::FrameTooLarge { .. } => "PAYLOAD_TOO_LARGE".into(),
        BridgeError::InvalidFrame(_) => "INVALID_SCHEMA".into(),
        BridgeError::QueueFull => "RESOURCE_LIMIT".into(),
        BridgeError::Generation { .. } => "INSTANCE_STOPPED".into(),
        BridgeError::Io(_)
        | BridgeError::ProtocolVersion { .. }
        | BridgeError::Handshake(_)
        | BridgeError::Disconnected(_)
        | BridgeError::Process(_) => "SERVICE_CALL_FAILED".into(),
    };
    let message = match code.as_str() {
        "PAYLOAD_TOO_LARGE" => "service request exceeds its configured limit",
        "INVALID_SCHEMA" => "service request does not match the expected schema",
        "RESOURCE_LIMIT" => "service call exceeded a resource limit",
        "INSTANCE_STOPPED" => "plugin instance has stopped",
        "UNKNOWN_SERVICE" | "UNKNOWN_METHOD" => "service method is unavailable",
        "SERVICE_UNAVAILABLE" => "service is unavailable",
        _ => "service call failed",
    };
    PluginError::new(code, message, "host")
}

fn callback_error(code: impl Into<String>, message: impl Into<String>) -> TessivumError {
    TessivumError::new(code, message, "bridge", Value::Null)
}

fn serialize_error(error: serde_json::Error) -> BridgeError {
    remote("SERIALIZATION_FAILED", error.to_string())
}

fn session_error(error: crate::session::SessionError) -> BridgeError {
    remote(error.code(), error.to_string())
}

fn agent_error(error: crate::agent::AgentError) -> BridgeError {
    remote("AGENT_ERROR", error.to_string())
}

fn settings_error(error: crate::settings::SettingsError) -> BridgeError {
    remote(error.code(), error.to_string())
}

fn credential_error(error: crate::credentials::CredentialError) -> BridgeError {
    remote(error.code(), error.to_string())
}

fn plugin_error(code: impl Into<String>, message: impl Into<String>) -> PluginError {
    PluginError::new(code, message, "host")
}

fn policy_error(code: impl Into<String>, message: impl Into<String>) -> PluginError {
    PluginError::new(code, message, "host")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolExecute {
    session: SessionId,
    call: ToolCallId,
    name: String,
    arguments: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolRegister {
    registration_id: String,
    callback_id: String,
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PromptSectionWire {
    id: String,
    order: i64,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PromptAssemble {
    #[serde(default)]
    sections: Vec<PromptSectionWire>,
    #[serde(default)]
    tools: Vec<crate::ToolSchema>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PromptRegister {
    registration_id: String,
    id: String,
    order: i64,
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LlmGenerate {
    request: GenerateRequest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LlmRegister {
    registration_id: String,
    callback_id: String,
    provider: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LlmCallbackResponse {
    chunks: Vec<StreamChunk>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionAppend {
    session: SessionId,
    event: SessionEvent,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionRead {
    session: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentGet {
    session: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentSend {
    session: SessionId,
    message: Message,
    target: InboxTarget,
    #[serde(default)]
    wakeup: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentCancel {
    session: SessionId,
    cause: crate::agent::AgentCancelCause,
    #[serde(default)]
    keep_inbox: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoggerCall {
    level: LogLevel,
    message: String,
    #[serde(default)]
    fields: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TimerSchedule {
    registration_id: String,
    callback_id: String,
    delay_ms: u64,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegistrationDispose {
    registration_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsRead {
    namespace: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialDescribe {
    reference: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EventEmit {
    event: String,
    #[serde(default)]
    payload: Value,
}
#[cfg(test)]
mod alpha11_tests {
    use std::{io::Cursor, sync::Arc};

    use super::*;
    use crate::{
        agent::AgentRegistry,
        llm::LlmRuntime,
        session::{MemorySessionPersistence, SessionStore},
        system_prompt::SystemPrompt,
        tools::ToolRuntime,
    };
    use tessivum_node_bridge::ClientConfig;
    use tokio::sync::Notify;

    fn bridge() -> DomainBridge {
        let tools = ToolRuntime::new();
        let sessions = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
        let agents = AgentRegistry::new(sessions.clone());
        DomainBridge::new(BridgeServices::new(
            tools,
            SystemPrompt::new(),
            LlmRuntime::new(),
            sessions,
            agents,
        ))
        .expect("bridge constructs")
    }

    fn client(generation: u64) -> BridgeClient {
        BridgeClient::from_io(
            Cursor::new(Vec::<u8>::new()),
            Vec::<u8>::new(),
            generation,
            ClientConfig::default(),
        )
        .expect("test client constructs")
    }

    #[cfg(unix)]
    #[test]
    fn pnpm_output_stream_does_not_abort_after_its_rolling_tail_size() {
        use std::{os::unix::net::UnixStream, thread};
        use tessivum_node_bridge::FrameCodec;

        let (rust, mut node) = UnixStream::pair().expect("duplex pair constructs");
        let client = BridgeClient::from_io(
            rust.try_clone().expect("reader clones"),
            rust,
            1,
            ClientConfig::default(),
        )
        .expect("client constructs");
        let peer = thread::spawn(move || {
            let codec = FrameCodec::new(1024 * 1024).expect("codec constructs");
            assert_eq!(
                codec.read_frame(&mut node).expect("hello reads").kind,
                FrameKind::Hello
            );
            codec
                .write_frame(&mut node, &Frame::ready(1))
                .expect("ready writes");
            for _ in 0..20 {
                assert_eq!(
                    codec.read_frame(&mut node).expect("output reads").kind,
                    FrameKind::PnpmOutput
                );
            }
        });
        client
            .handshake(Duration::from_secs(1))
            .expect("client becomes ready");
        let sink = PnpmOutputSink {
            client,
            operation_id: "large-install".into(),
            cancellation: fresh_cancellation(),
        };
        let chunk = [b'x'; 16 * 1024];
        for _ in 0..20 {
            sink.emit(PnpmOutputStream::Stdout, &chunk)
                .expect("streaming more than 256 KiB remains valid");
        }
        peer.join().expect("peer joins");
    }

    fn attach(bridge: &DomainBridge, generation: u64) {
        bridge
            .attach_client(client(generation), generation)
            .expect("test generation attaches");
        lock(&bridge.inner.generations)
            .get_mut(&generation)
            .expect("attached state")
            .web_routes_supported = true;
    }

    fn register(
        bridge: &DomainBridge,
        generation: u64,
        route_id: &str,
        kind: WebRouteKind,
        path: &str,
    ) {
        assert_eq!(
            BridgeHandler::handle(
                bridge,
                Frame::request(
                    generation,
                    1,
                    FrameKind::WebRouteRegister,
                    json!({"routeId": route_id, "kind": kind, "path": path}),
                ),
            )
            .expect("route registers"),
            json!({"registered": true})
        );
    }

    fn remote_code(error: BridgeError) -> String {
        match error {
            BridgeError::Remote(error) => error.code,
            error => panic!("expected remote failure: {error:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn routes_require_negotiated_extension() {
        let bridge = bridge();
        bridge
            .attach_client(client(1), 1)
            .expect("unnegotiated client attaches");
        assert_eq!(
            remote_code(
                bridge
                    .register_web_route(
                        1,
                        WebRouteRegistration {
                            route_id: "blocked".into(),
                            kind: WebRouteKind::Exact,
                            path: "/dsh-market/blocked".into(),
                        },
                    )
                    .expect_err("old ready payload cannot register routes"),
            ),
            "WEB_ROUTE_UNSUPPORTED"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn routes_prefer_exact_then_longest_prefix_and_cleanup() {
        let bridge = bridge();
        attach(&bridge, 1);
        register(&bridge, 1, "root", WebRouteKind::Prefix, "/dsh-market");
        register(
            &bridge,
            1,
            "nested",
            WebRouteKind::Prefix,
            "/dsh-market/admin",
        );
        register(
            &bridge,
            1,
            "exact",
            WebRouteKind::Exact,
            "/dsh-market/admin/ping",
        );

        assert_eq!(
            bridge
                .select_web_route("/dsh-market/admin/ping")
                .expect("exact match")
                .route_id,
            "exact"
        );
        assert_eq!(
            bridge
                .select_web_route("/dsh-market/admin/users")
                .expect("longest prefix")
                .route_id,
            "nested"
        );
        assert_eq!(
            bridge
                .select_web_route("/dsh-market-other")
                .map(|route| route.route_id),
            None
        );
        bridge.cleanup_generation(1);
        assert!(!bridge.has_web_route("/dsh-market/admin/ping"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn routes_reject_cross_generation_conflicts_and_remove_by_owner() {
        let bridge = bridge();
        attach(&bridge, 1);
        attach(&bridge, 2);
        register(&bridge, 1, "one", WebRouteKind::Exact, "/dsh-market/status");
        assert_eq!(
            remote_code(
                BridgeHandler::handle(
                    &bridge,
                    Frame::request(
                        2,
                        2,
                        FrameKind::WebRouteRegister,
                        json!({"routeId": "two", "kind": "exact", "path": "/dsh-market/status"}),
                    ),
                )
                .expect_err("conflicting route rejects"),
            ),
            "DUPLICATE_ROUTE"
        );
        assert_eq!(
            BridgeHandler::handle(
                &bridge,
                Frame::request(1, 3, FrameKind::WebRouteRemove, json!({"routeId": "one"})),
            )
            .expect("owner removes route"),
            json!({"removed": true})
        );
        assert!(!bridge.has_web_route("/dsh-market/status"));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn route_request_is_forwarded_to_its_owning_generation() {
        use std::{os::unix::net::UnixStream, sync::mpsc, thread};

        use tessivum_node_bridge::FrameCodec;

        let (rust, mut node) = UnixStream::pair().expect("duplex pair constructs");
        let client = BridgeClient::from_io(
            rust.try_clone().expect("reader clones"),
            rust,
            1,
            ClientConfig::default(),
        )
        .expect("client constructs");
        let (seen_tx, seen_rx) = mpsc::sync_channel(1);
        let peer = thread::spawn(move || {
            let codec = FrameCodec::new(1024 * 1024).expect("codec constructs");
            assert_eq!(
                codec.read_frame(&mut node).expect("hello reads").kind,
                FrameKind::Hello
            );
            codec
                .write_frame(&mut node, &Frame::ready(1))
                .expect("ready writes");
            let invoke = codec.read_frame(&mut node).expect("route invocation reads");
            assert_eq!(invoke.kind, FrameKind::WebRouteInvoke);
            seen_tx.send(invoke.payload).expect("invocation reports");
            codec
                .write_frame(
                    &mut node,
                    &Frame::response(
                        1,
                        invoke.request_id.expect("invoke has correlation"),
                        json!({"status": 201, "headers": [["content-type", "text/plain"]], "bodyBase64": "b2s="}),
                    ),
                )
                .expect("route response writes");
        });
        client
            .handshake(Duration::from_secs(1))
            .expect("client becomes ready");
        let bridge = bridge();
        bridge.attach_client(client, 1).expect("bridge attaches");
        lock(&bridge.inner.generations)
            .get_mut(&1)
            .expect("attached generation")
            .web_routes_supported = true;
        bridge
            .register_web_route(
                1,
                WebRouteRegistration {
                    route_id: "synthetic".into(),
                    kind: WebRouteKind::Exact,
                    path: "/dsh-market/synthetic".into(),
                },
            )
            .expect("route registers");
        let response = bridge
            .dispatch_web_route(WebRouteRequest {
                route_id: String::new(),
                method: "POST".into(),
                path: "/dsh-market/synthetic".into(),
                query: "page=1".into(),
                headers: vec![("content-type".into(), "text/plain".into())],
                body_base64: encode_web_body(b"request"),
            })
            .await
            .expect("route invocation succeeds")
            .expect("route matches");
        assert_eq!(response.status, 201);
        assert_eq!(
            decode_web_body(&response.body_base64).expect("response decodes"),
            b"ok"
        );
        let payload = seen_rx.recv().expect("Node observed request");
        assert_eq!(payload["routeId"], "synthetic");
        assert_eq!(payload["path"], "/dsh-market/synthetic");
        assert_eq!(payload["query"], "page=1");
        peer.join().expect("peer joins");
    }

    struct BlockingPnpmBoundary(Arc<Notify>);

    #[async_trait::async_trait]
    impl PnpmBoundary for BlockingPnpmBoundary {
        async fn run(
            &self,
            _: PnpmRunRequest,
            cancellation: CancellationToken,
            _: PnpmOutputSink,
        ) -> BridgeResult<PnpmRunResult> {
            self.0.notify_one();
            cancellation.cancelled().await;
            Ok(PnpmRunResult {
                exit_code: None,
                signal: Some("SIGTERM".into()),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pnpm_boundary_is_optional_generation_owned_and_cancellable() {
        let absent = bridge();
        attach(&absent, 1);
        let request = PnpmRunRequest {
            operation_id: "missing".into(),
            args: vec!["install".into()],
            invoking_dir: "/tmp/profile".into(),
        };
        assert_eq!(
            remote_code(
                absent
                    .pnpm_run(1, 7, request.clone())
                    .await
                    .expect_err("missing boundary is typed"),
            ),
            "PNPM_UNAVAILABLE"
        );

        let started = Arc::new(Notify::new());
        let tools = ToolRuntime::new();
        let sessions = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
        let agents = AgentRegistry::new(sessions.clone());
        let bridge = Arc::new(
            DomainBridge::new(
                BridgeServices::new(
                    tools,
                    SystemPrompt::new(),
                    LlmRuntime::new(),
                    sessions,
                    agents,
                )
                .with_pnpm_boundary(Arc::new(BlockingPnpmBoundary(Arc::clone(&started)))),
            )
            .expect("bridge constructs"),
        );
        attach(&bridge, 1);
        attach(&bridge, 2);
        let running = {
            let bridge = Arc::clone(&bridge);
            tokio::spawn(async move { bridge.pnpm_run(1, 9, request).await })
        };
        started.notified().await;
        assert!(!bridge
            .cancel_pnpm_request(2, 9)
            .expect("foreign generation observes"));
        assert_eq!(
            BridgeHandler::handle(&*bridge, Frame::cancel(1, 9))
                .expect("generic cancel dispatches"),
            json!({"cancelled": true})
        );
        assert_eq!(
            BridgeHandler::handle(&*bridge, Frame::cancel(1, 9))
                .expect("generic cancel is idempotent"),
            json!({"cancelled": false})
        );
        assert!(running.await.expect("runner task joins").is_ok());
    }
}

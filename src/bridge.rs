//! Typed, bounded product-domain dispatch for legacy Node and WASM plugins.
//!
//! This module deliberately exposes only the product services listed in
//! [`DomainRequest`]; it does not expose a generic `ContextHandle` lookup.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WasmServiceRequest {
    service: String,
    method: String,
    #[serde(default)]
    payload: Value,
}

/// Product authorization installed for one running WASM plugin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmEffectivePolicy {
    pub plugin_id: String,
    pub methods: BTreeSet<ServiceMethodPermission>,
}

impl WasmEffectivePolicy {
    pub fn new(
        plugin_id: impl Into<String>,
        methods: impl IntoIterator<Item = ServiceMethodPermission>,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
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
    policies: Mutex<BTreeMap<String, Arc<WasmPolicyEntry>>>,
}

struct WasmPolicyEntry {
    state: Mutex<WasmPolicyState>,
    drained: Condvar,
}

struct WasmPolicyState {
    methods: BTreeSet<ServiceMethodPermission>,
    revoked: bool,
    active: usize,
}

/// The sole policy owner for one plugin lifecycle. Dropping it revokes admission only.
pub struct WasmPolicyRegistration {
    plugin_id: String,
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
                methods: policy.methods,
                revoked: false,
                active: 0,
            }),
            drained: Condvar::new(),
        });
        let mut policies = lock(&self.inner.policies);
        if policies.contains_key(&policy.plugin_id) {
            return Err(policy_error(
                "PLUGIN_POLICY_ALREADY_REGISTERED",
                "a policy is already installed for this plugin",
            ));
        }
        policies.insert(policy.plugin_id.clone(), Arc::clone(&entry));
        Ok(WasmPolicyRegistration {
            plugin_id: policy.plugin_id,
            entry,
            registry: Arc::downgrade(&self.inner),
        })
    }

    /// Admits an exact service/method call and returns a guard held through dispatch.
    pub fn authorize(
        &self,
        plugin_id: &str,
        service: &str,
        method: &str,
    ) -> WasmResult<WasmPolicyLease> {
        let policies = lock(&self.inner.policies);
        let entry = policies.get(plugin_id).cloned().ok_or_else(|| {
            policy_error("PLUGIN_POLICY_NOT_FOUND", "plugin policy is not installed")
        })?;
        let mut state = lock(&entry.state);
        if state.revoked {
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
        drop(policies);
        Ok(WasmPolicyLease { entry })
    }
}

impl WasmPolicyRegistration {
    /// Synchronously rejects future calls. Existing leases remain valid until dropped.
    pub fn revoke(&self) -> bool {
        let mut revoked = false;
        if let Some(registry) = self.registry.upgrade() {
            let mut policies = lock(&registry.policies);
            if policies
                .get(&self.plugin_id)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
            {
                let mut state = lock(&self.entry.state);
                state.revoked = true;
                policies.remove(&self.plugin_id);
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
    retired_generations: Mutex<BTreeSet<u64>>,
}

struct GenerationState {
    client: BridgeClient,
    cancellation: CancellationToken,
    registrations: BTreeMap<String, NativeRegistration>,
    timers: BTreeMap<String, TimerEntry>,
    next_timer_token: u64,
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
            },
        );
        Ok(())
    }

    /// Cancels every callback and timer and drops all registration lifetime handles for `generation`.
    pub fn cleanup_generation(&self, generation: u64) {
        let state = lock(&self.inner.generations).remove(&generation);
        if let Some(state) = state {
            lock(&self.inner.retired_generations).insert(generation);
            cleanup_state(state);
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
        let value = self.block_on(async {
            tokio::time::timeout(self.inner.limits.request_timeout, self.handle_frame(frame))
                .await
                .map_err(|_| BridgeError::Timeout)?
        })?;
        self.validate_result(value, self.inner.limits.max_json_bytes)
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
        let wire: WasmServiceRequest = decode(request.payload).map_err(bridge_to_plugin_error)?;
        let domain = DomainRequest {
            service: wire.service,
            method: wire.method,
            params: wire.payload,
        };
        self.validate_request(&domain, self.inner.limits.max_json_bytes)
            .map_err(bridge_to_plugin_error)?;
        let _lease =
            self.inner
                .policy_registry
                .authorize(&plugin_id, &domain.service, &domain.method)?;
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

fn cleanup_state(mut state: GenerationState) {
    state.cancellation.cancel();
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

fn validate_policy(policy: &WasmEffectivePolicy) -> WasmResult<()> {
    if policy.plugin_id.trim().is_empty() {
        return Err(policy_error(
            "MANIFEST_PERMISSION_INVALID",
            "plugin id must not be blank",
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

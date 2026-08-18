//! Shared owner for the web and JSON-RPC transports.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tessivum_core::{ContextHandle, CoreError, EntryTree, Loader, PackageResolver, ServiceHandle};
use tessivum_extism::{Capability, CapabilityHandler, CapabilityRegistry, ResourceLimits};
use thiserror::Error;
use tokio::{
    sync::{broadcast, Mutex as AsyncMutex, Notify},
    task::JoinHandle,
};

use crate::{
    agent::{
        AgentError, AgentFactoryRegistration, AgentHandle, AgentOptions, AgentRegistry, AgentStatus,
    },
    agent_loop::AgentLoopFactory,
    approval::{
        ApprovalAsked, ApprovalDecision, ApprovalError, ApprovalNotification, ApprovalRequested,
        ApprovalResolved, ApprovalService, HostApprovalError, HostApprovalRegistration,
        HostApprovalRegistry,
    },
    bridge::{BridgeServices, DomainBridge, WasmPolicyRegistry},
    builtin_tools::{BuiltinTools, BuiltinToolsConfig},
    code_runtime::{CodeRuntime, ProcessCodeRuntime},
    credentials::{credentials_service_key, CredentialEvent, Credentials, YamlCredentialFile},
    legacy::{product_loader, LegacyProfile, ProductPackageResolver, WasmProductRuntime},
    llm::{LlmAdapter, LlmProviderRegistration, LlmRuntime, LlmStream, RecordedLlmAdapter},
    persistence_jsonl::JsonlSessionPersistence,
    protocol::{
        AgentCancelCause, InitializeParams, InitializeResult, Message, MessageId, MessageRole,
        MessageSource, SdkServerInfo, SessionEvent, SessionEventNotification, SessionHeader,
        SessionId, SessionPromptParams, SessionPromptResult, SessionStatus,
        SessionStatusNotification, SubagentFinishedNotification, SubagentStartedNotification,
        SESSION_FORMAT_VERSION,
    },
    session::{session_service_key, SessionError, SessionPersistence, SessionStore},
    settings::{settings_service_key, Settings, SettingsEvent, YamlSettingsProvider},
    subprocess::SubprocessRuntime,
    system_prompt::{PromptRegistration, PromptSection, SystemPrompt},
    telemetry::TelemetryCoordinator,
    tools::{ToolRestrictions, ToolRuntime},
    TessivumError,
};

const MAX_FRAME_BYTES: usize = 1_048_576;
const MAX_PROMPT_BLOCKS: usize = 128;
const MAX_PROFILE_BYTES: usize = 128;
const MAX_NOTIFICATIONS: usize = 4_096;
const MAX_LIVE_SESSIONS: usize = 1_024;

/// Deployment adapter construction for the one configured provider/model route.
/// A factory is called once at boot and must never retry durable work.
pub trait HostLlmAdapterFactory: Send + Sync {
    fn create(&self, provider: &str, model: &str) -> Result<Arc<dyn LlmAdapter>, TessivumError>;
}

/// Canonical path and profile identity attached to every host-owned session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostIdentity {
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
    pub profile: String,
}

/// Boot inputs. JSON patches are applied bundle → profile → home → CLI → telemetry.
#[derive(Clone)]
pub struct HostConfig {
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
    /// Host-selected writable settings file. `None` uses `data_dir/settings.yaml`.
    pub settings_path: Option<PathBuf>,
    /// Host-selected writable credentials file. `None` uses `data_dir/credentials.yaml`.
    pub credentials_path: Option<PathBuf>,
    pub profile: String,
    pub provider: String,
    pub model: String,
    pub max_tokens: Option<u64>,
    pub recorded_replay: Option<String>,
    pub adapter_factory: Option<Arc<dyn HostLlmAdapterFactory>>,
    pub bundle_patch: Value,
    pub profile_patch: Value,
    pub home_patch: Value,
    pub cli_patches: Vec<Value>,
    pub telemetry_patch: Value,
    pub system_prompt: Option<String>,
    pub enable_trusted_bash: bool,
    /// Host-selected tools requiring one Browser/local approval per call.
    pub approval_required_tools: BTreeSet<String>,
    pub notification_capacity: usize,
    pub max_live_sessions: usize,
    /// Entries use the Core Loader with the product WASM runtime and optional Legacy Node runtime.
    pub entries: Option<EntryTree>,
    pub legacy_profile: Option<LegacyProfile>,
    pub package_resolver: Option<Arc<dyn PackageResolver>>,
    pub wasm_limits: ResourceLimits,
    pub telemetry: Option<TelemetryCoordinator>,
    pub code_runtime: Option<ProcessCodeRuntime>,
}

impl std::fmt::Debug for HostConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostConfig")
            .field("cwd", &self.cwd)
            .field("data_dir", &self.data_dir)
            .field("settings_path", &self.settings_path)
            .field("credentials_path", &self.credentials_path)
            .field("profile", &self.profile)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("has_recording", &self.recorded_replay.is_some())
            .field("has_adapter_factory", &self.adapter_factory.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for HostConfig {
    fn default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(cwd.clone(), cwd.join(".tessivum"))
    }
}

impl HostConfig {
    pub fn new(cwd: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            data_dir: data_dir.into(),
            settings_path: None,
            credentials_path: None,
            profile: "default".into(),
            provider: "recorded".into(),
            model: "recorded".into(),
            max_tokens: None,
            recorded_replay: None,
            adapter_factory: None,
            bundle_patch: json!({}),
            profile_patch: json!({}),
            home_patch: json!({}),
            cli_patches: Vec::new(),
            telemetry_patch: json!({}),
            system_prompt: None,
            enable_trusted_bash: false,
            approval_required_tools: BTreeSet::new(),
            notification_capacity: 128,
            max_live_sessions: 128,
            entries: None,
            legacy_profile: None,
            package_resolver: None,
            wasm_limits: ResourceLimits::default(),
            telemetry: None,
            code_runtime: None,
        }
    }

    pub fn with_settings_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.settings_path = Some(path.into());
        self
    }

    pub fn with_credentials_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.credentials_path = Some(path.into());
        self
    }

    pub fn with_recorded_replay(mut self, replay: impl Into<String>) -> Self {
        self.recorded_replay = Some(replay.into());
        self
    }

    pub fn with_adapter_factory(mut self, factory: Arc<dyn HostLlmAdapterFactory>) -> Self {
        self.adapter_factory = Some(factory);
        self
    }

    pub fn with_approval_required_tool(mut self, tool: impl Into<String>) -> Self {
        self.approval_required_tools.insert(tool.into());
        self
    }

    pub fn with_cli_patch(mut self, patch: Value) -> Self {
        self.cli_patches.push(patch);
        self
    }

    pub fn compose_profile(&self) -> Result<Value, HostError> {
        validate_config(self)?;
        let mut result = Map::new();
        for patch in std::iter::once(&self.bundle_patch)
            .chain(std::iter::once(&self.profile_patch))
            .chain(std::iter::once(&self.home_patch))
            .chain(self.cli_patches.iter())
            .chain(std::iter::once(&self.telemetry_patch))
        {
            merge_object(
                &mut result,
                patch.as_object().expect("validated patch object"),
            );
        }
        Ok(Value::Object(result))
    }
}

/// Internal composition failures. The public [`HostApi`] boundary uses the
/// existing wire-stable [`TessivumError`] envelope.
#[derive(Debug, Error)]
pub enum HostError {
    #[error("invalid host configuration: {0}")]
    InvalidConfiguration(String),
    #[error("cannot canonicalize {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot create host data directory {path}: {source}")]
    CreateDataDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("host is shutting down")]
    ShuttingDown,
    #[error("host initialization conflicts with the boot route")]
    InitializationConflict,
    #[error("host live-session capacity is exhausted")]
    SessionCapacity,
    #[error("host shutdown failed: {0}")]
    Shutdown(String),
    #[error(transparent)]
    Runtime(#[from] TessivumError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    Approval(#[from] ApprovalError),
    #[error(transparent)]
    ApprovalRegistry(#[from] HostApprovalError),
}

impl HostError {
    fn invalid(code: &'static str, message: impl Into<String>) -> Self {
        Self::Runtime(TessivumError::new(code, message, "host", Value::Null))
    }

    pub fn code(&self) -> &str {
        match self {
            Self::InvalidConfiguration(_) => "INVALID_HOST_CONFIG",
            Self::Canonicalize { .. } => "INVALID_HOST_PATH",
            Self::CreateDataDir { .. } => "HOST_DATA_DIR_CREATE_FAILED",
            Self::ShuttingDown => "HOST_SHUTTING_DOWN",
            Self::InitializationConflict => "HOST_INITIALIZATION_CONFLICT",
            Self::SessionCapacity => "HOST_SESSION_CAPACITY",
            Self::Shutdown(_) => "HOST_SHUTDOWN_FAILED",
            Self::Runtime(error) => &error.code,
            Self::Session(error) => error.code(),
            Self::Agent(AgentError::Cancelled) => "CANCELLED",
            Self::Agent(_) => "HOST_AGENT_ERROR",
            Self::Core(_) => "HOST_CORE_ERROR",
            Self::Approval(_) | Self::ApprovalRegistry(_) => "HOST_APPROVAL_ERROR",
        }
    }

    fn wire(self) -> TessivumError {
        match self {
            Self::Runtime(error) => error,
            error => {
                let code = error.code().to_owned();
                TessivumError::new(code, error.to_string(), "host", Value::Null)
            }
        }
    }
}

/// Facts emitted only after session admission or a status transition.
#[derive(Clone, Debug)]
pub enum HostNotification {
    SessionEvent(SessionEventNotification),
    SessionStatus(SessionStatusNotification),
    SubagentStarted(SubagentStartedNotification),
    SubagentFinished(SubagentFinishedNotification),
    ApprovalRequested(ApprovalRequested),
    ApprovalResolved(ApprovalResolved),
    SettingsChanged(SettingsEvent),
    CredentialsChanged(CredentialEvent),
}

/// Durable session metadata needed by reconnecting transports without replaying logs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSessionInfo {
    pub session_id: SessionId,
    pub created_at: u64,
    pub cwd: Option<String>,
    pub event_count: u64,
}

/// Immutable identity and model route exposed to transports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostDescriptor {
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub max_tokens: Option<u64>,
}

/// One object-safe host contract for API and SDK transports.
#[async_trait]
pub trait HostApi: Send + Sync {
    async fn initialize(&self, params: InitializeParams)
        -> Result<InitializeResult, TessivumError>;
    async fn prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError>;
    async fn steer(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.prompt(params).await
    }
    async fn cancel(
        &self,
        session: SessionId,
        cause: AgentCancelCause,
    ) -> Result<bool, TessivumError>;
    async fn events(
        &self,
        session: SessionId,
        from_seq: u64,
    ) -> Result<Vec<SessionEvent>, TessivumError>;
    async fn status(&self, session: SessionId) -> Result<Option<SessionStatus>, TessivumError>;
    async fn create_session(
        &self,
        session_id: SessionId,
    ) -> Result<HostSessionInfo, TessivumError> {
        Ok(HostSessionInfo {
            session_id,
            created_at: 0,
            cwd: None,
            event_count: 0,
        })
    }
    async fn list_sessions(&self) -> Result<Vec<HostSessionInfo>, TessivumError> {
        Ok(Vec::new())
    }
    fn descriptor(&self) -> HostDescriptor {
        HostDescriptor {
            cwd: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .to_string_lossy()
                .into_owned(),
            provider: "recorded".into(),
            model: "recorded".into(),
            max_tokens: None,
        }
    }
    fn settings(&self) -> Option<Arc<Settings>> {
        None
    }
    fn credentials(&self) -> Option<Arc<Credentials>> {
        None
    }
    fn subscribe(&self) -> broadcast::Receiver<HostNotification>;
    fn approval_registry(&self) -> Option<HostApprovalRegistry> {
        None
    }
    async fn shutdown(&self) -> Result<(), TessivumError>;
}

/// Owns services and their graceful shutdown order.
pub struct HostRuntime {
    handle: HostHandle,
}

/// Transport-safe host handle with one admission fence shared by all clones.
#[derive(Clone)]
pub struct HostHandle {
    inner: Arc<HostInner>,
}

struct HostInner {
    identity: HostIdentity,
    profile: Value,
    config: HostConfig,
    settings: Arc<Settings>,
    credentials: Arc<Credentials>,
    cancellation: tessivum_core::CancellationToken,
    sessions: SessionStore,
    persistence: Arc<dyn SessionPersistence>,
    registry: AgentRegistry,
    approvals: HostApprovalRegistry,
    telemetry: Option<TelemetryCoordinator>,
    code_runtime: Option<ProcessCodeRuntime>,
    subprocesses: SubprocessRuntime,
    legacy: Option<LegacyProfile>,
    loader: AsyncMutex<Option<Loader>>,
    services: Services,
    owned_agents: Mutex<BTreeMap<SessionId, OwnedAgent>>,
    state: Mutex<State>,
    // ponytail: one Host-wide generation gate; shard by session only if prompt churn contends.
    setup: AsyncMutex<()>,
    admission: Mutex<AdmissionState>,
    drained: Notify,
    shutdown: AsyncMutex<()>,
    notices: broadcast::Sender<HostNotification>,
    relay_stop: Notify,
    relays_closed: AtomicBool,
    relays: Mutex<Vec<JoinHandle<()>>>,
}

struct OwnedAgent {
    _approval: HostApprovalRegistration,
    _agent: AgentHandle,
}

struct Services {
    root: ContextHandle,
    _sessions: ServiceHandle<SessionStore>,
    _llm: ServiceHandle<LlmRuntime>,
    _prompt: ServiceHandle<SystemPrompt>,
    _tools: ServiceHandle<ToolRuntime>,
    _agents: ServiceHandle<AgentRegistry>,
    _subprocesses: ServiceHandle<SubprocessRuntime>,
    _settings: ServiceHandle<Arc<Settings>>,
    _credentials: ServiceHandle<Arc<Credentials>>,
    _telemetry: Option<ServiceHandle<TelemetryCoordinator>>,
    _code: Option<ServiceHandle<ProcessCodeRuntime>>,
    _provider: LlmProviderRegistration,
    _prompt_registration: Option<PromptRegistration>,
    _builtin_tools: BuiltinTools,
    _factory: AgentFactoryRegistration,
}

#[derive(Default)]
struct State {
    initialized: Option<InitializeParams>,
    statuses: BTreeMap<SessionId, SessionStatus>,
    relayed: BTreeSet<SessionId>,
}

#[derive(Default)]
struct AdmissionState {
    closing: bool,
    count: usize,
}

struct Admission(Arc<HostInner>);

impl Drop for Admission {
    fn drop(&mut self) {
        let mut state = lock(&self.0.admission);
        state.count = state.count.saturating_sub(1);
        if state.closing && state.count == 0 {
            self.0.drained.notify_waiters();
        }
    }
}

impl HostRuntime {
    pub async fn boot(config: HostConfig) -> Result<Self, HostError> {
        let profile = config.compose_profile()?;
        let cwd = config
            .cwd
            .canonicalize()
            .map_err(|source| HostError::Canonicalize {
                path: config.cwd.clone(),
                source,
            })?;
        if !cwd.is_dir() {
            return Err(HostError::InvalidConfiguration(
                "cwd is not a directory".into(),
            ));
        }
        tokio::fs::create_dir_all(&config.data_dir)
            .await
            .map_err(|source| HostError::CreateDataDir {
                path: config.data_dir.clone(),
                source,
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&config.data_dir, std::fs::Permissions::from_mode(0o700))
                .await
                .map_err(|source| HostError::CreateDataDir {
                    path: config.data_dir.clone(),
                    source,
                })?;
        }
        let data_dir =
            config
                .data_dir
                .canonicalize()
                .map_err(|source| HostError::Canonicalize {
                    path: config.data_dir.clone(),
                    source,
                })?;
        if !data_dir.is_dir() {
            return Err(HostError::InvalidConfiguration(
                "data_dir is not a directory".into(),
            ));
        }

        let settings_path = host_file_path(
            &data_dir,
            config.settings_path.as_deref(),
            "settings.yaml",
            "settings_path",
        )?;
        let credentials_path = host_file_path(
            &data_dir,
            config.credentials_path.as_deref(),
            "credentials.yaml",
            "credentials_path",
        )?;

        let root = ContextHandle::root();
        let cancellation = root.scope().cancellation();
        let settings = Arc::new(Settings::new(Arc::new(YamlSettingsProvider::new(
            settings_path,
        ))));
        let settings_service = root.provide(settings_service_key(), Arc::clone(&settings))?;
        let credentials = Arc::new(Credentials::new(Arc::new(YamlCredentialFile::new(
            credentials_path,
        ))));
        let credentials_service =
            root.provide(credentials_service_key(), Arc::clone(&credentials))?;
        let persistence: Arc<dyn SessionPersistence> =
            Arc::new(JsonlSessionPersistence::new(&data_dir));
        let sessions = SessionStore::new(Arc::clone(&persistence));
        let session_service = root.provide(session_service_key(), sessions.clone())?;

        let llm = LlmRuntime::new();
        let provider = llm.register(config.provider.clone(), adapter_for(&config)?)?;
        let llm_service = llm.clone().publish(&root)?;
        let prompt = SystemPrompt::new();
        let prompt_registration = config
            .system_prompt
            .as_ref()
            .map(|text| prompt.register(PromptSection::new("host", 0, text.clone())))
            .transpose()?;
        let prompt_service = prompt.clone().publish(&root)?;
        let tools = ToolRuntime::new();
        let approvals = HostApprovalRegistry::new();
        tools.set_approval(Some(Arc::new(approvals.clone())));
        let builtin_tools = BuiltinTools::new(
            &tools,
            BuiltinToolsConfig {
                enable_bash: config.enable_trusted_bash,
                cwd: cwd.clone(),
                ..BuiltinToolsConfig::default()
            },
        )?;
        let agent_tools = if config.approval_required_tools.is_empty() {
            tools.clone()
        } else {
            let restrictions = config
                .approval_required_tools
                .iter()
                .cloned()
                .fold(ToolRestrictions::new(), ToolRestrictions::ask);
            tools.scoped(restrictions)?
        };
        let tools_service = tools.publish(&root)?;
        let registry = AgentRegistry::new(sessions.clone());
        let factory = registry.register_factory(Arc::new(AgentLoopFactory::new(
            llm.clone(),
            prompt.clone(),
            agent_tools,
        )))?;
        let agents_service = registry.clone().publish(&root)?;
        let subprocesses = SubprocessRuntime::new();
        let subprocess_service = subprocesses.publish(&root)?;
        let telemetry_service = config
            .telemetry
            .as_ref()
            .map(|value| value.clone().publish(&root))
            .transpose()?;
        let code_service = config
            .code_runtime
            .as_ref()
            .map(|value| value.clone().publish(&root))
            .transpose()?;

        let needs_legacy = config.entries.as_ref().is_some_and(|entries| {
            entries
                .active_entries()
                .iter()
                .any(|entry| entry.options.runtime == tessivum_core::RuntimeKind::LegacyNode)
        });
        let legacy = config.legacy_profile.clone().filter(|_| needs_legacy);
        let loader = if let Some(entries) = config.entries.clone() {
            let resolver: Arc<dyn PackageResolver> = match &config.package_resolver {
                Some(value) => Arc::clone(value),
                None => Arc::new(
                    ProductPackageResolver::new()
                        .confine_to(&cwd)
                        .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?,
                ),
            };
            if let Some(profile) = &legacy {
                profile
                    .start()
                    .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
            }
            let policies = WasmPolicyRegistry::new();
            let capabilities = Arc::new(CapabilityRegistry::new());
            let wasm_bridge = DomainBridge::with_policy_registry(
                BridgeServices::new(
                    tools,
                    prompt.clone(),
                    llm.clone(),
                    sessions.clone(),
                    registry.clone(),
                )
                .with_settings(Arc::clone(&settings))
                .with_credentials(Arc::clone(&credentials)),
                policies.clone(),
            )
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
            capabilities
                .register(Capability::ServiceCall, move |request| {
                    CapabilityHandler::call(&wasm_bridge, request)
                })
                .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
            capabilities.grant(Capability::ServiceCall);
            let wasm = Arc::new(WasmProductRuntime::new(
                capabilities,
                policies,
                config.wasm_limits.clone(),
            ));
            let mut loader = match product_loader(legacy.as_ref(), resolver, wasm) {
                Ok(loader) => loader.with_context(root.clone()),
                Err(error) => {
                    if let Some(profile) = &legacy {
                        let _ = profile.shutdown().await;
                    }
                    let _ = root.scope().dispose().await;
                    return Err(HostError::InvalidConfiguration(error.to_string()));
                }
            };
            if let Err(error) = loader.load(entries).await {
                if let Some(profile) = &legacy {
                    let _ = profile.shutdown().await;
                }
                let _ = root.scope().dispose().await;
                return Err(HostError::InvalidConfiguration(format!(
                    "Core Loader activation failed: {error}"
                )));
            }
            Some(loader)
        } else {
            None
        };

        let (notices, _) = broadcast::channel(config.notification_capacity);
        let inner = Arc::new(HostInner {
            identity: HostIdentity {
                cwd,
                data_dir,
                profile: config.profile.clone(),
            },
            profile,
            config: config.clone(),
            settings,
            credentials,
            cancellation,
            sessions,
            persistence,
            registry,
            approvals,
            telemetry: config.telemetry.clone(),
            code_runtime: config.code_runtime.clone(),
            subprocesses,
            legacy,
            loader: AsyncMutex::new(loader),
            services: Services {
                root,
                _sessions: session_service,
                _llm: llm_service,
                _prompt: prompt_service,
                _tools: tools_service,
                _agents: agents_service,
                _subprocesses: subprocess_service,
                _settings: settings_service,
                _credentials: credentials_service,
                _telemetry: telemetry_service,
                _code: code_service,
                _provider: provider,
                _prompt_registration: prompt_registration,
                _builtin_tools: builtin_tools,
                _factory: factory,
            },
            owned_agents: Mutex::new(BTreeMap::new()),
            state: Mutex::new(State::default()),
            setup: AsyncMutex::new(()),
            admission: Mutex::new(AdmissionState::default()),
            drained: Notify::new(),
            shutdown: AsyncMutex::new(()),
            notices,
            relay_stop: Notify::new(),
            relays_closed: AtomicBool::new(false),
            relays: Mutex::new(Vec::new()),
        });
        HostHandle::start_service_relays(&inner);
        let handle = HostHandle { inner };
        handle.start_approval_relay();
        Ok(Self { handle })
    }

    pub fn handle(&self) -> HostHandle {
        self.handle.clone()
    }
    pub fn identity(&self) -> &HostIdentity {
        self.handle.identity()
    }
    pub fn profile(&self) -> &Value {
        self.handle.profile()
    }
    pub async fn shutdown(&self) -> Result<(), HostError> {
        self.handle.shutdown_inner().await
    }
}

impl HostHandle {
    pub fn identity(&self) -> &HostIdentity {
        &self.inner.identity
    }
    pub fn profile(&self) -> &Value {
        &self.inner.profile
    }
    pub fn in_flight(&self) -> usize {
        lock(&self.inner.admission).count
    }
    pub fn is_shutting_down(&self) -> bool {
        lock(&self.inner.admission).closing
    }

    async fn initialize_inner(
        &self,
        mut params: InitializeParams,
    ) -> Result<InitializeResult, HostError> {
        let _admission = self.admit()?;
        params.validate()?;
        if params.provider.trim().is_empty() || params.model.trim().is_empty() {
            return Err(HostError::invalid(
                "INVALID_INITIALIZE_PARAMS",
                "provider and model must not be blank",
            ));
        }
        let cwd =
            Path::new(&params.cwd)
                .canonicalize()
                .map_err(|source| HostError::Canonicalize {
                    path: PathBuf::from(&params.cwd),
                    source,
                })?;
        if cwd != self.inner.identity.cwd
            || params.provider != self.inner.config.provider
            || params.model != self.inner.config.model
            || params.max_tokens != self.inner.config.max_tokens
        {
            return Err(HostError::InitializationConflict);
        }
        params.cwd = cwd.to_string_lossy().into_owned();
        let mut state = lock(&self.inner.state);
        if state
            .initialized
            .as_ref()
            .is_some_and(|current| current != &params)
        {
            return Err(HostError::InitializationConflict);
        }
        state.initialized = Some(params);
        Ok(InitializeResult {
            server_info: SdkServerInfo {
                name: "deepseek-harness-sdk-runtime".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        })
    }

    async fn create_session_inner(
        &self,
        session_id: SessionId,
    ) -> Result<HostSessionInfo, HostError> {
        let _admission = self.admit()?;
        validate_session(&session_id)?;
        if let Some(session) = self.inner.sessions.get(&session_id) {
            let header = session.header();
            return Ok(HostSessionInfo {
                session_id,
                created_at: header.created_at,
                cwd: header.cwd,
                event_count: session.events().len() as u64,
            });
        }
        if let Some(session) = self
            .inner
            .persistence
            .inspect(&session_id, self.inner.cancellation.clone())
            .await?
        {
            return Ok(HostSessionInfo {
                session_id: session.header.id,
                created_at: session.header.created_at,
                cwd: session.header.cwd,
                event_count: session.event_count,
            });
        }
        let created_at = now();
        self.inner
            .sessions
            .create(
                SessionHeader {
                    version: SESSION_FORMAT_VERSION,
                    id: session_id.clone(),
                    created_at,
                    cwd: Some(self.inner.identity.cwd.to_string_lossy().into_owned()),
                    parent_session: None,
                    seed_length: None,
                    origin: None,
                    delegation_depth: Some(0),
                    agent_preset: Some(self.inner.identity.profile.clone()),
                },
                self.inner.cancellation.clone(),
            )
            .await?;
        Ok(HostSessionInfo {
            session_id,
            created_at,
            cwd: Some(self.inner.identity.cwd.to_string_lossy().into_owned()),
            event_count: 0,
        })
    }

    async fn prompt_inner(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, HostError> {
        self.send_inner(params, false).await
    }

    async fn steer_inner(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, HostError> {
        self.send_inner(params, true).await
    }

    async fn send_inner(
        &self,
        params: SessionPromptParams,
        steer: bool,
    ) -> Result<SessionPromptResult, HostError> {
        let _admission = self.admit()?;
        validate_prompt(&params)?;
        let agent = self.ensure_agent(&params.session_id).await?;
        let session = agent.session();
        let mut commits = session.subscribe();
        let message_id = MessageId::random();
        let message = Message {
            id: message_id.clone(),
            role: MessageRole::User,
            content: params.content_blocks,
            source: MessageSource::User,
        };
        if steer {
            agent.steer(message).await?;
        } else {
            agent.followup(message).await?;
        }
        self.transition(params.session_id.clone(), SessionStatus::Running);
        let idle_agent =
            self.inner.registry.get(&params.session_id).ok_or_else(|| {
                HostError::InvalidConfiguration("accepted agent disappeared".into())
            })?;
        self.watch_idle(params.session_id, idle_agent);
        if !session.events().iter().any(|event| {
            event.event_type == "user/message"
                && event.data.get("id").and_then(Value::as_str) == Some(message_id.as_str())
        }) {
            loop {
                tokio::select! { received = commits.recv() => match received { Ok(event) if event.event_type == "user/message" && event.data.get("id").and_then(Value::as_str) == Some(message_id.as_str()) => break, Ok(_) => continue, Err(broadcast::error::RecvError::Lagged(_)) => { if session.events().iter().any(|event| event.event_type == "user/message" && event.data.get("id").and_then(Value::as_str) == Some(message_id.as_str())) { break; } }, Err(broadcast::error::RecvError::Closed) => return Err(HostError::invalid("PROMPT_NOT_DURABLE", "agent session closed before prompt admission")), }, result = agent.when_idle() => { if session.events().iter().any(|event| event.event_type == "user/message" && event.data.get("id").and_then(Value::as_str) == Some(message_id.as_str())) { break; } return Err(result.err().unwrap_or(AgentError::Disposed).into()); } }
            }
        }
        Ok(SessionPromptResult { message_id })
    }

    async fn cancel_inner(
        &self,
        session: SessionId,
        cause: AgentCancelCause,
    ) -> Result<bool, HostError> {
        self.inner.approvals.cancel_session(&session);
        let _setup = self.inner.setup.lock().await;
        let agent = self.inner.registry.get(&session);
        let cancelled = match self.inner.registry.cancel(&session, cause, false) {
            Ok(value) => value,
            Err(AgentError::NotFound(_)) => false,
            Err(error) => return Err(error.into()),
        };
        if cancelled {
            let owned = lock(&self.inner.owned_agents).remove(&session);
            drop(owned);
            if let Some(agent) = agent {
                agent.dispose().await?;
            }
            self.transition(session, SessionStatus::Idle);
        }
        Ok(cancelled)
    }

    async fn events_inner(
        &self,
        session: SessionId,
        from_seq: u64,
    ) -> Result<Vec<SessionEvent>, HostError> {
        validate_session(&session)?;
        if let Some(live) = self.inner.sessions.get(&session) {
            return Ok(live
                .events()
                .into_iter()
                .filter(|event| event.seq >= from_seq)
                .collect());
        }
        Ok(self
            .inner
            .persistence
            .read_from(&session, from_seq, self.inner.cancellation.clone())
            .await?)
    }

    async fn status_inner(&self, session: SessionId) -> Result<Option<SessionStatus>, HostError> {
        validate_session(&session)?;
        if let Some(status) = lock(&self.inner.state).statuses.get(&session).copied() {
            return Ok(Some(status));
        }
        if let Some(agent) = self.inner.registry.get(&session) {
            return Ok(Some(match agent.status() {
                AgentStatus::Idle => SessionStatus::Idle,
                AgentStatus::Running => SessionStatus::Running,
            }));
        }
        Ok(self
            .inner
            .persistence
            .inspect(&session, self.inner.cancellation.clone())
            .await?
            .map(|_| SessionStatus::Idle))
    }

    async fn shutdown_inner(&self) -> Result<(), HostError> {
        let _shutdown = self.inner.shutdown.lock().await;
        let already_closing = {
            let mut state = lock(&self.inner.admission);
            let already = state.closing;
            if !already {
                state.closing = true;
                if state.count == 0 {
                    self.inner.drained.notify_waiters();
                }
            }
            already
        };
        if already_closing {
            self.wait_drained().await;
            return Ok(());
        }
        self.inner.approvals.cancel_all();
        self.inner.registry.cancel_all(
            AgentCancelCause::Hook {
                reason: "host shutdown".into(),
            },
            false,
        );
        self.wait_drained().await;
        let mut failures = Vec::new();
        if let Err(error) = self.inner.registry.dispose_all().await {
            failures.push(format!("agents: {error}"));
        }
        let owned_agents = std::mem::take(&mut *lock(&self.inner.owned_agents));
        drop(owned_agents);
        if let Some(code) = &self.inner.code_runtime {
            if let Err(error) = code.dispose().await {
                failures.push(format!("code: {error}"));
            }
        }
        self.inner.subprocesses.shutdown().await;
        for session in self.inner.sessions.list() {
            if let Err(error) = session.flush(self.inner.cancellation.clone()).await {
                failures.push(format!("session {}: {error}", session.id()));
            }
        }
        self.stop_relays().await;
        if let Some(telemetry) = &self.inner.telemetry {
            telemetry
                .shutdown(
                    self.inner
                        .sessions
                        .list()
                        .into_iter()
                        .map(|session| session.id()),
                )
                .await;
        }
        let loader = { self.inner.loader.lock().await.take() };
        if let Some(mut loader) = loader {
            let unloaded = tokio::task::spawn_blocking(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime.block_on(async move {
                    loader
                        .replace(EntryTree::default())
                        .await
                        .map_err(|error| error.to_string())
                })
            })
            .await;
            match unloaded {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("loader: {error}")),
                Err(error) => failures.push(format!("loader task: {error}")),
            }
        }
        if let Some(legacy) = &self.inner.legacy {
            if let Err(error) = legacy.shutdown().await {
                failures.push(format!("legacy: {error}"));
            }
        }
        self.inner.settings.shutdown().await;
        self.inner.credentials.shutdown().await;
        self.inner.cancellation.cancel();
        if let Err(error) = self.inner.services.root.scope().dispose().await {
            failures.push(format!("root: {error}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(HostError::Shutdown(failures.join("; ")))
        }
    }

    fn admit(&self) -> Result<Admission, HostError> {
        let mut state = lock(&self.inner.admission);
        if state.closing {
            return Err(HostError::ShuttingDown);
        }
        state.count = state
            .count
            .checked_add(1)
            .ok_or_else(|| HostError::InvalidConfiguration("admission count overflow".into()))?;
        Ok(Admission(Arc::clone(&self.inner)))
    }

    async fn wait_drained(&self) {
        loop {
            let notified = self.inner.drained.notified();
            if lock(&self.inner.admission).count == 0 {
                return;
            }
            notified.await;
        }
    }

    async fn ensure_agent(&self, session_id: &SessionId) -> Result<AgentHandle, HostError> {
        let _setup = self.inner.setup.lock().await;
        if let Some(agent) = self.inner.registry.get(session_id) {
            return Ok(agent);
        }
        if lock(&self.inner.owned_agents).len() >= self.inner.config.max_live_sessions {
            return Err(HostError::SessionCapacity);
        }
        let owned = self
            .inner
            .registry
            .create_or_resume(
                SessionHeader {
                    version: SESSION_FORMAT_VERSION,
                    id: session_id.clone(),
                    created_at: now(),
                    cwd: Some(self.inner.identity.cwd.to_string_lossy().into_owned()),
                    parent_session: None,
                    seed_length: None,
                    origin: None,
                    delegation_depth: Some(0),
                    agent_preset: Some(self.inner.identity.profile.clone()),
                },
                AgentOptions {
                    provider: self.inner.config.provider.clone(),
                    model: self.inner.config.model.clone(),
                    max_tokens: self.inner.config.max_tokens,
                },
                self.inner.cancellation.clone(),
            )
            .await?;
        let observer = match self.inner.registry.get(session_id) {
            Some(agent) => agent,
            None => {
                let _ = owned.dispose().await;
                return Err(HostError::InvalidConfiguration(
                    "created agent was not published".into(),
                ));
            }
        };
        let authority = owned.authority();
        let approvals = match ApprovalService::new(observer) {
            Ok(approvals) => approvals,
            Err(error) => {
                let _ = owned.dispose().await;
                return Err(error.into());
            }
        };
        let approval = match self.inner.approvals.install(&authority, approvals) {
            Ok(approval) => approval,
            Err(error) => {
                let _ = owned.dispose().await;
                return Err(error.into());
            }
        };
        let session = owned.session();
        lock(&self.inner.owned_agents).insert(
            session_id.clone(),
            OwnedAgent {
                _approval: approval,
                _agent: owned,
            },
        );
        self.ensure_relay(session);
        self.transition(session_id.clone(), SessionStatus::Idle);
        self.inner.registry.get(session_id).ok_or_else(|| {
            HostError::InvalidConfiguration("created agent was not published".into())
        })
    }

    fn start_service_relays(inner: &Arc<HostInner>) {
        let mut settings_events = inner.settings.subscribe();
        let settings_inner = Arc::clone(inner);
        let settings_relay = tokio::spawn(async move {
            loop {
                if settings_inner.relays_closed.load(Ordering::Acquire) {
                    break;
                }
                tokio::select! {
                    event = settings_events.recv() => match event {
                        Ok(event) => { let _ = settings_inner.notices.send(HostNotification::SettingsChanged(event)); }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = settings_inner.relay_stop.notified() => break,
                }
            }
        });
        let mut credential_events = inner.credentials.subscribe();
        let credentials_inner = Arc::clone(inner);
        let credentials_relay = tokio::spawn(async move {
            loop {
                if credentials_inner.relays_closed.load(Ordering::Acquire) {
                    break;
                }
                tokio::select! {
                    event = credential_events.recv() => match event {
                        Ok(event) => { let _ = credentials_inner.notices.send(HostNotification::CredentialsChanged(event)); }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = credentials_inner.relay_stop.notified() => break,
                }
            }
        });
        lock(&inner.relays).extend([settings_relay, credentials_relay]);
    }

    fn ensure_relay(&self, session: Arc<crate::session::Session>) {
        let session_id = session.id();
        if !lock(&self.inner.state).relayed.insert(session_id.clone()) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let mut receiver = session.subscribe();
        let task = tokio::spawn(async move {
            let mut next_seq = session
                .events()
                .last()
                .map_or(0, |event| event.seq.saturating_add(1));
            loop {
                if inner.relays_closed.load(Ordering::Acquire) {
                    break;
                }
                tokio::select! {
                    event = receiver.recv() => match event {
                        Ok(event) => {
                            if event.seq > next_seq {
                                relay_missing_events(&inner, &session, &session_id, &mut next_seq);
                            }
                            if event.seq >= next_seq {
                                next_seq = event.seq.saturating_add(1);
                                relay_event(&inner, &session_id, event);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            relay_missing_events(&inner, &session, &session_id, &mut next_seq);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = inner.relay_stop.notified() => break,
                }
            }
            relay_missing_events(&inner, &session, &session_id, &mut next_seq);
        });
        lock(&self.inner.relays).push(task);
    }

    fn start_approval_relay(&self) {
        let inner = Arc::clone(&self.inner);
        let mut receiver = inner.approvals.subscribe();
        let task = tokio::spawn(async move {
            loop {
                if inner.relays_closed.load(Ordering::Acquire) {
                    break;
                }
                tokio::select! {
                    notice = receiver.recv() => match notice {
                        Ok(ApprovalNotification::Requested(notice)) => {
                            let _ = inner.notices.send(HostNotification::ApprovalRequested(notice));
                        }
                        Ok(ApprovalNotification::Resolved(notice)) => {
                            let _ = inner.notices.send(HostNotification::ApprovalResolved(notice));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = inner.relay_stop.notified() => break,
                }
            }
        });
        lock(&self.inner.relays).push(task);
    }

    fn transition(&self, session_id: SessionId, status: SessionStatus) {
        let changed = {
            let mut state = lock(&self.inner.state);
            if state.statuses.get(&session_id) == Some(&status) {
                false
            } else {
                state.statuses.insert(session_id.clone(), status);
                true
            }
        };
        if changed {
            let _ = self.inner.notices.send(HostNotification::SessionStatus(
                SessionStatusNotification { session_id, status },
            ));
        }
    }

    fn watch_idle(&self, session_id: SessionId, agent: AgentHandle) {
        let host = self.clone();
        tokio::spawn(async move {
            let _ = agent.when_idle().await;
            if !host.is_shutting_down() {
                host.transition(session_id, SessionStatus::Idle);
            }
        });
    }

    async fn stop_relays(&self) {
        self.inner.relays_closed.store(true, Ordering::Release);
        self.inner.relay_stop.notify_waiters();
        let relays = std::mem::take(&mut *lock(&self.inner.relays));
        for relay in relays {
            let _ = relay.await;
        }
    }
}

#[async_trait]
impl HostApi for HostHandle {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult, TessivumError> {
        self.initialize_inner(params).await.map_err(HostError::wire)
    }
    async fn prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.prompt_inner(params).await.map_err(HostError::wire)
    }
    async fn steer(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.steer_inner(params).await.map_err(HostError::wire)
    }
    async fn cancel(
        &self,
        session: SessionId,
        cause: AgentCancelCause,
    ) -> Result<bool, TessivumError> {
        self.cancel_inner(session, cause)
            .await
            .map_err(HostError::wire)
    }
    async fn events(
        &self,
        session: SessionId,
        from_seq: u64,
    ) -> Result<Vec<SessionEvent>, TessivumError> {
        self.events_inner(session, from_seq)
            .await
            .map_err(HostError::wire)
    }
    async fn status(&self, session: SessionId) -> Result<Option<SessionStatus>, TessivumError> {
        self.status_inner(session).await.map_err(HostError::wire)
    }
    async fn create_session(
        &self,
        session_id: SessionId,
    ) -> Result<HostSessionInfo, TessivumError> {
        self.create_session_inner(session_id)
            .await
            .map_err(HostError::wire)
    }
    async fn list_sessions(&self) -> Result<Vec<HostSessionInfo>, TessivumError> {
        self.inner
            .persistence
            .list(self.inner.cancellation.clone())
            .await
            .map(|sessions| {
                sessions
                    .into_iter()
                    .map(|session| HostSessionInfo {
                        session_id: session.header.id,
                        created_at: session.header.created_at,
                        cwd: session.header.cwd,
                        event_count: session.event_count,
                    })
                    .collect()
            })
            .map_err(HostError::from)
            .map_err(HostError::wire)
    }
    fn descriptor(&self) -> HostDescriptor {
        HostDescriptor {
            cwd: self.inner.identity.cwd.to_string_lossy().into_owned(),
            provider: self.inner.config.provider.clone(),
            model: self.inner.config.model.clone(),
            max_tokens: self.inner.config.max_tokens,
        }
    }
    fn approval_registry(&self) -> Option<HostApprovalRegistry> {
        Some(self.inner.approvals.clone())
    }
    fn settings(&self) -> Option<Arc<Settings>> {
        Some(Arc::clone(&self.inner.settings))
    }
    fn credentials(&self) -> Option<Arc<Credentials>> {
        Some(Arc::clone(&self.inner.credentials))
    }
    fn subscribe(&self) -> broadcast::Receiver<HostNotification> {
        self.inner.notices.subscribe()
    }
    async fn shutdown(&self) -> Result<(), TessivumError> {
        self.shutdown_inner().await.map_err(HostError::wire)
    }
}

#[async_trait]
impl HostApi for HostRuntime {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult, TessivumError> {
        self.handle.initialize(params).await
    }
    async fn prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.handle.prompt(params).await
    }
    async fn steer(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.handle.steer(params).await
    }
    async fn cancel(
        &self,
        session: SessionId,
        cause: AgentCancelCause,
    ) -> Result<bool, TessivumError> {
        self.handle.cancel(session, cause).await
    }
    async fn events(
        &self,
        session: SessionId,
        from_seq: u64,
    ) -> Result<Vec<SessionEvent>, TessivumError> {
        self.handle.events(session, from_seq).await
    }
    async fn status(&self, session: SessionId) -> Result<Option<SessionStatus>, TessivumError> {
        self.handle.status(session).await
    }
    async fn create_session(
        &self,
        session_id: SessionId,
    ) -> Result<HostSessionInfo, TessivumError> {
        self.handle.create_session(session_id).await
    }
    async fn list_sessions(&self) -> Result<Vec<HostSessionInfo>, TessivumError> {
        self.handle.list_sessions().await
    }
    fn descriptor(&self) -> HostDescriptor {
        self.handle.descriptor()
    }
    fn approval_registry(&self) -> Option<HostApprovalRegistry> {
        self.handle.approval_registry()
    }
    fn settings(&self) -> Option<Arc<Settings>> {
        self.handle.settings()
    }
    fn credentials(&self) -> Option<Arc<Credentials>> {
        self.handle.credentials()
    }
    fn subscribe(&self) -> broadcast::Receiver<HostNotification> {
        self.handle.subscribe()
    }
    async fn shutdown(&self) -> Result<(), TessivumError> {
        self.handle.shutdown().await
    }
}

/// SIGTERM is a graceful success; SIGINT remains shell cancellation (130).
pub async fn shutdown_signal() -> Result<i32, std::io::Error> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map(|()| 130),
            _ = term.recv() => Ok(0),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map(|()| 130)
    }
}

fn adapter_for(config: &HostConfig) -> Result<Arc<dyn LlmAdapter>, HostError> {
    if let Some(factory) = &config.adapter_factory {
        return Ok(factory.create(&config.provider, &config.model)?);
    }
    if let Some(replay) = &config.recorded_replay {
        return Ok(Arc::new(recorded_adapter::Adapter::new(
            replay.clone(),
            config.provider.clone(),
            config.model.clone(),
        )));
    }
    Ok(Arc::new(recorded_adapter::UnconfiguredAdapter))
}

mod recorded_adapter {
    use super::*;
    pub(super) struct Adapter {
        recording: String,
        provider: String,
        model: String,
        routes: Mutex<BTreeMap<SessionId, Arc<RecordedLlmAdapter>>>,
    }
    impl Adapter {
        pub(super) fn new(recording: String, provider: String, model: String) -> Self {
            Self {
                recording,
                provider,
                model,
                routes: Mutex::new(BTreeMap::new()),
            }
        }
    }
    #[async_trait]
    impl LlmAdapter for Adapter {
        async fn generate(
            &self,
            request: crate::protocol::GenerateRequest,
            cancellation: tessivum_core::CancellationToken,
        ) -> Result<LlmStream, TessivumError> {
            let session = request.session_id.clone().ok_or_else(|| {
                TessivumError::new(
                    "INVALID_LLM_REQUEST",
                    "recorded host requests require a session id",
                    "host",
                    Value::Null,
                )
            })?;
            let adapter = {
                let mut routes = lock(&self.routes);
                match routes.get(&session) {
                    Some(adapter) => Arc::clone(adapter),
                    None => {
                        let adapter = Arc::new(RecordedLlmAdapter::from_jsonl_with_route(
                            &self.recording,
                            Some(session.clone()),
                            self.provider.clone(),
                            self.model.clone(),
                        )?);
                        routes.insert(session, Arc::clone(&adapter));
                        adapter
                    }
                }
            };
            adapter.generate(request, cancellation).await
        }
    }
    pub(super) struct UnconfiguredAdapter;
}
#[async_trait]
impl LlmAdapter for recorded_adapter::UnconfiguredAdapter {
    async fn generate(
        &self,
        _request: crate::protocol::GenerateRequest,
        _cancellation: tessivum_core::CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        Err(TessivumError::new(
            "LLM_ADAPTER_NOT_CONFIGURED",
            "host has no recorded replay or deployment adapter",
            "host",
            Value::Null,
        ))
    }
}

fn host_file_path(
    data_dir: &Path,
    override_path: Option<&Path>,
    default_name: &str,
    field: &str,
) -> Result<PathBuf, HostError> {
    let path = override_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.join(default_name));
    if path.file_name().is_none() || path.is_dir() {
        return Err(HostError::InvalidConfiguration(format!(
            "{field} must name a file selected by the host"
        )));
    }
    Ok(path)
}

fn validate_config(config: &HostConfig) -> Result<(), HostError> {
    if config.profile.is_empty()
        || config.profile.len() > MAX_PROFILE_BYTES
        || !config
            .profile
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(HostError::InvalidConfiguration(
            "profile must be a bounded ASCII identifier".into(),
        ));
    }
    if config.provider.trim().is_empty()
        || config.model.trim().is_empty()
        || config.max_tokens == Some(0)
    {
        return Err(HostError::InvalidConfiguration(
            "provider/model must be nonblank and max_tokens positive".into(),
        ));
    }
    if !(1..=MAX_NOTIFICATIONS).contains(&config.notification_capacity)
        || !(1..=MAX_LIVE_SESSIONS).contains(&config.max_live_sessions)
    {
        return Err(HostError::InvalidConfiguration(
            "host capacities are outside their bounds".into(),
        ));
    }
    if config.cli_patches.len() > 64 {
        return Err(HostError::InvalidConfiguration(
            "too many CLI patch layers".into(),
        ));
    }
    let needs_legacy = config.entries.as_ref().is_some_and(|entries| {
        entries
            .active_entries()
            .iter()
            .any(|entry| entry.options.runtime == tessivum_core::RuntimeKind::LegacyNode)
    });
    if needs_legacy && config.legacy_profile.is_none() {
        return Err(HostError::InvalidConfiguration(
            "legacy-node entries require a Legacy profile".into(),
        ));
    }
    config
        .wasm_limits
        .validate()
        .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
    for patch in std::iter::once(&config.bundle_patch)
        .chain(std::iter::once(&config.profile_patch))
        .chain(std::iter::once(&config.home_patch))
        .chain(config.cli_patches.iter())
        .chain(std::iter::once(&config.telemetry_patch))
    {
        if !patch.is_object() || json_size(patch) > MAX_FRAME_BYTES {
            return Err(HostError::InvalidConfiguration(
                "patches must be bounded JSON objects".into(),
            ));
        }
    }
    Ok(())
}

fn validate_prompt(params: &SessionPromptParams) -> Result<(), HostError> {
    validate_session(&params.session_id)?;
    params.validate()?;
    if params.content_blocks.is_empty()
        || params.content_blocks.len() > MAX_PROMPT_BLOCKS
        || json_size(&params.content_blocks) > MAX_FRAME_BYTES
    {
        return Err(HostError::invalid(
            "INVALID_SESSION_PROMPT",
            "contentBlocks must be a bounded non-empty array",
        ));
    }
    Ok(())
}

fn validate_session(session: &SessionId) -> Result<(), HostError> {
    if session.as_str().is_empty() || session.as_str().len() > MAX_PROFILE_BYTES {
        Err(HostError::invalid(
            "INVALID_SESSION_ID",
            "session id is invalid",
        ))
    } else {
        Ok(())
    }
}

fn merge_object(base: &mut Map<String, Value>, patch: &Map<String, Value>) {
    for (key, value) in patch {
        match (base.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(patch)) => merge_object(existing, patch),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn relay_missing_events(
    inner: &HostInner,
    session: &crate::session::Session,
    session_id: &SessionId,
    next_seq: &mut u64,
) {
    let start = *next_seq;
    for event in session
        .events()
        .into_iter()
        .filter(|event| event.seq >= start)
    {
        *next_seq = event.seq.saturating_add(1);
        relay_event(inner, session_id, event);
    }
}

fn relay_event(inner: &HostInner, session_id: &SessionId, event: SessionEvent) {
    if let Some(telemetry) = &inner.telemetry {
        telemetry.capture_event(session_id, &event);
    }
    let _ = inner
        .notices
        .send(HostNotification::SessionEvent(SessionEventNotification {
            session_id: session_id.clone(),
            event: event.clone(),
        }));
    match event.event_type.as_str() {
        "approval/asked" => {
            if let Ok(asked) = serde_json::from_value::<ApprovalAsked>(event.data.clone()) {
                inner.approvals.observe_asked(&asked);
            }
        }
        "approval/decided" => {
            if let Ok(decision) = serde_json::from_value::<ApprovalDecision>(event.data.clone()) {
                inner.approvals.observe_decided(session_id, &decision);
            }
        }
        "turn/end" => inner.approvals.cancel_session(session_id),
        _ => {}
    }
}

fn json_size(value: &impl serde::Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |value| value.len())
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| value.as_millis().try_into().unwrap_or(u64::MAX))
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

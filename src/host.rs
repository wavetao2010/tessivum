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
    attachments::{
        attachments_service_key, decode_inline_image, AttachmentError, AttachmentInput,
        AttachmentLimits, AttachmentRef, AttachmentStore,
    },
    bridge::{BridgeServices, DomainBridge, WasmPolicyRegistry},
    builtin_tools::{BuiltinTools, BuiltinToolsConfig},
    code_runtime::{CodeRuntime, ProcessCodeRuntime},
    credentials::{
        credentials_service_key, CredentialEvent, CredentialRef, Credentials, YamlCredentialFile,
    },
    legacy::{product_loader, LegacyProfile, ProductPackageResolver, WasmProductRuntime},
    llm::{LlmAdapter, LlmProviderRegistration, LlmRuntime, LlmStream, RecordedLlmAdapter},
    openai_responses::{
        OpenAiResponsesAdapter, ProviderSnapshot, ResponsesModel, ResponsesRoute,
        ResponsesRouteResolver, RESPONSES_IMAGE_MODALITY, RESPONSES_TEXT_MODALITY,
    },
    persistence_jsonl::JsonlSessionPersistence,
    protocol::{
        AgentCancelCause, ContentBlock, InitializeParams, InitializeResult, Message, MessageId,
        MessageRole, MessageSource, SdkServerInfo, SessionEvent, SessionEventNotification,
        SessionHeader, SessionId, SessionModelSelection, SessionPromptParams, SessionPromptResult,
        SessionStatus, SessionStatusNotification, SubagentFinishedNotification,
        SubagentStartedNotification, MAX_SAFE_INTEGER, SESSION_FORMAT_VERSION,
    },
    session::{session_service_key, SessionError, SessionPersistence, SessionStore},
    settings::{
        settings_service_key, Settings, SettingsError, SettingsEvent, SettingsPathOp,
        SettingsRegistration, SettingsSnapshot, YamlSettingsProvider,
        AGENT_DEFAULT_MODEL_NAMESPACE, LLM_OPENAI_RESPONSES_NAMESPACE,
    },
    subprocess::SubprocessRuntime,
    system_prompt::{PromptRegistration, PromptSection, SystemPrompt},
    telemetry::TelemetryCoordinator,
    tools::{ToolRestrictions, ToolRuntime},
    workspace::{SessionResourceResolver, WorkspaceError, WorkspaceId, WorkspaceRegistry},
    TessivumError,
};

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostModelInfo {
    pub provider: String,
    pub id: String,
    pub name: Option<String>,
    pub input_modalities: Vec<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub routable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostModelGroup {
    pub provider: String,
    pub display_name: String,
    pub models: Vec<HostModelInfo>,
    pub credential_configured: bool,
    pub routable: bool,
    pub failure: Option<HostRouteFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostRouteFailure {
    pub provider: String,
    pub model: Option<String>,
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionModels {
    pub current: Option<SessionModelSelection>,
    pub routable: bool,
    pub groups: Vec<HostModelGroup>,
    pub failures: Vec<HostRouteFailure>,
}

#[derive(Clone, Debug)]
struct DynamicRouteResolver {
    routes: Arc<Mutex<Arc<BTreeMap<String, ResponsesRoute>>>>,
    credentials: Arc<Credentials>,
}

impl ResponsesRouteResolver for DynamicRouteResolver {
    fn resolve(&self, provider: &str, model: &str) -> Result<ProviderSnapshot, TessivumError> {
        let route = lock(&self.routes).get(provider).cloned().ok_or_else(|| {
            model_error(
                "LLM_PROVIDER_NOT_FOUND",
                "provider route is not registered",
                provider,
                Some(model),
            )
        })?;
        let model_descriptor = route
            .models
            .iter()
            .find(|candidate| candidate.id == model)
            .cloned()
            .ok_or_else(|| {
                model_error(
                    "LLM_MODEL_NOT_FOUND",
                    "model is not declared by provider route",
                    provider,
                    Some(model),
                )
            })?;
        let credential_ref = CredentialRef::new(route.credential_ref.clone()).map_err(|error| {
            TessivumError::new(
                "INVALID_CREDENTIAL_REF",
                error.to_string(),
                "host",
                Value::Null,
            )
        })?;
        let api_key = resolve_credential_sync(Arc::clone(&self.credentials), credential_ref)?;
        match api_key {
            Some(api_key) => ProviderSnapshot::new(route, model_descriptor, api_key),
            None => ProviderSnapshot::without_key(route, model_descriptor),
        }
    }
}

#[derive(Default)]
struct RouteState {
    routes: Arc<BTreeMap<String, ResponsesRoute>>,
    registrations: BTreeMap<String, LlmProviderRegistration>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostProviderDirectoryEntry {
    pub route: ResponsesRoute,
    pub credential_configured: bool,
    pub namespace: String,
    pub settings_path: Vec<String>,
    pub active: bool,
    pub declared: bool,
}

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
    #[error(transparent)]
    Attachment(#[from] AttachmentError),

    #[error("session {session_id} is durable but ungrouped")]
    SessionUngrouped { session_id: SessionId },
    #[error("failed to attach session {session_id} to workspace {workspace_id}: {source}")]
    WorkspaceAttach {
        session_id: SessionId,
        workspace_id: WorkspaceId,
        #[source]
        source: WorkspaceError,
    },
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
}

impl HostError {
    fn invalid(code: impl Into<String>, message: impl Into<String>) -> Self {
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
            Self::SessionUngrouped { .. } => "SESSION_UNGROUPED",
            Self::WorkspaceAttach { .. } => "WORKSPACE_ATTACH_FAILED",
            Self::Workspace(error) => error.code(),
            Self::Runtime(error) => &error.code,
            Self::Session(error) => error.code(),
            Self::Agent(AgentError::Cancelled) => "CANCELLED",
            Self::Agent(_) => "HOST_AGENT_ERROR",
            Self::Core(_) => "HOST_CORE_ERROR",
            Self::Approval(_) => "HOST_APPROVAL_ERROR",
            Self::ApprovalRegistry(_) => "HOST_APPROVAL_REGISTRY_ERROR",
            Self::Attachment(error) => error.code(),
        }
    }

    fn wire(self) -> TessivumError {
        match self {
            Self::Runtime(error) => error,
            Self::SessionUngrouped { session_id } => TessivumError::new(
                "SESSION_UNGROUPED",
                format!("session {session_id} is durable but ungrouped"),
                "host",
                json!({"sessionId": session_id}),
            ),
            Self::WorkspaceAttach {
                session_id,
                workspace_id,
                source: _,
            } => TessivumError::new(
                "WORKSPACE_ATTACH_FAILED",
                format!("failed to attach session {session_id} to workspace {workspace_id}"),
                "host",
                json!({"sessionId": session_id, "workspaceId": workspace_id}),
            ),
            Self::Workspace(error) => TessivumError::new(
                error.code(),
                "workspace operation failed",
                "host",
                Value::Null,
            ),
            Self::Approval(error) => TessivumError::new(
                "HOST_APPROVAL_ERROR",
                error.to_string(),
                "host",
                Value::Null,
            ),
            Self::ApprovalRegistry(error) => TessivumError::new(
                "HOST_APPROVAL_REGISTRY_ERROR",
                error.to_string(),
                "host",
                Value::Null,
            ),
            error => {
                let code = error.code().to_owned();
                TessivumError::new(code, error.to_string(), "host", Value::Null)
            }
        }
    }
}

/// A settings write routed through the host so live provider routes are updated
/// before the transport reports success.
pub enum HostSettingsMutation {
    Update {
        patch: Value,
        expected_revision: Option<u64>,
    },
    Replace {
        user: Value,
        expected_revision: Option<u64>,
    },
    Mutate {
        ops: Vec<SettingsPathOp>,
        expected_revision: Option<u64>,
    },
}
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
    pub workspace_id: Option<WorkspaceId>,
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
            workspace_id: None,
            created_at: 0,
            cwd: None,
            event_count: 0,
        })
    }
    async fn create_session_in(
        &self,
        session_id: SessionId,
        _workspace_id: WorkspaceId,
    ) -> Result<HostSessionInfo, TessivumError> {
        self.create_session(session_id).await
    }
    async fn delete_workspace(&self, _workspace_id: WorkspaceId) -> Result<bool, TessivumError> {
        Ok(false)
    }
    async fn list_sessions(&self) -> Result<Vec<HostSessionInfo>, TessivumError> {
        Ok(Vec::new())
    }
    fn provider_directory(&self) -> Vec<HostProviderDirectoryEntry> {
        Vec::new()
    }
    fn model_groups(&self, _provider: &str) -> Vec<HostModelGroup> {
        Vec::new()
    }
    async fn session_models(
        &self,
        _session: SessionId,
    ) -> Result<HostSessionModels, TessivumError> {
        Ok(HostSessionModels {
            current: None,
            routable: false,
            groups: Vec::new(),
            failures: Vec::new(),
        })
    }
    async fn select_model(
        &self,
        _session: SessionId,
        _provider: String,
        _model: String,
        _reasoning_effort: Option<String>,
    ) -> Result<SessionModelSelection, TessivumError> {
        Err(TessivumError::new(
            "MODEL_SELECTION_UNSUPPORTED",
            "this host does not support model selection",
            "host",
            Value::Null,
        ))
    }
    fn attachment_limits(&self) -> AttachmentLimits {
        AttachmentLimits::default()
    }
    async fn upload_attachment(
        &self,
        _data: Vec<u8>,
        _name: Option<String>,
    ) -> Result<AttachmentRef, TessivumError> {
        Err(TessivumError::new(
            "ATTACHMENTS_UNSUPPORTED",
            "this host does not support attachments",
            "host",
            Value::Null,
        ))
    }
    async fn normalize_prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptParams, TessivumError> {
        Ok(params)
    }
    async fn mutate_settings(
        &self,
        namespace: String,
        mutation: HostSettingsMutation,
    ) -> Result<SettingsSnapshot, SettingsError> {
        let settings = self.settings().ok_or(SettingsError::Closed)?;
        match mutation {
            HostSettingsMutation::Update {
                patch,
                expected_revision,
            } => settings.update(&namespace, patch, expected_revision).await,
            HostSettingsMutation::Replace {
                user,
                expected_revision,
            } => settings.replace(&namespace, user, expected_revision).await,
            HostSettingsMutation::Mutate {
                ops,
                expected_revision,
            } => settings.mutate(&namespace, ops, expected_revision).await,
        }
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
    fn workspace_registry(&self) -> Option<WorkspaceRegistry> {
        None
    }
    fn default_workspace_id(&self) -> Option<WorkspaceId> {
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
    llm: LlmRuntime,
    attachments: Arc<AttachmentStore>,
    route_adapter: Arc<dyn LlmAdapter>,
    dynamic_routes: bool,
    route_resolver: Arc<DynamicRouteResolver>,
    route_state: Mutex<RouteState>,
    route_gate: AsyncMutex<()>,

    cancellation: tessivum_core::CancellationToken,
    sessions: SessionStore,
    persistence: Arc<dyn SessionPersistence>,
    workspace_registry: WorkspaceRegistry,
    default_workspace_id: Option<WorkspaceId>,
    resources: Arc<SessionResourceResolver>,
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
    // ponytail: one Host-wide gate serializes session create/delete and agent handoff; shard by session only if contention matters.
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

enum ImagePlan {
    Reference(AttachmentRef),
    Inline(AttachmentRef),
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
    _attachments: ServiceHandle<Arc<AttachmentStore>>,
    _telemetry: Option<ServiceHandle<TelemetryCoordinator>>,
    _code: Option<ServiceHandle<ProcessCodeRuntime>>,
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
        let attachments = Arc::new(AttachmentStore::new(
            data_dir.join("attachments"),
            AttachmentLimits::default(),
        )?);
        let attachments_service =
            root.provide(attachments_service_key(), Arc::clone(&attachments))?;
        let settings = Arc::new(Settings::new(Arc::new(YamlSettingsProvider::new(
            settings_path,
        ))));
        let settings_service = root.provide(settings_service_key(), Arc::clone(&settings))?;
        let credentials = Arc::new(Credentials::new(Arc::new(YamlCredentialFile::new(
            credentials_path,
        ))));
        let credentials_service =
            root.provide(credentials_service_key(), Arc::clone(&credentials))?;
        let settings_base = profile
            .get(LLM_OPENAI_RESPONSES_NAMESPACE)
            .cloned()
            .unwrap_or_else(|| json!({}));
        settings
            .register(openai_settings_registration(settings_base))
            .await
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let default_base = profile
            .get(AGENT_DEFAULT_MODEL_NAMESPACE)
            .cloned()
            .unwrap_or_else(|| json!({}));
        settings
            .register(default_model_registration(&config, default_base))
            .await
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let route_snapshot = settings
            .get(LLM_OPENAI_RESPONSES_NAMESPACE)
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let initial_routes = parse_routes(&route_snapshot.value, route_snapshot.revision)
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let route_map = Arc::new(initial_routes);
        let route_resolver = Arc::new(DynamicRouteResolver {
            routes: Arc::new(Mutex::new(Arc::clone(&route_map))),
            credentials: Arc::clone(&credentials),
        });
        let dynamic_routes = config.adapter_factory.is_none() && config.recorded_replay.is_none();
        let persistence: Arc<dyn SessionPersistence> =
            Arc::new(JsonlSessionPersistence::new(&data_dir));
        let session_inspections = persistence.list(cancellation.clone()).await?;
        let workspace_registry = WorkspaceRegistry::open(&data_dir, &cwd, session_inspections)?;
        let default_workspace_id = workspace_registry
            .list()
            .into_iter()
            .find(|workspace| Path::new(&workspace.path) == cwd.as_path())
            .map(|workspace| workspace.workspace_id);
        let resources = Arc::new(SessionResourceResolver::new(workspace_registry.clone()));
        let sessions = SessionStore::new(Arc::clone(&persistence));
        let session_service = root.provide(session_service_key(), sessions.clone())?;

        let llm = LlmRuntime::new();
        let adapter: Arc<dyn LlmAdapter> = if dynamic_routes {
            Arc::new(OpenAiResponsesAdapter::with_resolver_and_store(
                (*route_resolver).clone(),
                Arc::clone(&attachments),
            ))
        } else {
            adapter_for(&config)?
        };
        let mut registrations = BTreeMap::new();
        if dynamic_routes {
            for provider in route_map.keys() {
                registrations.insert(
                    provider.clone(),
                    llm.register(provider.clone(), Arc::clone(&adapter))?,
                );
            }
        } else {
            registrations.insert(
                config.provider.clone(),
                llm.register(config.provider.clone(), Arc::clone(&adapter))?,
            );
        }
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
                resolver: Some(Arc::clone(&resources)),
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
            llm,
            attachments: Arc::clone(&attachments),
            route_adapter: Arc::clone(&adapter),
            dynamic_routes,
            route_resolver,
            route_state: Mutex::new(RouteState {
                routes: route_map,
                registrations,
            }),
            route_gate: AsyncMutex::new(()),
            cancellation,
            sessions,
            persistence,
            workspace_registry,
            default_workspace_id,
            resources,
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
                _attachments: attachments_service,
                _telemetry: telemetry_service,
                _code: code_service,
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
    pub fn provider_directory(&self) -> Vec<HostProviderDirectoryEntry> {
        let state = lock(&self.inner.route_state);
        state
            .routes
            .values()
            .cloned()
            .map(|route| {
                let credential_configured = CredentialRef::new(route.credential_ref.clone())
                    .ok()
                    .and_then(|reference| {
                        resolve_credential_sync(Arc::clone(&self.inner.credentials), reference).ok()
                    })
                    .flatten()
                    .is_some();
                let active = state
                    .registrations
                    .get(&route.id)
                    .is_some_and(LlmProviderRegistration::is_active);
                HostProviderDirectoryEntry {
                    route,
                    credential_configured,
                    namespace: LLM_OPENAI_RESPONSES_NAMESPACE.into(),
                    settings_path: vec!["providers".into()],
                    active,
                    declared: true,
                }
            })
            .collect()
    }

    pub fn model_groups(&self, provider: &str) -> Vec<HostModelGroup> {
        let state = lock(&self.inner.route_state);
        state
            .routes
            .get(provider)
            .cloned()
            .map(|route| model_group_for_route(&self.inner.credentials, route))
            .into_iter()
            .collect()
    }

    pub async fn session_models(
        &self,
        session_id: SessionId,
    ) -> Result<HostSessionModels, HostError> {
        validate_session(&session_id)?;
        let events = if let Some(session) = self.inner.sessions.get(&session_id) {
            session.events()
        } else {
            self.inner
                .persistence
                .read_from(&session_id, 0, self.inner.cancellation.clone())
                .await?
        };
        let current = latest_model_selection(&events)
            .or_else(|| self.default_selection())
            .or_else(|| Some(self.config_selection()));
        let groups = {
            let state = lock(&self.inner.route_state);
            state
                .routes
                .values()
                .cloned()
                .map(|route| model_group_for_route(&self.inner.credentials, route))
                .collect::<Vec<_>>()
        };
        let mut failures = Vec::new();
        let routable = current
            .as_ref()
            .is_some_and(|selection| self.selection_is_routable(selection, &mut failures));
        Ok(HostSessionModels {
            current,
            routable,
            groups,
            failures,
        })
    }

    pub async fn select_model(
        &self,
        session_id: SessionId,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Result<SessionModelSelection, HostError> {
        let _admission = self.admit()?;
        validate_session(&session_id)?;
        let selection = SessionModelSelection {
            provider,
            model,
            reasoning_effort,
        };
        selection.validate()?;
        self.validate_selection(&selection)?;
        let _setup = self.inner.setup.lock().await;
        let session = match self.inner.sessions.get(&session_id) {
            Some(session) => session,
            None => {
                self.inner
                    .sessions
                    .restore(
                        &session_id,
                        crate::session::RestoreMode::Live,
                        self.inner.cancellation.clone(),
                    )
                    .await?
            }
        };
        if let Some(agent) = self.inner.registry.get(&session_id) {
            if agent.status() == AgentStatus::Running {
                return Err(HostError::invalid(
                    "SESSION_BUSY",
                    "cannot select a model while the session is running",
                ));
            }
            self.inner.approvals.cancel_session(&session_id);
            let _ = self
                .inner
                .registry
                .cancel(&session_id, AgentCancelCause::Disposed, false);
            lock(&self.inner.owned_agents).remove(&session_id);
            let _ = agent.dispose().await;
        }
        let event = SessionEvent {
            event_type: "session/model-selected".into(),
            seq: session.next_seq()?,
            time: now(),
            data: serde_json::to_value(&selection).unwrap_or(Value::Null),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
        };
        session
            .append(event, self.inner.cancellation.clone())
            .await?;
        self.ensure_relay(session);
        Ok(selection)
    }

    fn default_selection(&self) -> Option<SessionModelSelection> {
        self.inner
            .settings
            .get(AGENT_DEFAULT_MODEL_NAMESPACE)
            .ok()
            .and_then(|snapshot| serde_json::from_value(snapshot.value).ok())
    }

    fn config_selection(&self) -> SessionModelSelection {
        SessionModelSelection {
            provider: self.inner.config.provider.clone(),
            model: self.inner.config.model.clone(),
            reasoning_effort: None,
        }
    }

    async fn selection_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionModelSelection, HostError> {
        if !self.inner.dynamic_routes {
            return Ok(self.config_selection());
        }
        let events = if let Some(session) = self.inner.sessions.get(session_id) {
            session.events()
        } else {
            self.inner
                .persistence
                .read_from(session_id, 0, self.inner.cancellation.clone())
                .await?
        };
        if let Some(selection) =
            latest_model_selection(&events).or_else(|| self.default_selection())
        {
            self.validate_selection(&selection)?;
            return Ok(selection);
        }
        Ok(self.config_selection())
    }

    fn selection_is_routable(
        &self,
        selection: &SessionModelSelection,
        failures: &mut Vec<HostRouteFailure>,
    ) -> bool {
        let state = lock(&self.inner.route_state);
        let Some(route) = state.routes.get(&selection.provider) else {
            if !self.inner.dynamic_routes
                && selection.provider == self.inner.config.provider
                && selection.model == self.inner.config.model
            {
                return true;
            }
            failures.push(route_failure(
                "LLM_PROVIDER_NOT_FOUND",
                "provider route is not registered",
                selection,
                None,
            ));
            return false;
        };
        let Some(model) = route
            .models
            .iter()
            .find(|model| model.id == selection.model)
        else {
            failures.push(route_failure(
                "LLM_MODEL_NOT_FOUND",
                "model is not declared by provider route",
                selection,
                None,
            ));
            return false;
        };
        if CredentialRef::new(route.credential_ref.clone())
            .ok()
            .and_then(|reference| {
                resolve_credential_sync(Arc::clone(&self.inner.credentials), reference).ok()
            })
            .flatten()
            .is_none()
        {
            failures.push(route_failure(
                "MISSING_CREDENTIAL",
                "provider credential is not configured",
                selection,
                Some(model.id.clone()),
            ));
            return false;
        }
        true
    }

    fn validate_selection(&self, selection: &SessionModelSelection) -> Result<(), HostError> {
        let mut failures = Vec::new();
        if self.selection_is_routable(selection, &mut failures) {
            Ok(())
        } else {
            let failure = failures.into_iter().next().unwrap_or_else(|| {
                route_failure(
                    "MODEL_NOT_ROUTABLE",
                    "model is not routable",
                    selection,
                    None,
                )
            });
            Err(HostError::invalid(failure.code, failure.message))
        }
    }
    pub fn attachment_limits(&self) -> AttachmentLimits {
        self.inner.attachments.limits().clone()
    }

    async fn upload_attachment_inner(
        &self,
        data: Vec<u8>,
        name: Option<String>,
    ) -> Result<AttachmentRef, HostError> {
        let _admission = self.admit()?;
        Ok(self
            .inner
            .attachments
            .save(AttachmentInput::new(data, name))
            .await?)
    }

    async fn normalize_prompt_inner(
        &self,
        mut params: SessionPromptParams,
    ) -> Result<SessionPromptParams, HostError> {
        validate_session(&params.session_id)?;
        let mut plans = Vec::new();
        let mut inputs = Vec::new();
        collect_image_plans(
            &params.content_blocks,
            &self.inner.attachments,
            &mut plans,
            &mut inputs,
        )?;
        if plans.is_empty() {
            return Ok(params);
        }

        let limits = self.inner.attachments.limits();
        if plans.len() > limits.max_images_per_message {
            return Err(AttachmentError::BatchCountLimit.into());
        }
        let mut total_bytes = 0u64;
        for plan in &plans {
            let reference = match plan {
                ImagePlan::Reference(reference) | ImagePlan::Inline(reference) => reference,
            };
            if !limits.media_types.contains(&reference.media_type) {
                return Err(AttachmentError::UnsupportedMediaType.into());
            }
            let pixels = u64::from(reference.width)
                .checked_mul(u64::from(reference.height))
                .ok_or(AttachmentError::PixelLimit)?;
            if reference.bytes > limits.max_image_bytes {
                return Err(AttachmentError::ByteLimit.into());
            }
            if pixels == 0 || pixels > limits.max_image_pixels {
                return Err(AttachmentError::PixelLimit.into());
            }
            total_bytes = total_bytes
                .checked_add(reference.bytes)
                .ok_or(AttachmentError::BatchByteLimit)?;
            if total_bytes > limits.max_message_image_bytes {
                return Err(AttachmentError::BatchByteLimit.into());
            }
        }

        if self.inner.dynamic_routes {
            let events = if let Some(session) = self.inner.sessions.get(&params.session_id) {
                session.events()
            } else {
                self.inner
                    .persistence
                    .read_from(&params.session_id, 0, self.inner.cancellation.clone())
                    .await?
            };
            let selection = latest_model_selection(&events).or_else(|| self.default_selection());
            let supports_image = selection
                .as_ref()
                .and_then(|selection| {
                    let state = lock(&self.inner.route_state);
                    state.routes.get(&selection.provider).and_then(|route| {
                        route
                            .models
                            .iter()
                            .find(|model| model.id == selection.model)
                            .cloned()
                    })
                })
                .is_some_and(|model| {
                    let input = if model.input.is_empty() {
                        &[RESPONSES_TEXT_MODALITY.to_owned()][..]
                    } else {
                        &model.input
                    };
                    input
                        .iter()
                        .any(|modality| modality == RESPONSES_IMAGE_MODALITY)
                });
            if !supports_image {
                return Err(HostError::invalid(
                    "UNSUPPORTED_MODALITY",
                    "the selected model does not support image input",
                ));
            }
        }

        for plan in &plans {
            if let ImagePlan::Reference(reference) = plan {
                self.inner.attachments.read_ref(reference).await?;
            }
        }
        let inline_refs = if inputs.is_empty() {
            Vec::new()
        } else {
            self.inner.attachments.save_batch(inputs).await?
        };
        let mut plan_index = 0;
        let mut inline_index = 0;
        replace_image_plans(
            &mut params.content_blocks,
            &plans,
            &mut plan_index,
            &inline_refs,
            &mut inline_index,
        )?;
        Ok(params)
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
        let _setup = self.inner.setup.lock().await;
        self.create_session_in_unadmitted(session_id, self.default_workspace_id()?)
            .await
    }

    async fn create_session_in_inner(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<HostSessionInfo, HostError> {
        let _admission = self.admit()?;
        let _setup = self.inner.setup.lock().await;
        self.create_session_in_unadmitted(session_id, workspace_id)
            .await
    }

    async fn create_session_in_unadmitted(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<HostSessionInfo, HostError> {
        validate_session(&session_id)?;
        let lease = self.inner.workspace_registry.resolve(&workspace_id)?;
        let root = lease.validate_current()?;
        let existing = match self.inner.sessions.get(&session_id) {
            Some(session) => Some((session.header(), session.events().len() as u64)),
            None => self
                .inner
                .persistence
                .inspect(&session_id, self.inner.cancellation.clone())
                .await?
                .map(|session| (session.header, session.event_count)),
        };
        if let Some((header, event_count)) = existing {
            self.require_session_root(&header, &root)?;
            if self
                .inner
                .workspace_registry
                .workspace_for_session(&session_id)
                .is_some_and(|workspace| workspace.workspace_id != workspace_id)
            {
                return Err(HostError::invalid(
                    "SESSION_CONFLICT",
                    "session is already attached to another workspace",
                ));
            }
            self.inner
                .workspace_registry
                .recognize_session(&session_id)?;
            self.inner
                .workspace_registry
                .attach_session(&workspace_id, &session_id, None)
                .map_err(|source| HostError::WorkspaceAttach {
                    session_id: session_id.clone(),
                    workspace_id: workspace_id.clone(),
                    source,
                })?;
            return Ok(HostSessionInfo {
                session_id,
                workspace_id: Some(workspace_id),
                created_at: header.created_at,
                cwd: header.cwd,
                event_count,
            });
        }

        let created_at = now();
        let cwd = root.to_string_lossy().into_owned();
        self.inner
            .sessions
            .create(
                SessionHeader {
                    version: SESSION_FORMAT_VERSION,
                    id: session_id.clone(),
                    created_at,
                    cwd: Some(cwd.clone()),
                    parent_session: None,
                    seed_length: None,
                    origin: None,
                    delegation_depth: Some(0),
                    agent_preset: Some(self.inner.identity.profile.clone()),
                },
                self.inner.cancellation.clone(),
            )
            .await?;
        self.inner
            .workspace_registry
            .recognize_session(&session_id)?;
        self.inner
            .workspace_registry
            .attach_session(&workspace_id, &session_id, None)
            .map_err(|source| HostError::WorkspaceAttach {
                session_id: session_id.clone(),
                workspace_id: workspace_id.clone(),
                source,
            })?;
        Ok(HostSessionInfo {
            session_id,
            workspace_id: Some(workspace_id),
            created_at,
            cwd: Some(cwd),
            event_count: 0,
        })
    }

    async fn delete_workspace_inner(&self, workspace_id: WorkspaceId) -> Result<bool, HostError> {
        let _admission = self.admit()?;
        let _setup = self.inner.setup.lock().await;
        let Some(workspace) = self
            .inner
            .workspace_registry
            .list()
            .into_iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
        else {
            return Ok(false);
        };
        let sessions = workspace.session_ids;
        for session_id in sessions {
            self.inner.approvals.cancel_session(&session_id);
            let owned = lock(&self.inner.owned_agents).remove(&session_id);
            drop(owned);
            if let Some(agent) = self.inner.registry.get(&session_id) {
                agent.cancel(AgentCancelCause::Disposed, false);
                agent.dispose().await?;
            }
            lock(&self.inner.state).statuses.remove(&session_id);
        }
        self.inner.workspace_registry.delete(workspace_id, None)?;
        Ok(true)
    }

    fn default_workspace_id(&self) -> Result<WorkspaceId, HostError> {
        let workspace_id = self.inner.default_workspace_id.clone().ok_or_else(|| {
            HostError::invalid(
                "WORKSPACE_NOT_FOUND",
                "host cwd workspace is not registered",
            )
        })?;
        self.inner.workspace_registry.resolve(&workspace_id)?;
        Ok(workspace_id)
    }

    fn require_session_root(&self, header: &SessionHeader, root: &Path) -> Result<(), HostError> {
        let valid = header
            .cwd
            .as_deref()
            .and_then(|cwd| Path::new(cwd).canonicalize().ok())
            .is_some_and(|cwd| cwd == root);
        if valid {
            Ok(())
        } else {
            Err(HostError::invalid(
                "SESSION_CONFLICT",
                "session cwd does not match the requested workspace",
            ))
        }
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
        let params = self.normalize_prompt_inner(params).await?;
        validate_prompt(&params)?;
        let (agent, session, mut commits, message_id) = {
            let _setup = self.inner.setup.lock().await;
            let agent = self.ensure_agent_under_setup(&params.session_id).await?;
            let session = agent.session();
            let commits = session.subscribe();
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
            let idle_agent = self.inner.registry.get(&params.session_id).ok_or_else(|| {
                HostError::InvalidConfiguration("accepted agent disappeared".into())
            })?;
            self.watch_idle(params.session_id, idle_agent);
            (agent, session, commits, message_id)
        };
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
        self.inner.settings.shutdown().await;
        self.inner.credentials.shutdown().await;
        self.inner.workspace_registry.close();
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
        self.inner.workspace_registry.shutdown();
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

    async fn ensure_agent_under_setup(
        &self,
        session_id: &SessionId,
    ) -> Result<AgentHandle, HostError> {
        let header = match self.inner.sessions.get(session_id) {
            Some(session) => session.header(),
            None => match self
                .inner
                .persistence
                .inspect(session_id, self.inner.cancellation.clone())
                .await?
            {
                Some(session) => session.header,
                None => {
                    self.create_session_in_unadmitted(
                        session_id.clone(),
                        self.default_workspace_id()?,
                    )
                    .await?;
                    self.inner
                        .sessions
                        .get(session_id)
                        .ok_or_else(|| {
                            HostError::InvalidConfiguration("new session was not published".into())
                        })?
                        .header()
                }
            },
        };
        if self
            .inner
            .workspace_registry
            .workspace_for_session(session_id)
            .is_none()
        {
            return Err(HostError::SessionUngrouped {
                session_id: session_id.clone(),
            });
        }
        let lease = self.inner.resources.resolve(session_id)?;
        let root = lease.validate_current()?;
        self.require_session_root(&header, &root)?;
        if let Some(agent) = self.inner.registry.get(session_id) {
            return Ok(agent);
        }
        let selection = self.selection_for_session(session_id).await?;
        let max_tokens = self.inner.config.max_tokens.or_else(|| {
            let state = lock(&self.inner.route_state);
            state
                .routes
                .get(&selection.provider)
                .and_then(|route| {
                    route
                        .models
                        .iter()
                        .find(|model| model.id == selection.model)
                })
                .and_then(|model| model.max_tokens)
        });
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
                    created_at: header.created_at,
                    cwd: Some(root.to_string_lossy().into_owned()),
                    parent_session: None,
                    seed_length: None,
                    origin: None,
                    delegation_depth: Some(0),
                    agent_preset: Some(self.inner.identity.profile.clone()),
                },
                AgentOptions {
                    provider: selection.provider,
                    model: selection.model,
                    max_tokens,
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
                    while let Ok(event) = settings_events.try_recv() {
                        let _ = settings_inner
                            .notices
                            .send(HostNotification::SettingsChanged(event));
                    }
                    break;
                }
                tokio::select! {
                    event = settings_events.recv() => match event {
                        Ok(event) => {
                            let _ = settings_inner.notices.send(HostNotification::SettingsChanged(event));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = settings_inner.relay_stop.notified() => {
                        while let Ok(event) = settings_events.try_recv() {
                            let _ = settings_inner.notices.send(HostNotification::SettingsChanged(event));
                        }
                        break;
                    },
                }
            }
        });
        let mut credential_events = inner.credentials.subscribe();
        let credentials_inner = Arc::clone(inner);
        let credentials_relay = tokio::spawn(async move {
            loop {
                if credentials_inner.relays_closed.load(Ordering::Acquire) {
                    while let Ok(event) = credential_events.try_recv() {
                        let _ = credentials_inner
                            .notices
                            .send(HostNotification::CredentialsChanged(event));
                    }
                    break;
                }
                tokio::select! {
                    event = credential_events.recv() => match event {
                        Ok(event) => { let _ = credentials_inner.notices.send(HostNotification::CredentialsChanged(event)); }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = credentials_inner.relay_stop.notified() => {
                        while let Ok(event) = credential_events.try_recv() {
                            let _ = credentials_inner.notices.send(HostNotification::CredentialsChanged(event));
                        }
                        break;
                    },
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
        let starting_next_seq = session.next_seq().unwrap_or(u64::MAX);
        let mut receiver = session.subscribe();
        let task = tokio::spawn(async move {
            let mut next_seq = starting_next_seq;
            relay_missing_events(&inner, &session, &session_id, &mut next_seq);
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

fn collect_image_plans(
    blocks: &[ContentBlock],
    store: &AttachmentStore,
    plans: &mut Vec<ImagePlan>,
    inputs: &mut Vec<AttachmentInput>,
) -> Result<(), AttachmentError> {
    for block in blocks {
        match block {
            ContentBlock::Image { attachment } => {
                if let Ok(reference) = AttachmentRef::from_value(attachment) {
                    plans.push(ImagePlan::Reference(reference));
                } else {
                    let input = decode_inline_image(attachment)?;
                    let metadata = store.validate(&input)?;
                    plans.push(ImagePlan::Inline(metadata));
                    inputs.push(input);
                }
            }
            ContentBlock::ToolResult { content, .. } => {
                collect_image_plans(content, store, plans, inputs)?;
            }
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolCall { .. } => {}
        }
    }
    Ok(())
}

fn replace_image_plans(
    blocks: &mut [ContentBlock],
    plans: &[ImagePlan],
    plan_index: &mut usize,
    inline_refs: &[AttachmentRef],
    inline_index: &mut usize,
) -> Result<(), HostError> {
    for block in blocks {
        match block {
            ContentBlock::Image { attachment } => {
                let plan = plans.get(*plan_index).ok_or_else(|| {
                    HostError::invalid(
                        "INVALID_IMAGE_BLOCK",
                        "image normalization plan is incomplete",
                    )
                })?;
                let reference = match plan {
                    ImagePlan::Reference(reference) => reference.clone(),
                    ImagePlan::Inline(_) => {
                        let reference = inline_refs.get(*inline_index).ok_or_else(|| {
                            HostError::invalid(
                                "INVALID_IMAGE_BLOCK",
                                "inline image upload is incomplete",
                            )
                        })?;
                        *inline_index += 1;
                        reference.clone()
                    }
                };
                *attachment = serde_json::to_value(reference).map_err(|error| {
                    HostError::InvalidConfiguration(format!(
                        "serialize attachment reference: {error}"
                    ))
                })?;
                *plan_index += 1;
            }
            ContentBlock::ToolResult { content, .. } => {
                replace_image_plans(content, plans, plan_index, inline_refs, inline_index)?;
            }
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolCall { .. } => {}
        }
    }
    Ok(())
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
    async fn create_session_in(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<HostSessionInfo, TessivumError> {
        self.create_session_in_inner(session_id, workspace_id)
            .await
            .map_err(HostError::wire)
    }
    async fn delete_workspace(&self, workspace_id: WorkspaceId) -> Result<bool, TessivumError> {
        self.delete_workspace_inner(workspace_id)
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
                        workspace_id: self
                            .inner
                            .workspace_registry
                            .workspace_for_session(&session.header.id)
                            .map(|workspace| workspace.workspace_id),
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
    fn provider_directory(&self) -> Vec<HostProviderDirectoryEntry> {
        HostHandle::provider_directory(self)
    }
    fn model_groups(&self, provider: &str) -> Vec<HostModelGroup> {
        HostHandle::model_groups(self, provider)
    }
    async fn session_models(&self, session: SessionId) -> Result<HostSessionModels, TessivumError> {
        HostHandle::session_models(self, session)
            .await
            .map_err(HostError::wire)
    }
    async fn select_model(
        &self,
        session: SessionId,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Result<SessionModelSelection, TessivumError> {
        HostHandle::select_model(self, session, provider, model, reasoning_effort)
            .await
            .map_err(HostError::wire)
    }
    fn attachment_limits(&self) -> AttachmentLimits {
        HostHandle::attachment_limits(self)
    }
    async fn upload_attachment(
        &self,
        data: Vec<u8>,
        name: Option<String>,
    ) -> Result<AttachmentRef, TessivumError> {
        self.upload_attachment_inner(data, name)
            .await
            .map_err(HostError::wire)
    }
    async fn mutate_settings(
        &self,
        namespace: String,
        mutation: HostSettingsMutation,
    ) -> Result<SettingsSnapshot, SettingsError> {
        mutate_settings_inner(&self.inner, namespace, mutation).await
    }
    async fn normalize_prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptParams, TessivumError> {
        self.normalize_prompt_inner(params)
            .await
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
    fn workspace_registry(&self) -> Option<WorkspaceRegistry> {
        Some(self.inner.workspace_registry.clone())
    }
    fn default_workspace_id(&self) -> Option<WorkspaceId> {
        self.inner.default_workspace_id.clone()
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
    async fn create_session_in(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<HostSessionInfo, TessivumError> {
        self.handle
            .create_session_in(session_id, workspace_id)
            .await
    }
    async fn delete_workspace(&self, workspace_id: WorkspaceId) -> Result<bool, TessivumError> {
        self.handle.delete_workspace(workspace_id).await
    }
    async fn list_sessions(&self) -> Result<Vec<HostSessionInfo>, TessivumError> {
        self.handle.list_sessions().await
    }
    fn provider_directory(&self) -> Vec<HostProviderDirectoryEntry> {
        self.handle.provider_directory()
    }
    fn model_groups(&self, provider: &str) -> Vec<HostModelGroup> {
        self.handle.model_groups(provider)
    }
    async fn session_models(&self, session: SessionId) -> Result<HostSessionModels, TessivumError> {
        self.handle
            .session_models(session)
            .await
            .map_err(HostError::wire)
    }
    async fn select_model(
        &self,
        session: SessionId,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Result<SessionModelSelection, TessivumError> {
        self.handle
            .select_model(session, provider, model, reasoning_effort)
            .await
            .map_err(HostError::wire)
    }
    fn attachment_limits(&self) -> AttachmentLimits {
        self.handle.attachment_limits()
    }
    async fn upload_attachment(
        &self,
        data: Vec<u8>,
        name: Option<String>,
    ) -> Result<AttachmentRef, TessivumError> {
        self.handle.upload_attachment(data, name).await
    }
    async fn normalize_prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptParams, TessivumError> {
        self.handle.normalize_prompt(params).await
    }
    async fn mutate_settings(
        &self,
        namespace: String,
        mutation: HostSettingsMutation,
    ) -> Result<SettingsSnapshot, SettingsError> {
        self.handle.mutate_settings(namespace, mutation).await
    }
    fn descriptor(&self) -> HostDescriptor {
        self.handle.descriptor()
    }
    fn approval_registry(&self) -> Option<HostApprovalRegistry> {
        self.handle.approval_registry()
    }
    fn workspace_registry(&self) -> Option<WorkspaceRegistry> {
        self.handle.workspace_registry()
    }
    fn default_workspace_id(&self) -> Option<WorkspaceId> {
        HostApi::default_workspace_id(&self.handle)
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

async fn mutate_settings_inner(
    inner: &Arc<HostInner>,
    namespace: String,
    mutation: HostSettingsMutation,
) -> Result<SettingsSnapshot, SettingsError> {
    let routed = inner.dynamic_routes && namespace == LLM_OPENAI_RESPONSES_NAMESPACE;
    let _route_gate = routed.then(|| inner.route_gate.lock());
    let previous = if routed {
        Some(inner.settings.user(&namespace)?)
    } else {
        None
    };
    let snapshot = match mutation {
        HostSettingsMutation::Update {
            patch,
            expected_revision,
        } => {
            inner
                .settings
                .update(&namespace, patch, expected_revision)
                .await?
        }
        HostSettingsMutation::Replace {
            user,
            expected_revision,
        } => {
            inner
                .settings
                .replace(&namespace, user, expected_revision)
                .await?
        }
        HostSettingsMutation::Mutate {
            ops,
            expected_revision,
        } => {
            inner
                .settings
                .mutate(&namespace, ops, expected_revision)
                .await?
        }
    };
    if routed {
        if let Err(error) = apply_route_settings_locked(inner).await {
            if let Some(previous) = previous {
                let _ = inner
                    .settings
                    .replace(&namespace, previous, Some(snapshot.revision))
                    .await;
            }
            return Err(SettingsError::Validation(error.wire()));
        }
    }
    Ok(snapshot)
}

async fn apply_route_settings_locked(inner: &Arc<HostInner>) -> Result<(), HostError> {
    let snapshot = inner
        .settings
        .get(LLM_OPENAI_RESPONSES_NAMESPACE)
        .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
    let candidate = Arc::new(
        parse_routes(&snapshot.value, snapshot.revision)
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?,
    );
    let (old_routes, mut old_registrations) = {
        let mut state = lock(&inner.route_state);
        (
            Arc::clone(&state.routes),
            std::mem::take(&mut state.registrations),
        )
    };
    for registration in old_registrations.values_mut() {
        registration.unregister();
    }
    let mut registrations = BTreeMap::new();
    for provider in candidate.keys() {
        match inner
            .llm
            .register(provider.clone(), Arc::clone(&inner.route_adapter))
        {
            Ok(registration) => {
                registrations.insert(provider.clone(), registration);
            }
            Err(error) => {
                for registration in registrations.values_mut() {
                    registration.unregister();
                }
                for provider in old_routes.keys() {
                    if let Ok(registration) = inner
                        .llm
                        .register(provider.clone(), Arc::clone(&inner.route_adapter))
                    {
                        old_registrations.insert(provider.clone(), registration);
                    }
                }
                let mut state = lock(&inner.route_state);
                state.routes = old_routes;
                state.registrations = old_registrations;
                return Err(error.into());
            }
        }
    }
    *lock(&inner.route_resolver.routes) = Arc::clone(&candidate);
    let mut state = lock(&inner.route_state);
    state.routes = candidate;
    state.registrations = registrations;
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawOpenAiSettings {
    #[serde(default)]
    providers: BTreeMap<String, RawOpenAiRoute>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawOpenAiRoute {
    display_name: String,
    #[serde(alias = "baseURL")]
    base_url: String,
    #[serde(alias = "credentialRef")]
    api_key_env: String,
    models: Vec<RawOpenAiModel>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawOpenAiModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default = "default_modalities", alias = "inputModalities")]
    input: Vec<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
}

fn default_modalities() -> Vec<String> {
    vec![RESPONSES_TEXT_MODALITY.into()]
}

fn parse_routes(
    value: &Value,
    revision: u64,
) -> Result<BTreeMap<String, ResponsesRoute>, TessivumError> {
    let raw: RawOpenAiSettings = serde_json::from_value(value.clone()).map_err(|error| {
        TessivumError::new(
            "INVALID_OPENAI_ROUTE_SETTINGS",
            format!("invalid provider settings: {error}"),
            "host",
            Value::Null,
        )
    })?;
    let mut routes = BTreeMap::new();
    for (id, raw_route) in raw.providers {
        if id.trim().is_empty() || raw_route.models.is_empty() {
            return Err(TessivumError::new(
                "INVALID_OPENAI_ROUTE_SETTINGS",
                "provider routes require an id and at least one model",
                "host",
                Value::Null,
            ));
        }
        let credential = CredentialRef::new(raw_route.api_key_env.clone()).map_err(|error| {
            TessivumError::new(
                "INVALID_CREDENTIAL_REF",
                error.to_string(),
                "host",
                Value::Null,
            )
        })?;
        let mut models = Vec::with_capacity(raw_route.models.len());
        for raw_model in raw_route.models {
            if raw_model
                .context_window
                .is_some_and(|value| value > MAX_SAFE_INTEGER)
                || raw_model
                    .max_tokens
                    .is_some_and(|value| value > MAX_SAFE_INTEGER)
            {
                return Err(TessivumError::new(
                    "INVALID_OPENAI_MODEL",
                    "model limits must be safe positive integers",
                    "host",
                    Value::Null,
                ));
            }
            let mut input = raw_model.input;
            if input.is_empty() {
                input.push(RESPONSES_TEXT_MODALITY.into());
            }
            let mut seen = BTreeSet::new();
            if input.iter().any(|modality| !seen.insert(modality.clone())) {
                return Err(TessivumError::new(
                    "INVALID_OPENAI_MODALITY",
                    "model input modalities must be unique",
                    "host",
                    Value::Null,
                ));
            }
            let model = ResponsesModel {
                id: raw_model.id,
                name: raw_model.name,
                input,
                context_window: raw_model.context_window,
                max_tokens: raw_model.max_tokens,
            };
            model.validate()?;
            models.push(model);
        }
        let mut route = ResponsesRoute::new(
            id.clone(),
            raw_route.display_name,
            raw_route.base_url,
            credential.as_str(),
            models,
        );
        route.generation = revision;
        route.validate()?;
        routes.insert(id, route);
    }
    Ok(routes)
}

fn openai_settings_registration(base: Value) -> SettingsRegistration {
    SettingsRegistration::new(
        LLM_OPENAI_RESPONSES_NAMESPACE,
        json!({
            "type": "object",
            "properties": {
                "providers": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "object",
                        "properties": {
                            "displayName": {"type": "string"},
                            "baseURL": {"type": "string", "format": "uri"},
                            "apiKeyEnv": {"type": "string", "role": "credential-ref"},
                            "models": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "required": ["id"],
                                    "properties": {
                                        "id": {"type": "string"},
                                        "name": {"type": "string"},
                                        "input": {
                                            "type": "array",
                                            "items": {"type": "string", "enum": ["text", "image"]}
                                        },
                                        "contextWindow": {"type": "integer", "minimum": 1},
                                        "maxTokens": {"type": "integer", "minimum": 1}
                                    },
                                    "additionalProperties": false
                                }
                            }
                        },
                        "required": ["displayName", "baseURL", "apiKeyEnv", "models"],
                        "additionalProperties": false
                    }
                }
            },
            "additionalProperties": false
        }),
        json!({"providers": {}}),
        base,
    )
    .with_validator(Arc::new(|value| parse_routes(value, 0).map(|_| ())))
}

fn default_model_registration(config: &HostConfig, base: Value) -> SettingsRegistration {
    SettingsRegistration::new(
        AGENT_DEFAULT_MODEL_NAMESPACE,
        json!({"type":"object","required":["provider","model"],"additionalProperties":false}),
        json!({"provider":config.provider,"model":config.model}),
        base,
    )
    .with_validator(Arc::new(|value| {
        let selection: SessionModelSelection =
            serde_json::from_value(value.clone()).map_err(|error| {
                TessivumError::new(
                    "INVALID_MODEL_SELECTION",
                    error.to_string(),
                    "settings",
                    Value::Null,
                )
            })?;
        selection.validate()
    }))
}

fn latest_model_selection(events: &[SessionEvent]) -> Option<SessionModelSelection> {
    events.iter().rev().find_map(|event| {
        (event.event_type == "session/model-selected")
            .then(|| serde_json::from_value(event.data.clone()).ok())
            .flatten()
    })
}

fn model_group_for_route(credentials: &Arc<Credentials>, route: ResponsesRoute) -> HostModelGroup {
    let credential_configured = CredentialRef::new(route.credential_ref.clone())
        .ok()
        .and_then(|reference| resolve_credential_sync(Arc::clone(credentials), reference).ok())
        .flatten()
        .is_some();
    let routable = credential_configured;
    HostModelGroup {
        provider: route.id.clone(),
        display_name: route.display_name.clone(),
        models: route
            .models
            .into_iter()
            .map(|model| HostModelInfo {
                provider: route.id.clone(),
                id: model.id,
                name: model.name,
                input_modalities: if model.input.is_empty() {
                    default_modalities()
                } else {
                    model.input
                },
                context_window: model.context_window,
                max_tokens: model.max_tokens,
                routable,
            })
            .collect(),
        credential_configured,
        routable,
        failure: (!credential_configured).then(|| HostRouteFailure {
            provider: route.id,
            model: None,
            code: "MISSING_CREDENTIAL".into(),
            message: "provider credential is not configured".into(),
        }),
    }
}

fn route_failure(
    code: &str,
    message: &str,
    selection: &SessionModelSelection,
    model: Option<String>,
) -> HostRouteFailure {
    HostRouteFailure {
        provider: selection.provider.clone(),
        model,
        code: code.into(),
        message: message.into(),
    }
}

fn model_error(code: &str, message: &str, provider: &str, model: Option<&str>) -> TessivumError {
    TessivumError::new(
        code,
        message,
        "host",
        json!({"provider": provider, "model": model}),
    )
}

fn resolve_credential_sync(
    credentials: Arc<Credentials>,
    reference: CredentialRef,
) -> Result<Option<String>, TessivumError> {
    let handle = tokio::runtime::Handle::try_current().ok();
    std::thread::spawn(move || {
        let result = if let Some(handle) = handle {
            handle.block_on(credentials.resolve(&reference))
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    crate::credentials::CredentialError::Persistence(error.to_string())
                })
                .and_then(|runtime| runtime.block_on(credentials.resolve(&reference)))
        };
        result.map_err(|error| {
            TessivumError::new(error.code(), error.to_string(), "credentials", Value::Null)
        })
    })
    .join()
    .map_err(|_| {
        TessivumError::new(
            "CREDENTIALS_RESOLVE_FAILED",
            "credential resolution thread failed",
            "credentials",
            Value::Null,
        )
    })?
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
        "turn/end" => {
            if let Some(turn) = event.data.get("turn").and_then(Value::as_u64) {
                inner.approvals.cancel_turn(session_id, turn);
            }
        }
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

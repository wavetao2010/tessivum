//! Durable, parent-owned child agent orchestration.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Weak,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::{
    agent::{AgentError, AgentHandle, AgentOptions, AgentRegistry, InboxTarget},
    agent_mode::AgentModeId,
    builtin_tools::BashJobOwners,
    host::inbox_enqueued_event,
    jobs::JobStart,
    protocol::{
        AgentCancelCause, ContentBlock, Message, MessageId, MessageRole, MessageSource,
        SessionEvent, SessionHeader, SessionId, SessionOrigin, SurfaceOp, SESSION_FORMAT_VERSION,
    },
    session::{Session, SessionError, SessionInspection, SessionPersistence, SessionStore},
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    workspace::{SessionResourceResolver, WorkspaceError, WorkspaceLease, WorkspaceRegistry},
    TessivumError,
};

/// Stable key for the parent-owned subagent capability.
pub fn subagents_service_key() -> ServiceKey {
    ServiceKey::new("harness.subagents", "1")
}

/// A durable continuation descriptor recorded before a child can be reported as started.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentDescriptor {
    pub provider: String,
    pub agent_id: String,
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    #[serde(default)]
    pub mode: SubagentMode,
    pub capabilities: BTreeSet<String>,
    pub options: AgentOptions,
}

/// The durable child mode exposed by the harness RPC contract.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubagentMode {
    #[default]
    OneShot,
    Continuable,
}
/// Activity sampled from the live child driver without restoring a cold session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentActivity {
    Running,
    Inactive,
}

/// Explicit liveness for operator lifecycle controls.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentStatus {
    Running,
    Idle,
    Ready,
}

/// A per-child diagnostic that keeps one damaged durable record from hiding siblings.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentDiagnosticReason {
    Corrupt,
    Unsupported,
    Unavailable,
}

/// A durable direct-child catalog row for the browser compatibility route.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum SubagentListEntry {
    Child {
        id: SessionId,
        mode: SubagentMode,
        activity: SubagentActivity,
        status: SubagentStatus,
        has_children: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Diagnostic {
        id: SessionId,
        reason: SubagentDiagnosticReason,
    },
}

/// One durable catalog row annotated with its stable tree position.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDescendantListEntry {
    #[serde(flatten)]
    pub entry: SubagentListEntry,
    pub parent_session_id: SessionId,
    pub depth: usize,
}

impl SubagentListEntry {
    fn id(&self) -> &SessionId {
        match self {
            Self::Child { id, .. } | Self::Diagnostic { id, .. } => id,
        }
    }
}

/// Browser-facing direct-child catalog with the parent liveness witness.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentCatalog {
    pub entries: Vec<SubagentListEntry>,
    pub parent_available: bool,
}

/// One event returned by a child history read.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentHistoryEntry {
    pub event: SessionEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<SubagentToolEventView>,
}

/// Ephemeral rendering intent for a tool event. It is never persisted.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentToolEventView {
    #[serde(rename = "for")]
    pub target: SubagentToolEventTarget,
    pub view: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentToolEventTarget {
    Call,
    Result,
}

/// Synchronous projection watermark attached only to the tail history page.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionProjectionsBlock {
    pub as_of_seq: i64,
    pub values: BTreeMap<String, Value>,
}

/// Read-only child history request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentHistoryRequest {
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    pub mode: SubagentMode,
    pub before_seq: Option<u64>,
    pub max_messages: Option<usize>,
}

/// Read-only child history response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentHistoryResult {
    pub events: Vec<SubagentHistoryEntry>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projections: Option<SessionProjectionsBlock>,
}

/// A continuable child prompt. `mode` remains present so service callers cannot
/// accidentally bypass the RPC mode rule.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentPromptRequest {
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    pub mode: SubagentMode,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_time_zone: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentPromptResult {
    pub message_id: MessageId,
}

/// A continuable child interrupt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentInterruptRequest {
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    pub mode: SubagentMode,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentInterruptResult {
    pub accepted: bool,
}

/// An operator request to permanently remove one inactive direct child.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentDeleteRequest {
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDeleteResult {
    pub deleted: bool,
}

/// Caller input for one child activation. All fields are untrusted.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubagentStartRequest {
    pub provider: String,
    pub agent_id: String,
    pub child_session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<AgentModeId>,
    #[serde(default)]
    pub mode: SubagentMode,
    pub capabilities: Vec<String>,
    pub options: AgentOptions,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub resume: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_message: Option<Message>,
}

/// Provider input after service-owned parent, capability, and header validation.
#[derive(Clone, Debug)]
pub struct ProviderStart {
    pub descriptor: SubagentDescriptor,
    pub header: SessionHeader,
    pub resume: bool,
}

/// A named provider for child activations. The service performs the capability
/// check before this method is ever called.
#[async_trait]
pub trait SubagentProvider: Send + Sync {
    fn capabilities(&self) -> BTreeSet<String>;

    async fn start(
        &self,
        request: ProviderStart,
        cancellation: CancellationToken,
    ) -> Result<AgentHandle, SubagentError>;
}

/// Native provider that delegates to the existing process-local agent registry.
#[derive(Clone)]
pub struct NativeSubagentProvider {
    agents: AgentRegistry,
    capabilities: BTreeSet<String>,
}

impl NativeSubagentProvider {
    pub fn new(agents: AgentRegistry, capabilities: impl IntoIterator<Item = String>) -> Self {
        Self {
            agents,
            capabilities: capabilities.into_iter().collect(),
        }
    }
}

#[async_trait]
impl SubagentProvider for NativeSubagentProvider {
    fn capabilities(&self) -> BTreeSet<String> {
        self.capabilities.clone()
    }

    async fn start(
        &self,
        request: ProviderStart,
        cancellation: CancellationToken,
    ) -> Result<AgentHandle, SubagentError> {
        let result = if request.resume {
            self.agents
                .resume(
                    request.descriptor.child_session_id,
                    request.descriptor.options,
                    cancellation,
                )
                .await
        } else {
            self.agents
                .create(request.header, request.descriptor.options, cancellation)
                .await
        };
        result.map_err(SubagentError::Agent)
    }
}

/// Lifetime owner for a named provider registration.
pub struct SubagentProviderRegistration {
    providers: Weak<Mutex<ProviderState>>,
    name: String,
    generation: u64,
    closed: AtomicBool,
}

impl fmt::Debug for SubagentProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubagentProviderRegistration")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl SubagentProviderRegistration {
    pub fn unregister(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        let Some(providers) = self.providers.upgrade() else {
            return false;
        };
        let mut providers = lock(&providers);
        if providers
            .providers
            .get(&self.name)
            .is_some_and(|entry| entry.generation == self.generation)
        {
            providers.providers.remove(&self.name);
            true
        } else {
            false
        }
    }
}

impl Drop for SubagentProviderRegistration {
    fn drop(&mut self) {
        self.unregister();
    }
}

/// Successful start acknowledgement. The identifier is unique for the service lifetime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentAcceptance {
    pub acceptance_id: u64,
    pub descriptor: SubagentDescriptor,
}

/// Non-fatal terminal result of one accepted child run.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentRunResult {
    pub status: SubagentRunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<SubagentFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<Vec<ContentBlock>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubagentRunStatus {
    Completed,
    Cancelled,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentFailure {
    pub code: String,
    pub message: String,
}

/// Fatal API misuse and admission failures. A settled child error is represented
/// by [`SubagentRunResult`], never this type.
#[derive(Debug, Error)]
pub enum SubagentError {
    #[error("provider name must not be empty")]
    InvalidProviderName,
    #[error("agent ID must not be empty")]
    InvalidAgentId,
    #[error("child session ID must not be empty")]
    InvalidChildSessionId,
    #[error("child session cannot equal its parent")]
    SelfParent,
    #[error("subagent capability names must not be empty")]
    InvalidCapability,
    #[error("subagent options are invalid: {0}")]
    InvalidOptions(&'static str),
    #[error("subagent cwd overrides are unsupported")]
    CwdOverrideUnsupported,
    #[error("parent session is required and must be live")]
    ParentRequired,
    #[error("subagent parent attachment requires a Tokio runtime")]
    ParentRuntimeRequired,
    #[error("provider {0:?} is not registered")]
    ProviderNotFound(String),
    #[error("provider {provider:?} does not grant capability {capability:?}")]
    CapabilityDenied {
        provider: String,
        capability: String,
    },
    #[error("a provider is already registered as {0:?}")]
    DuplicateProvider(String),
    #[error("start was cancelled before acceptance")]
    CancelledBeforeAcceptance,
    #[error("resumed child does not name this direct parent")]
    ResumeParentMismatch,
    #[error("resumed child does not share this parent's workspace or cwd")]
    ResumeWorkspaceMismatch,
    #[error("a child activation can run only once")]
    AlreadyRun,
    #[error("child session does not name the supplied direct parent")]
    DirectParentMismatch,
    #[error("child session mode does not match the supplied address")]
    ModeMismatch,
    #[error("this operation requires a continuable child")]
    ContinuableRequired,
    #[error("continuable child is no longer available for delivery")]
    DeliveryUnavailable,
    #[error("prompt was not durably admitted before the child became idle")]
    PromptNotDurable,
    #[error("subagent still has a live agent")]
    DeleteActive,
    #[error("subagent has direct descendants; delete them first")]
    DeleteHasChildren,
    #[error("subagent is not a deletable listed direct child")]
    DeleteUnavailable,
    #[error("subagent operation was cancelled")]
    Cancelled,
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Protocol(#[from] TessivumError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
}

impl SubagentError {
    /// Stable machine-readable code for host and transport boundaries.
    pub fn code(&self) -> &str {
        match self {
            Self::ParentRequired => "SUBAGENT_PARENT_REQUIRED",
            Self::DirectParentMismatch | Self::ResumeParentMismatch => "SUBAGENT_PARENT_MISMATCH",
            Self::ModeMismatch => "SUBAGENT_MODE_MISMATCH",
            Self::ContinuableRequired => "SUBAGENT_MODE_UNSUPPORTED",
            Self::DeliveryUnavailable | Self::PromptNotDurable => "SUBAGENT_DELIVERY_UNAVAILABLE",
            Self::Cancelled
            | Self::CancelledBeforeAcceptance
            | Self::Agent(AgentError::Cancelled) => "CANCELLED",
            Self::Agent(AgentError::Disposed) => "SUBAGENT_DELIVERY_UNAVAILABLE",
            Self::Agent(_) => "AGENT_BUSY",
            Self::Session(error) => error.code(),
            Self::Protocol(error) => &error.code,
            Self::Workspace(error) => error.code(),
            Self::AlreadyRun => "SUBAGENT_NOT_RESUMABLE",
            Self::DeleteActive => "SUBAGENT_DELETE_ACTIVE",
            Self::DeleteHasChildren => "SUBAGENT_DELETE_HAS_CHILDREN",
            Self::DeleteUnavailable => "SUBAGENT_DELETE_UNAVAILABLE",
            _ => "SUBAGENT_ERROR",
        }
    }
}

/// Lifetime owner for model-facing subagent control tools.
pub struct SubagentTools {
    _registrations: Vec<ToolRegistration>,
}

impl SubagentTools {
    pub fn install(runtime: &ToolRuntime, service: SubagentService) -> Result<Self, TessivumError> {
        let mut registrations = Vec::with_capacity(3);
        for action in [
            SubagentToolAction::List,
            SubagentToolAction::Send,
            SubagentToolAction::Interrupt,
        ] {
            registrations.push(runtime.register(ToolDefinition::new(
                action.name(),
                action.description(),
                action.schema(),
                SubagentTool {
                    service: service.clone(),
                    action,
                },
            ))?);
        }
        Ok(Self {
            _registrations: registrations,
        })
    }
}

/// Model-facing delegation tools backed by the native provider.
pub struct SubagentDelegationTools {
    _registrations: Vec<ToolRegistration>,
}

impl SubagentDelegationTools {
    pub fn install(
        runtime: &ToolRuntime,
        service: SubagentService,
        job_owners: BashJobOwners,
    ) -> Result<Self, TessivumError> {
        let mut registrations = Vec::with_capacity(3);
        let schema = json!({
            "type": "object",
            "properties": {
                "description": {"type": "string"},
                "prompt": {"type": "string"},
                "run_in_background": {"type": "boolean"}
            },
            "required": ["description", "prompt"],
            "additionalProperties": false
        });
        for (name, kind, description) in [
            (
                "subagent",
                DelegationKind::Subagent,
                "Delegate to a continuable native child. It starts as a result-delivering background job by default and remains available to list, message, or interrupt; set run_in_background false to wait inline.",
            ),
            (
                "subagent_fork",
                DelegationKind::Fork,
                "Delegate one self-contained task to a fresh native child seeded with this conversation's completed prefix.",
            ),
        ] {
            registrations.push(runtime.register(ToolDefinition::new(
                name,
                description,
                schema.clone(),
                DelegationTool { service: service.clone(), job_owners: Arc::clone(&job_owners), kind },
            ))?);
        }
        registrations.push(runtime.register(ToolDefinition::new(
            "ralph",
            "Run fresh autonomous child rounds for a fixed objective until a worker reports completion or blocking, or the round limit is reached.",
            json!({
                "type": "object",
                "properties": {
                    "objective": {"type": "string"},
                    "maxRounds": {"type": "integer"}
                },
                "required": ["objective"],
                "additionalProperties": false
            }),
            DelegationTool {
                service,
                job_owners,
                kind: DelegationKind::Ralph,
            },
        ))?);
        Ok(Self {
            _registrations: registrations,
        })
    }
}

#[derive(Clone, Copy)]
enum DelegationKind {
    Subagent,
    Fork,
    Ralph,
}

#[derive(Clone)]
struct DelegationTool {
    service: SubagentService,
    job_owners: BashJobOwners,
    kind: DelegationKind,
}

#[async_trait]
impl ToolHandler for DelegationTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let agent = Arc::new(
            self.service
                .inner
                .agents
                .get(&context.session)
                .ok_or_else(|| subagent_tool_error("subagent requires a live parent agent"))?,
        );
        let options = agent.options();
        if matches!(self.kind, DelegationKind::Ralph) {
            let objective = required_tool_string(&arguments, "objective")?;
            let max_rounds = ralph_max_rounds(&arguments)?;
            let parent = self.service.attach(agent).map_err(subagent_error)?;
            let (result, child_id) =
                run_ralph(parent, objective, max_rounds, options, context.cancellation)
                    .await
                    .map_err(subagent_tool_error)?;
            return Ok(ToolOutput::new(
                vec![ContentBlock::Text {
                    text: serde_json::to_string(&result).expect("Ralph result serializes"),
                }],
                false,
                json!({"childSessionId": child_id, "result": result}),
            ));
        }

        let label = required_tool_string(&arguments, "description")?;
        let prompt = required_tool_string(&arguments, "prompt")?;
        let child_id = SessionId::random();
        let (mode, seed_parent_prefix, run_in_background) = match self.kind {
            DelegationKind::Subagent => (
                SubagentMode::Continuable,
                false,
                arguments
                    .get("run_in_background")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            ),
            DelegationKind::Fork => (
                SubagentMode::OneShot,
                true,
                arguments
                    .get("run_in_background")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
            DelegationKind::Ralph => unreachable!("Ralph is handled above"),
        };
        let request = SubagentStartRequest {
            provider: "native".into(),
            agent_id: label.clone(),
            child_session_id: child_id.clone(),
            agent_mode: None,
            mode,
            capabilities: Vec::new(),
            options,
            created_at: subagent_now(),
            cwd: None,
            resume: false,
            initial_message: Some(Message {
                id: MessageId::random(),
                role: MessageRole::User,
                content: vec![ContentBlock::Text { text: prompt }],
                source: MessageSource::User {
                    client_time_zone: None,
                },
            }),
        };

        if mode == SubagentMode::Continuable {
            let parent = self
                .service
                .continuable_parent(agent)
                .map_err(subagent_error)?;
            if run_in_background {
                let owner = {
                    self.job_owners
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .get(&context.session)
                        .cloned()
                }
                .ok_or_else(|| {
                    subagent_tool_error("the session has no live background-job owner")
                })?;
                let (_, activation) = parent
                    .start(request, context.cancellation.clone())
                    .await
                    .map_err(subagent_error)?;
                let job_activation = activation.clone();
                let job_child_id = child_id.clone();
                let job = owner.start(JobStart::new(
                    "subagent",
                    subagent_job_label(&label),
                    64 * 1024,
                    move |control| {
                        let activation = job_activation.clone();
                        let child_id = job_child_id.clone();
                        async move {
                            let result =
                                wait_continuable_activation(&activation, control.cancellation())
                                    .await?;
                            for block in
                                result.last_assistant_message.as_deref().unwrap_or_default()
                            {
                                if let ContentBlock::Text { text } = block {
                                    control.write_text(text);
                                }
                            }
                            Ok(json!({"childSessionId": child_id}))
                        }
                    },
                ));
                return match job {
                    Ok(job) => Ok(ToolOutput::new(
                        vec![ContentBlock::Text {
                            text: format!("Background subagent started with job id {}.", job.id),
                        }],
                        false,
                        serde_json::to_value(job).expect("job snapshot serializes"),
                    )),
                    Err(error) => {
                        let _ = activation.dispose().await;
                        Err(subagent_tool_error(error.to_string()))
                    }
                };
            }
            let result = run_continuable_delegation(&parent, request, context.cancellation)
                .await
                .map_err(subagent_tool_error)?;
            return Ok(ToolOutput::new(
                result.last_assistant_message.unwrap_or_default(),
                false,
                json!({"childSessionId": child_id}),
            ));
        }

        let parent = self.service.attach(agent).map_err(subagent_error)?;
        if run_in_background {
            let owner = {
                self.job_owners
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .get(&context.session)
                    .cloned()
            };
            let Some(owner) = owner else {
                parent.dispose().await;
                return Err(subagent_tool_error(
                    "the session has no live background-job owner",
                ));
            };
            let job_child_id = child_id.clone();
            let job_parent = parent.clone();
            let job = owner.start(JobStart::new(
                "subagent",
                subagent_job_label(&label),
                64 * 1024,
                move |control| {
                    let parent = job_parent.clone();
                    let request = request.clone();
                    let child_id = job_child_id.clone();
                    async move {
                        let result = run_one_shot_delegation(
                            parent,
                            request,
                            control.cancellation(),
                            seed_parent_prefix,
                        )
                        .await?;
                        for block in result.last_assistant_message.as_deref().unwrap_or_default() {
                            if let ContentBlock::Text { text } = block {
                                control.write_text(text);
                            }
                        }
                        Ok(json!({"childSessionId": child_id}))
                    }
                },
            ));
            return match job {
                Ok(job) => Ok(ToolOutput::new(
                    vec![ContentBlock::Text {
                        text: format!("Background subagent started with job id {}.", job.id),
                    }],
                    false,
                    serde_json::to_value(job).expect("job snapshot serializes"),
                )),
                Err(error) => {
                    parent.dispose().await;
                    Err(subagent_tool_error(error.to_string()))
                }
            };
        }
        let result =
            run_one_shot_delegation(parent, request, context.cancellation, seed_parent_prefix)
                .await
                .map_err(subagent_tool_error)?;
        Ok(ToolOutput::new(
            result.last_assistant_message.unwrap_or_default(),
            false,
            json!({"childSessionId": child_id}),
        ))
    }
}

const MAX_RALPH_ROUNDS: u64 = 64;

async fn run_one_shot_delegation(
    parent: SubagentParent,
    request: SubagentStartRequest,
    cancellation: CancellationToken,
    seed_parent_prefix: bool,
) -> Result<SubagentRunResult, String> {
    let result = run_one_shot_activation(&parent, request, cancellation, seed_parent_prefix).await;
    parent.dispose().await;
    result
}

async fn run_one_shot_activation(
    parent: &SubagentParent,
    request: SubagentStartRequest,
    cancellation: CancellationToken,
    seed_parent_prefix: bool,
) -> Result<SubagentRunResult, String> {
    let (_, activation) = if seed_parent_prefix {
        parent.start_forked(request, cancellation.clone()).await
    } else {
        parent.start(request, cancellation.clone()).await
    }
    .map_err(|error| error.to_string())?;
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => activation.dispose().await,
        result = activation.run() => result,
    }
    .map_err(|error| error.to_string())?;
    completed_delegation(result)
}

async fn run_continuable_delegation(
    parent: &SubagentParent,
    request: SubagentStartRequest,
    cancellation: CancellationToken,
) -> Result<SubagentRunResult, String> {
    let (_, activation) = parent
        .start(request, cancellation.clone())
        .await
        .map_err(|error| error.to_string())?;
    wait_continuable_activation(&activation, cancellation).await
}

async fn wait_continuable_activation(
    activation: &SubagentActivation,
    cancellation: CancellationToken,
) -> Result<SubagentRunResult, String> {
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => activation.dispose().await,
        result = activation.wait_for_idle() => result,
    };
    match result {
        Ok(result) if result.status == SubagentRunStatus::Completed => Ok(result),
        Ok(result) => {
            let _ = activation.dispose().await;
            completed_delegation(result)
        }
        Err(error) => {
            let _ = activation.dispose().await;
            Err(error.to_string())
        }
    }
}

fn completed_delegation(result: SubagentRunResult) -> Result<SubagentRunResult, String> {
    if result.status == SubagentRunStatus::Completed {
        Ok(result)
    } else {
        Err(result.error.as_ref().map_or_else(
            || "subagent did not complete".into(),
            |error| error.message.clone(),
        ))
    }
}

async fn run_ralph(
    parent: SubagentParent,
    objective: String,
    max_rounds: u64,
    options: AgentOptions,
    cancellation: CancellationToken,
) -> Result<(RalphRunResult, SessionId), String> {
    let result = async {
        let mut handoff = None;
        for round in 1..=max_rounds {
            let child_id = SessionId::random();
            let request = SubagentStartRequest {
                provider: "native".into(),
                agent_id: format!("Ralph round {round}"),
                child_session_id: child_id.clone(),
                agent_mode: None,
                mode: SubagentMode::OneShot,
                capabilities: Vec::new(),
                options: options.clone(),
                created_at: subagent_now(),
                cwd: None,
                resume: false,
                initial_message: Some(Message {
                    id: MessageId::random(),
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text {
                        text: ralph_prompt(&objective, round, max_rounds, handoff.as_ref()),
                    }],
                    source: MessageSource::User {
                        client_time_zone: None,
                    },
                }),
            };
            let run =
                run_one_shot_activation(&parent, request, cancellation.clone(), false).await?;
            let report = ralph_report(&run)?;
            let status = match report.status {
                RalphRoundStatus::Complete => Some(RalphRunStatus::Complete),
                RalphRoundStatus::Blocked => Some(RalphRunStatus::Blocked),
                RalphRoundStatus::Continue if round == max_rounds => {
                    Some(RalphRunStatus::BudgetLimited)
                }
                RalphRoundStatus::Continue => None,
            };
            if let Some(status) = status {
                return Ok((
                    RalphRunResult {
                        status,
                        rounds_started: round,
                        report,
                    },
                    child_id,
                ));
            }
            handoff = Some(report);
        }
        unreachable!("maxRounds is positive")
    }
    .await;
    parent.dispose().await;
    result
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum RalphRoundStatus {
    Continue,
    Complete,
    Blocked,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
enum RalphRunStatus {
    Complete,
    Blocked,
    BudgetLimited,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RalphRunResult {
    status: RalphRunStatus,
    rounds_started: u64,
    report: RalphRoundReport,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RalphRoundReport {
    status: RalphRoundStatus,
    summary: String,
    evidence: Vec<String>,
    next_steps: Vec<String>,
    blocker: String,
}

fn ralph_prompt(
    objective: &str,
    round: u64,
    max_rounds: u64,
    handoff: Option<&RalphRoundReport>,
) -> String {
    let handoff = handoff.map_or_else(
        || "No prior round report is available.".into(),
        |report| serde_json::to_string(report).expect("Ralph report serializes"),
    );
    format!(
        "Work autonomously on this objective in a fresh agent round ({round}/{max_rounds}):\n\n{objective}\n\nPrior round report:\n{handoff}\n\nReturn only a JSON object with status (continue, complete, or blocked), summary, evidence, nextSteps, and blocker."
    )
}

fn ralph_report(result: &SubagentRunResult) -> Result<RalphRoundReport, String> {
    let text = result
        .last_assistant_message
        .as_ref()
        .and_then(|blocks| {
            blocks.iter().rev().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text),
                _ => None,
            })
        })
        .ok_or_else(|| "Ralph round did not return a structured report".to_string())?;
    let report: RalphRoundReport = serde_json::from_str(text)
        .map_err(|_| "Ralph round did not return a structured report".to_string())?;
    if !normalized_ralph_text(&report.summary)
        || !report
            .evidence
            .iter()
            .all(|value| normalized_ralph_text(value))
        || !report
            .next_steps
            .iter()
            .all(|value| normalized_ralph_text(value))
        || report.blocker != report.blocker.trim()
    {
        return Err(
            "Ralph round report strings must be normalized and non-empty except blocker".into(),
        );
    }
    match report.status {
        RalphRoundStatus::Continue
            if report.next_steps.is_empty() || !report.blocker.is_empty() =>
        {
            Err("a continuing Ralph report needs nextSteps and an empty blocker".into())
        }
        RalphRoundStatus::Complete
            if report.evidence.is_empty()
                || !report.next_steps.is_empty()
                || !report.blocker.is_empty() =>
        {
            Err("a complete Ralph report needs evidence, no nextSteps, and an empty blocker".into())
        }
        RalphRoundStatus::Blocked if !normalized_ralph_text(&report.blocker) => {
            Err("a blocked Ralph report needs a concrete blocker".into())
        }
        _ => Ok(report),
    }
}

fn normalized_ralph_text(value: &str) -> bool {
    !value.is_empty() && value == value.trim()
}

fn ralph_max_rounds(arguments: &Value) -> Result<u64, TessivumError> {
    match arguments.get("maxRounds") {
        None => Ok(MAX_RALPH_ROUNDS),
        Some(value) => value
            .as_u64()
            .filter(|rounds| (1..=MAX_RALPH_ROUNDS).contains(rounds))
            .ok_or_else(|| {
                subagent_tool_error(format!(
                    "maxRounds must be a positive safe integer no greater than {MAX_RALPH_ROUNDS}"
                ))
            }),
    }
}

fn subagent_job_label(label: &str) -> String {
    label.chars().take(256).collect()
}

fn required_tool_string(arguments: &Value, key: &str) -> Result<String, TessivumError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| subagent_tool_error(format!("{key} must be a non-empty string")))
}

fn subagent_tool_error(message: impl Into<String>) -> TessivumError {
    TessivumError::new("SUBAGENT_TOOL_FAILED", message, "subagent", Value::Null)
}

fn subagent_error(error: SubagentError) -> TessivumError {
    subagent_tool_error(error.to_string())
}

fn subagent_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Copy)]
enum SubagentToolAction {
    List,
    Send,
    Interrupt,
}

impl SubagentToolAction {
    fn name(self) -> &'static str {
        match self {
            Self::List => "list_agents",
            Self::Send => "send_message",
            Self::Interrupt => "interrupt_agent",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::List => "Lists your continuable child agents or descendants by durable id.",
            Self::Send => "Sends one follow-up message to a continuable direct child agent.",
            Self::Interrupt => "Requests cancellation of a running descendant agent.",
        }
    }

    fn schema(self) -> Value {
        match self {
            Self::List => json!({
                "type": "object",
                "properties": {"scope": {"type": "string", "enum": ["children", "descendants"]}},
                "additionalProperties": false
            }),
            Self::Send => json!({
                "type": "object",
                "properties": {
                    "subagent_id": {"type": "string"},
                    "message": {"type": "string"}
                },
                "required": ["subagent_id", "message"],
                "additionalProperties": false
            }),
            Self::Interrupt => json!({
                "type": "object",
                "properties": {"agent_id": {"type": "string"}},
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListAgentsInput {
    scope: Option<ListAgentsScope>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ListAgentsScope {
    Children,
    Descendants,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendMessageInput {
    subagent_id: SessionId,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InterruptAgentInput {
    agent_id: SessionId,
}

#[derive(Serialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
enum ModelSubagentListEntry {
    Child {
        id: SessionId,
        label: String,
        status: String,
        activity: SubagentActivity,
        mode: SubagentMode,
        has_children: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent: Option<SessionId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        depth: Option<usize>,
    },
    Diagnostic {
        id: SessionId,
        reason: SubagentDiagnosticReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent: Option<SessionId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        depth: Option<usize>,
    },
}

impl ModelSubagentListEntry {
    fn render(&self) -> String {
        match self {
            Self::Child {
                id,
                label,
                status,
                parent,
                depth,
                ..
            } => format!(
                "{id} [{status}]{} — {label}",
                parent.as_ref().map_or_else(String::new, |parent| format!(
                    " parent={parent} depth={}",
                    depth.expect("descendant position is complete")
                ))
            ),
            Self::Diagnostic {
                id,
                reason,
                parent,
                depth,
            } => {
                let reason = match reason {
                    SubagentDiagnosticReason::Corrupt => "corrupt",
                    SubagentDiagnosticReason::Unsupported => "unsupported",
                    SubagentDiagnosticReason::Unavailable => "unavailable",
                };
                format!(
                    "{id} [diagnostic: {reason}]{}",
                    parent.as_ref().map_or_else(String::new, |parent| format!(
                        " parent={parent} depth={}",
                        depth.expect("descendant position is complete")
                    ))
                )
            }
        }
    }
}

#[derive(Clone)]
struct SubagentTool {
    service: SubagentService,
    action: SubagentToolAction,
}

#[async_trait]
impl ToolHandler for SubagentTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        match self.action {
            SubagentToolAction::List => self.list(context, arguments).await,
            SubagentToolAction::Send => self.send(context, arguments).await,
            SubagentToolAction::Interrupt => self.interrupt(context, arguments).await,
        }
    }
}

impl SubagentTool {
    fn error(error: SubagentError) -> TessivumError {
        TessivumError::new(error.code(), error.to_string(), "subagent", Value::Null)
    }

    fn project(
        &self,
        entry: SubagentListEntry,
        parent: Option<SessionId>,
        depth: Option<usize>,
    ) -> Option<ModelSubagentListEntry> {
        match entry {
            SubagentListEntry::Diagnostic { id, reason } => {
                Some(ModelSubagentListEntry::Diagnostic {
                    id,
                    reason,
                    parent,
                    depth,
                })
            }
            SubagentListEntry::Child {
                id,
                mode: SubagentMode::Continuable,
                activity,
                status,
                has_children,
                label,
            } => Some(ModelSubagentListEntry::Child {
                status: match status {
                    SubagentStatus::Running => "running",
                    SubagentStatus::Idle => "idle",
                    SubagentStatus::Ready => "ready",
                }
                .into(),
                id,
                label: label.unwrap_or_default(),
                activity,
                mode: SubagentMode::Continuable,
                has_children,
                parent,
                depth,
            }),
            SubagentListEntry::Child { .. } => None,
        }
    }

    async fn list(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let request: ListAgentsInput = serde_json::from_value(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_SUBAGENT_CONTROL",
                "list_agents input is invalid",
                "subagent",
                Value::Null,
            )
        })?;
        self.service
            .require_live_parent_session(&context.session)
            .map_err(Self::error)?;
        let entries: Vec<ModelSubagentListEntry> =
            match request.scope.unwrap_or(ListAgentsScope::Children) {
                ListAgentsScope::Children => self
                    .service
                    .list(context.session, context.cancellation)
                    .await
                    .map_err(Self::error)?
                    .into_iter()
                    .filter_map(|entry| self.project(entry, None, None))
                    .collect(),
                ListAgentsScope::Descendants => self
                    .service
                    .list_descendants(context.session, context.cancellation)
                    .await
                    .map_err(Self::error)?
                    .into_iter()
                    .filter_map(|entry| {
                        self.project(
                            entry.entry,
                            Some(entry.parent_session_id),
                            Some(entry.depth),
                        )
                    })
                    .collect(),
            };
        let text = if entries.is_empty() {
            "(no subagents)".into()
        } else {
            entries
                .iter()
                .map(ModelSubagentListEntry::render)
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ToolOutput::new(
            vec![ContentBlock::Text { text }],
            false,
            serde_json::to_value(entries).expect("subagent list output serializes"),
        ))
    }

    async fn send(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let request: SendMessageInput = serde_json::from_value(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_SUBAGENT_CONTROL",
                "send_message input is invalid",
                "subagent",
                Value::Null,
            )
        })?;
        let result = self
            .service
            .prompt(
                SubagentPromptRequest {
                    parent_session_id: context.session,
                    child_session_id: request.subagent_id.clone(),
                    mode: SubagentMode::Continuable,
                    content: vec![ContentBlock::Text {
                        text: request.message,
                    }],
                    client_time_zone: None,
                },
                context.cancellation,
            )
            .await
            .map_err(Self::error)?;
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: format!(
                    "message queued as the next turn for subagent {}",
                    request.subagent_id
                ),
            }],
            false,
            json!({"messageId": result.message_id}),
        ))
    }

    async fn interrupt(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let request: InterruptAgentInput = serde_json::from_value(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_SUBAGENT_CONTROL",
                "interrupt_agent input is invalid",
                "subagent",
                Value::Null,
            )
        })?;
        self.service
            .interrupt(
                SubagentInterruptRequest {
                    parent_session_id: context.session,
                    child_session_id: request.agent_id.clone(),
                    mode: SubagentMode::Continuable,
                },
                context.cancellation,
            )
            .await
            .map_err(Self::error)?;
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: format!("interrupt requested for agent {}", request.agent_id),
            }],
            false,
            json!({"accepted": true}),
        ))
    }
}

struct ParentAdmissions {
    closing: bool,
    pending: usize,
    children: Vec<SubagentActivation>,
    late_children: Vec<SubagentActivation>,
}

struct SubagentParentState {
    service: Weak<SubagentInner>,
    parent: Arc<AgentHandle>,
    admissions: Mutex<ParentAdmissions>,
    quiesced: Notify,
    cleanup_finished: Notify,
    watcher_closed: AtomicBool,
    watcher_closed_notify: Notify,
    cleanup_started: AtomicBool,
    cleanup_done: AtomicBool,
    cleanup_results: Mutex<Vec<SubagentRunResult>>,
    runtime: tokio::runtime::Handle,
}

struct SubagentAdmissionPermit {
    state: Arc<SubagentParentState>,
    released: bool,
}

impl SubagentAdmissionPermit {
    fn admit_or_queue(&self, activation: SubagentActivation) -> bool {
        let mut admissions = self.state.admissions.lock();
        if admissions.closing
            || self.state.watcher_closed.load(Ordering::Acquire)
            || self.state.parent.is_disposed()
            || self.state.parent.cancellation().is_cancelled()
        {
            admissions.closing = true;
            admissions.late_children.push(activation);
            false
        } else {
            admissions.children.push(activation);
            true
        }
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let wake = {
            let mut admissions = self.state.admissions.lock();
            admissions.pending -= 1;
            admissions.closing && admissions.pending == 0
        };
        if wake {
            self.state.quiesced.notify_waiters();
        }
    }
}

impl Drop for SubagentAdmissionPermit {
    fn drop(&mut self) {
        self.release();
    }
}

/// An opaque capability tied to one live parent agent generation. Possession,
/// rather than a caller-supplied session ID, authorizes child operations.
#[derive(Clone)]
pub struct SubagentParent {
    state: Arc<SubagentParentState>,
}

impl fmt::Debug for SubagentParent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubagentParent")
            .field("parent", &self.state.parent.id())
            .finish_non_exhaustive()
    }
}

impl SubagentParent {
    /// Starts a child owned by this parent generation.
    pub async fn start(
        &self,
        request: SubagentStartRequest,
        cancellation: CancellationToken,
    ) -> Result<(SubagentAcceptance, SubagentActivation), SubagentError> {
        self.start_inner(request, cancellation, None, false, false)
            .await
    }

    /// Starts a fresh one-shot child seeded from this parent's completed prefix.
    pub async fn start_forked(
        &self,
        request: SubagentStartRequest,
        cancellation: CancellationToken,
    ) -> Result<(SubagentAcceptance, SubagentActivation), SubagentError> {
        self.start_inner(request, cancellation, None, true, false)
            .await
    }
    /// Starts a continuable child seeded from this parent's completed prefix.
    pub async fn start_seeded_continuable(
        &self,
        request: SubagentStartRequest,
        cancellation: CancellationToken,
    ) -> Result<(SubagentAcceptance, SubagentActivation), SubagentError> {
        self.start_inner(request, cancellation, None, true, true)
            .await
    }

    pub(crate) async fn start_seeded_continuable_with_seed(
        &self,
        request: SubagentStartRequest,
        seed_events: Vec<SessionEvent>,
        cancellation: CancellationToken,
    ) -> Result<(SubagentAcceptance, SubagentActivation), SubagentError> {
        self.start_inner(request, cancellation, Some(seed_events), false, true)
            .await
    }

    async fn start_inner(
        &self,
        request: SubagentStartRequest,
        cancellation: CancellationToken,
        seed_events: Option<Vec<SessionEvent>>,
        seed_parent_prefix: bool,
        allow_continuable_seed: bool,
    ) -> Result<(SubagentAcceptance, SubagentActivation), SubagentError> {
        let mut permit = match self.state.reserve_admission() {
            Some(permit) => permit,
            None => {
                self.state.begin_cleanup();
                return Err(SubagentError::ParentRequired);
            }
        };
        let service = self.service()?;
        let (acceptance, activation) = service
            .start(
                &self.state.parent,
                request,
                cancellation,
                seed_events,
                seed_parent_prefix,
                allow_continuable_seed,
            )
            .await?;
        let admitted = permit.admit_or_queue(activation.clone());
        permit.release();
        if !admitted {
            self.state.begin_cleanup();
            return Err(SubagentError::ParentRequired);
        }
        Ok((acceptance, activation))
    }

    /// Lists this generation's direct durable children.
    pub async fn children(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SubagentError> {
        self.service()?
            .children(&self.state.parent, cancellation)
            .await
    }

    /// Lists this generation's durable descendants.
    pub async fn descendants(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SubagentError> {
        self.service()?
            .descendants(&self.state.parent, cancellation)
            .await
    }

    fn service(&self) -> Result<Arc<SubagentInner>, SubagentError> {
        let service = self
            .state
            .service
            .upgrade()
            .ok_or(SubagentError::ParentRequired)?;
        service.require_live_parent(&self.state.parent)?;
        Ok(service)
    }

    /// Closes this parent capability and joins every accepted direct child.
    pub async fn dispose(&self) -> Vec<SubagentRunResult> {
        self.state.close_and_dispose().await
    }

    pub(crate) fn begin_dispose(&self) {
        self.state.close_watcher();
        self.state.begin_cleanup();
    }
}

impl SubagentParentState {
    fn is_open(&self) -> bool {
        !self.watcher_closed.load(Ordering::Acquire)
            && !self.parent.is_disposed()
            && !self.parent.cancellation().is_cancelled()
            && !self.admissions.lock().closing
    }

    fn close_watcher(&self) {
        if !self.watcher_closed.swap(true, Ordering::AcqRel) {
            self.watcher_closed_notify.notify_waiters();
        }
    }

    fn reserve_admission(self: &Arc<Self>) -> Option<SubagentAdmissionPermit> {
        let mut admissions = self.admissions.lock();
        if admissions.closing
            || self.watcher_closed.load(Ordering::Acquire)
            || self.parent.is_disposed()
            || self.parent.cancellation().is_cancelled()
        {
            admissions.closing = true;
            return None;
        }
        admissions.pending += 1;
        Some(SubagentAdmissionPermit {
            state: Arc::clone(self),
            released: false,
        })
    }

    fn begin_cleanup(self: &Arc<Self>) {
        if self.cleanup_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = Arc::clone(self);
        self.runtime.spawn(async move {
            let results = cleanup_parent_children(Arc::clone(&state)).await;
            *state.cleanup_results.lock() = results;
            state.cleanup_done.store(true, Ordering::Release);
            state.cleanup_finished.notify_waiters();
        });
    }

    async fn close_and_dispose(self: &Arc<Self>) -> Vec<SubagentRunResult> {
        self.close_watcher();
        self.begin_cleanup();
        loop {
            let notified = self.cleanup_finished.notified();
            if self.cleanup_done.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
        self.cleanup_results.lock().clone()
    }
}

async fn cleanup_parent_children(state: Arc<SubagentParentState>) -> Vec<SubagentRunResult> {
    let children = {
        let mut admissions = state.admissions.lock();
        admissions.closing = true;
        std::mem::take(&mut admissions.children)
    };
    let mut results = Vec::with_capacity(children.len());
    for child in children {
        if let Ok(result) = child.dispose().await {
            results.push(result);
        }
    }
    loop {
        let notified = state.quiesced.notified();
        if state.admissions.lock().pending == 0 {
            break;
        }
        notified.await;
    }
    let late_children = std::mem::take(&mut state.admissions.lock().late_children);
    for child in late_children {
        if let Ok(result) = child.dispose().await {
            results.push(result);
        }
    }
    results
}
struct ChildState {
    acceptance: SubagentAcceptance,
    parent: Arc<Session>,
    cancellation: CancellationToken,
    agent: AsyncMutex<Option<Arc<AgentHandle>>>,
    operation: AsyncMutex<()>,
    terminal: Mutex<Option<SubagentRunResult>>,
    service: Weak<SubagentInner>,
}

struct ProviderEntry {
    generation: u64,
    provider: Arc<dyn SubagentProvider>,
}

#[derive(Default)]
struct ProviderState {
    next_generation: u64,
    providers: BTreeMap<String, ProviderEntry>,
}

impl ChildState {
    fn terminal(&self) -> Option<SubagentRunResult> {
        lock(&self.terminal).clone()
    }

    fn request_cancel(&self, cause: AgentCancelCause) -> bool {
        let agent = self
            .agent
            .try_lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(Arc::clone));
        if let Some(agent) = agent {
            agent.cancel_including_idle(cause, false)
        } else {
            self.cancellation.cancel();
            false
        }
    }

    async fn live_agent(&self) -> Result<Arc<AgentHandle>, SubagentError> {
        self.agent
            .lock()
            .await
            .as_ref()
            .map(Arc::clone)
            .ok_or(SubagentError::AlreadyRun)
    }

    async fn followup(&self, message: Message) -> Result<(), SubagentError> {
        self.live_agent().await?.followup(message).await?;
        Ok(())
    }

    fn interrupt(&self) -> bool {
        self.request_cancel(AgentCancelCause::Parent)
    }

    async fn run(&self) -> Result<SubagentRunResult, SubagentError> {
        let _operation = self.operation.lock().await;
        if self.terminal().is_some() {
            return Err(SubagentError::AlreadyRun);
        }
        // Keep the agent mutex free while the runtime calls arbitrary code so an
        // interrupt can acquire it and mark the agent's cancellation cause.
        let agent = self.live_agent().await?;
        let result = match agent.when_idle().await {
            Ok(()) if agent.cancellation().is_cancelled() => cancelled_result(),
            Ok(()) => completed_result(&agent.session()),
            Err(AgentError::Cancelled) => cancelled_result(),
            Err(error) => error_result("AGENT_RUNTIME_FAILED", error.to_string()),
        };
        Ok(self.finish(result).await)
    }

    /// Waits for this turn without retiring a continuable child.
    async fn wait_for_idle(&self) -> Result<SubagentRunResult, SubagentError> {
        let _operation = self.operation.lock().await;
        if self.terminal().is_some() {
            return Err(SubagentError::AlreadyRun);
        }
        let agent = self.live_agent().await?;
        Ok(match agent.when_idle().await {
            Ok(()) if agent.cancellation().is_cancelled() => cancelled_result(),
            Ok(()) => completed_result(&agent.session()),
            Err(AgentError::Cancelled) => cancelled_result(),
            Err(error) => error_result("AGENT_RUNTIME_FAILED", error.to_string()),
        })
    }

    async fn dispose(&self) -> SubagentRunResult {
        self.request_cancel(AgentCancelCause::Disposed);
        let _operation = self.operation.lock().await;
        if let Some(result) = self.terminal() {
            return result;
        }
        self.finish(cancelled_result()).await
    }

    async fn finish(&self, mut result: SubagentRunResult) -> SubagentRunResult {
        let agent = self.agent.lock().await.take();
        if result.last_assistant_message.is_none() {
            result.last_assistant_message = agent
                .as_ref()
                .and_then(|agent| last_assistant_message(&agent.session()));
        }
        if let Some(agent) = agent {
            if let Err(error) = agent.dispose().await {
                result = error_result("AGENT_DISPOSE_FAILED", error.to_string());
            }
        }
        if let Err(error) = append_event(
            &self.parent,
            "subagent/contained-end",
            json!({
                "acceptanceId": self.acceptance.acceptance_id,
                "childSessionId": self.acceptance.descriptor.child_session_id,
                "status": result.status,
                "error": result.error,
                "lastAssistantMessage": result.last_assistant_message,
            }),
        )
        .await
        {
            result = error_result("SUBAGENT_EVENT_APPEND_FAILED", error.to_string());
        }
        *lock(&self.terminal) = Some(result.clone());
        if let Some(service) = self.service.upgrade() {
            lock(&service.children).remove(&self.acceptance.acceptance_id);
        }
        result
    }
}

/// An accepted child capability. Its private state, not an exposed numeric ID,
/// authorizes follow-up, interruption, and disposal.
#[derive(Clone)]
pub struct SubagentActivation {
    state: Arc<ChildState>,
}

impl SubagentActivation {
    pub fn acceptance_id(&self) -> u64 {
        self.state.acceptance.acceptance_id
    }

    pub async fn followup(&self, message: Message) -> Result<(), SubagentError> {
        self.state.followup(message).await
    }

    pub fn interrupt(&self) -> bool {
        self.state.interrupt()
    }

    pub async fn run(&self) -> Result<SubagentRunResult, SubagentError> {
        self.state.run().await
    }
    /// Waits for the current turn while retaining this continuable child.
    pub async fn wait_for_idle(&self) -> Result<SubagentRunResult, SubagentError> {
        self.state.wait_for_idle().await
    }

    pub async fn dispose(&self) -> Result<SubagentRunResult, SubagentError> {
        Ok(self.state.dispose().await)
    }

    pub(crate) async fn settle_replay(&self, result: SubagentRunResult) -> SubagentRunResult {
        let _operation = self.state.operation.lock().await;
        self.state.finish(result).await
    }
}

struct WorkspaceResources {
    registry: WorkspaceRegistry,
    resolver: SessionResourceResolver,
}

struct ParentWorkspace {
    registry: WorkspaceRegistry,
    lease: WorkspaceLease,
    cwd: String,
}

struct SubagentInner {
    agents: AgentRegistry,
    sessions: SessionStore,
    persistence: Arc<dyn SessionPersistence>,
    workspace: Option<WorkspaceResources>,
    providers: Arc<Mutex<ProviderState>>,
    children: Mutex<BTreeMap<u64, Arc<ChildState>>>,
    continuable_parents: Mutex<BTreeMap<SessionId, Weak<SubagentParentState>>>,
    rpc_owners: Mutex<BTreeMap<SessionId, Arc<AgentHandle>>>,
    rpc_operations: Mutex<BTreeMap<SessionId, Arc<AsyncMutex<()>>>>,
    next_acceptance: AtomicU64,
}

/// Named provider registry and parent-authorized child lifecycle owner.
#[derive(Clone)]
pub struct SubagentService {
    inner: Arc<SubagentInner>,
}

impl fmt::Debug for SubagentService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubagentService")
            .field("live_children", &lock(&self.inner.children).len())
            .finish_non_exhaustive()
    }
}

impl SubagentService {
    pub fn new(
        agents: AgentRegistry,
        sessions: SessionStore,
        persistence: Arc<dyn SessionPersistence>,
    ) -> Self {
        Self::compose(agents, sessions, persistence, None)
    }

    /// Composes subagent orchestration with durable workspace authority.
    pub fn new_with_workspace_registry(
        agents: AgentRegistry,
        sessions: SessionStore,
        persistence: Arc<dyn SessionPersistence>,
        registry: WorkspaceRegistry,
    ) -> Self {
        Self::new_with_workspace_resolver(
            agents,
            sessions,
            persistence,
            SessionResourceResolver::new(registry),
        )
    }

    /// Composes subagent orchestration with an existing session resource resolver.
    pub fn new_with_workspace_resolver(
        agents: AgentRegistry,
        sessions: SessionStore,
        persistence: Arc<dyn SessionPersistence>,
        resolver: SessionResourceResolver,
    ) -> Self {
        let registry = resolver.registry().clone();
        Self::compose(
            agents,
            sessions,
            persistence,
            Some(WorkspaceResources { registry, resolver }),
        )
    }

    fn compose(
        agents: AgentRegistry,
        sessions: SessionStore,
        persistence: Arc<dyn SessionPersistence>,
        workspace: Option<WorkspaceResources>,
    ) -> Self {
        Self {
            inner: Arc::new(SubagentInner {
                agents,
                sessions,
                persistence,
                workspace,
                providers: Arc::new(Mutex::new(ProviderState::default())),
                children: Mutex::new(BTreeMap::new()),
                continuable_parents: Mutex::new(BTreeMap::new()),
                rpc_owners: Mutex::new(BTreeMap::new()),
                rpc_operations: Mutex::new(BTreeMap::new()),
                next_acceptance: AtomicU64::new(0),
            }),
        }
    }

    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(subagents_service_key(), self)
    }

    /// Reuses the one service-owned parent capability for continuable children.
    fn continuable_parent(
        &self,
        parent: Arc<AgentHandle>,
    ) -> Result<SubagentParent, SubagentError> {
        self.inner.require_live_parent(&parent)?;
        let parent_id = parent.id();
        let mut parents = lock(&self.inner.continuable_parents);
        if let Some(state) = parents.get(&parent_id).and_then(Weak::upgrade) {
            if crate::agent::same_authority(&state.parent.authority(), &parent.authority())
                && state.is_open()
            {
                return Ok(SubagentParent { state });
            }
        }
        let capability = self.attach(parent)?;
        parents.insert(parent_id, Arc::downgrade(&capability.state));
        Ok(capability)
    }

    /// Registers exactly one provider for a stable, nonempty name.
    pub fn register(
        &self,
        name: impl Into<String>,
        provider: Arc<dyn SubagentProvider>,
    ) -> Result<SubagentProviderRegistration, SubagentError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(SubagentError::InvalidProviderName);
        }
        let mut providers = lock(&self.inner.providers);
        if providers.providers.contains_key(&name) {
            return Err(SubagentError::DuplicateProvider(name));
        }
        providers.next_generation = providers.next_generation.checked_add(1).unwrap_or(1);
        let generation = providers.next_generation;
        providers.providers.insert(
            name.clone(),
            ProviderEntry {
                generation,
                provider,
            },
        );
        Ok(SubagentProviderRegistration {
            providers: Arc::downgrade(&self.inner.providers),
            name,
            generation,
            closed: AtomicBool::new(false),
        })
    }

    /// Derives a parent capability from the live agent generation that owns it.
    pub fn attach(&self, parent: Arc<AgentHandle>) -> Result<SubagentParent, SubagentError> {
        self.inner.require_live_parent(&parent)?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| SubagentError::ParentRuntimeRequired)?;
        let state = Arc::new(SubagentParentState {
            runtime: runtime.clone(),
            service: Arc::downgrade(&self.inner),
            parent: Arc::clone(&parent),
            admissions: Mutex::new(ParentAdmissions {
                closing: false,
                pending: 0,
                children: Vec::new(),
                late_children: Vec::new(),
            }),
            quiesced: Notify::new(),
            cleanup_finished: Notify::new(),
            watcher_closed: AtomicBool::new(false),
            watcher_closed_notify: Notify::new(),
            cleanup_started: AtomicBool::new(false),
            cleanup_done: AtomicBool::new(false),
            cleanup_results: Mutex::new(Vec::new()),
        });
        let cleanup = Arc::clone(&state);
        let cancellation = parent.cancellation();
        runtime.spawn(async move {
            let closed = cleanup.watcher_closed_notify.notified();
            if cleanup.watcher_closed.load(Ordering::Acquire) {
                return;
            }
            tokio::select! {
                _ = cancellation.cancelled() => { let _ = cleanup.close_and_dispose().await; }
                _ = closed => {}
            }
        });
        Ok(SubagentParent { state })
    }

    /// Lists direct durable children without restoring or activating either side.
    pub async fn list(
        &self,
        parent_session_id: SessionId,
        cancellation: CancellationToken,
    ) -> Result<Vec<SubagentListEntry>, SubagentError> {
        self.inner.list(parent_session_id, cancellation).await
    }

    /// Lists every durable descendant in stable depth-first pre-order.
    pub async fn list_descendants(
        &self,
        parent_session_id: SessionId,
        cancellation: CancellationToken,
    ) -> Result<Vec<SubagentDescendantListEntry>, SubagentError> {
        enum Work {
            Children {
                parent: SessionId,
                depth: usize,
            },
            Entry {
                entry: SubagentListEntry,
                parent: SessionId,
                depth: usize,
            },
        }

        let mut seen = BTreeSet::from([parent_session_id.clone()]);
        let mut work = vec![Work::Children {
            parent: parent_session_id,
            depth: 1,
        }];
        let mut descendants = Vec::new();
        while let Some(next) = work.pop() {
            if cancellation.is_cancelled() {
                return Err(SubagentError::Cancelled);
            }
            match next {
                Work::Children { parent, depth } => {
                    let entries = self.list(parent.clone(), cancellation.clone()).await?;
                    for entry in entries.into_iter().rev() {
                        work.push(Work::Entry {
                            entry,
                            parent: parent.clone(),
                            depth,
                        });
                    }
                }
                Work::Entry {
                    entry,
                    parent,
                    depth,
                } => {
                    let id = entry.id().clone();
                    if !seen.insert(id.clone()) {
                        continue;
                    }
                    descendants.push(SubagentDescendantListEntry {
                        entry,
                        parent_session_id: parent,
                        depth,
                    });
                    work.push(Work::Children {
                        parent: id,
                        depth: depth + 1,
                    });
                }
            }
        }
        Ok(descendants)
    }

    fn require_live_parent_session(&self, session_id: &SessionId) -> Result<(), SubagentError> {
        let parent = self
            .inner
            .agents
            .get(session_id)
            .ok_or(SubagentError::ParentRequired)?;
        self.inner.require_live_parent(&parent).map(|_| ())
    }

    /// Reads direct-child history without restoring or activating either session.
    pub async fn history(
        &self,
        request: SubagentHistoryRequest,
        cancellation: CancellationToken,
    ) -> Result<SubagentHistoryResult, SubagentError> {
        self.inner.history(request, cancellation).await
    }

    /// Delivers one continuable-child follow-up and waits until its user message
    /// is committed to the child log.
    pub async fn prompt(
        &self,
        request: SubagentPromptRequest,
        cancellation: CancellationToken,
    ) -> Result<SubagentPromptResult, SubagentError> {
        self.inner.prompt(request, cancellation).await
    }

    /// Requests parent-cause cancellation without waiting for child shutdown.
    pub async fn interrupt(
        &self,
        request: SubagentInterruptRequest,
        cancellation: CancellationToken,
    ) -> Result<SubagentInterruptResult, SubagentError> {
        self.inner.interrupt(request, cancellation).await
    }
    /// Permanently removes one inactive direct child without touching parents or descendants.
    pub async fn delete(
        &self,
        request: SubagentDeleteRequest,
        cancellation: CancellationToken,
    ) -> Result<SubagentDeleteResult, SubagentError> {
        self.inner.delete(request, cancellation).await
    }
}
impl SubagentInner {
    fn require_live_parent(&self, parent: &AgentHandle) -> Result<Arc<Session>, SubagentError> {
        if parent.is_disposed() {
            return Err(SubagentError::ParentRequired);
        }
        let attached = parent.session();
        self.sessions
            .get(&parent.id())
            .filter(|live| Arc::ptr_eq(live, &attached))
            .ok_or(SubagentError::ParentRequired)
    }

    fn rpc_operation(&self, child_session_id: &SessionId) -> Arc<AsyncMutex<()>> {
        lock(&self.rpc_operations)
            .entry(child_session_id.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    async fn direct_child_header(
        &self,
        parent_session_id: &SessionId,
        child_session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<SessionHeader, SubagentError> {
        if cancellation.is_cancelled() {
            return Err(SubagentError::Cancelled);
        }
        if parent_session_id.as_str().trim().is_empty()
            || child_session_id.as_str().trim().is_empty()
            || parent_session_id == child_session_id
        {
            return Err(SubagentError::DirectParentMismatch);
        }
        let header = match self.sessions.get(child_session_id) {
            Some(session) => session.header(),
            None => self
                .persistence
                .load(child_session_id, cancellation)
                .await?
                .ok_or_else(|| {
                    SubagentError::Session(SessionError::NotFound(child_session_id.clone()))
                })?,
        };
        if header.origin != Some(SessionOrigin::Subagent)
            || header.parent_session.as_ref() != Some(parent_session_id)
        {
            return Err(SubagentError::DirectParentMismatch);
        }
        Ok(header)
    }

    async fn is_descendant(
        &self,
        ancestor_session_id: &SessionId,
        child_session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<bool, SubagentError> {
        let mut seen = BTreeSet::from([child_session_id.clone()]);
        let mut current = child_session_id.clone();
        loop {
            if cancellation.is_cancelled() {
                return Err(SubagentError::Cancelled);
            }
            let header = match self.sessions.get(&current) {
                Some(session) => session.header(),
                None => match self
                    .persistence
                    .load(&current, cancellation.clone())
                    .await?
                {
                    Some(header) => header,
                    None => return Ok(false),
                },
            };
            if header.origin != Some(SessionOrigin::Subagent) {
                return Ok(false);
            }
            let Some(parent) = header.parent_session else {
                return Ok(false);
            };
            if &parent == ancestor_session_id {
                return Ok(true);
            }
            if !seen.insert(parent.clone()) {
                return Ok(false);
            }
            current = parent;
        }
    }

    async fn direct_child_descriptor(
        &self,
        parent_session_id: &SessionId,
        child_session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SubagentDescriptor>, SubagentError> {
        let events = match self.sessions.get(parent_session_id) {
            Some(session) => session.events(),
            None => {
                self.persistence
                    .read_from(parent_session_id, 0, cancellation)
                    .await?
            }
        };
        Ok(events.into_iter().rev().find_map(|event| {
            if event.event_type != "subagent/contained-start" {
                return None;
            }
            let descriptor =
                serde_json::from_value::<SubagentDescriptor>(event.data.get("child")?.clone())
                    .ok()?;
            (descriptor.parent_session_id == *parent_session_id
                && descriptor.child_session_id == *child_session_id)
                .then_some(descriptor)
        }))
    }

    fn child_status(&self, child_session_id: &SessionId) -> SubagentStatus {
        self.agents
            .get(child_session_id)
            .map_or(SubagentStatus::Ready, |agent| match agent.status() {
                crate::agent::AgentStatus::Running => SubagentStatus::Running,
                crate::agent::AgentStatus::Idle => SubagentStatus::Idle,
            })
    }

    async fn delete(
        &self,
        request: SubagentDeleteRequest,
        cancellation: CancellationToken,
    ) -> Result<SubagentDeleteResult, SubagentError> {
        if cancellation.is_cancelled() {
            return Err(SubagentError::Cancelled);
        }
        let operation = self.rpc_operation(&request.child_session_id);
        let _operation = operation.lock().await;
        self.direct_child_header(
            &request.parent_session_id,
            &request.child_session_id,
            cancellation.clone(),
        )
        .await?;
        let status = self
            .list(request.parent_session_id.clone(), cancellation.clone())
            .await?
            .into_iter()
            .find_map(|entry| match entry {
                SubagentListEntry::Child { id, status, .. } if id == request.child_session_id => {
                    Some(status)
                }
                _ => None,
            })
            .ok_or(SubagentError::DeleteUnavailable)?;
        if status != SubagentStatus::Ready {
            return Err(SubagentError::DeleteActive);
        }
        let mut headers = self
            .persistence
            .list(cancellation.clone())
            .await?
            .into_iter()
            .map(|inspection| (inspection.header.id.clone(), inspection.header))
            .collect::<BTreeMap<_, _>>();
        for session in self.sessions.list() {
            headers.insert(session.id(), session.header());
        }
        if headers.values().any(|header| {
            header.origin == Some(SessionOrigin::Subagent)
                && header.parent_session.as_ref() == Some(&request.child_session_id)
        }) {
            return Err(SubagentError::DeleteHasChildren);
        }
        self.persistence
            .delete(&request.child_session_id, cancellation)
            .await?;
        self.sessions.remove(&request.child_session_id);
        lock(&self.rpc_owners).remove(&request.child_session_id);
        lock(&self.rpc_operations).remove(&request.child_session_id);
        Ok(SubagentDeleteResult { deleted: true })
    }

    async fn resume_continuable_child(
        &self,
        descriptor: SubagentDescriptor,
        header: SessionHeader,
        cancellation: CancellationToken,
    ) -> Result<AgentHandle, SubagentError> {
        if descriptor.mode != SubagentMode::Continuable {
            return Err(SubagentError::ModeMismatch);
        }
        let provider = self.select_provider(&descriptor)?;
        let child_session_id = descriptor.child_session_id.clone();
        let owner = Arc::new(
            provider
                .start(
                    ProviderStart {
                        descriptor,
                        header,
                        resume: true,
                    },
                    cancellation,
                )
                .await?,
        );
        let Some(child) = self.agents.get(&child_session_id) else {
            let _ = owner.dispose().await;
            return Err(SubagentError::DeliveryUnavailable);
        };
        lock(&self.rpc_owners).insert(child_session_id, owner);
        Ok(child)
    }

    async fn list(
        &self,
        parent_session_id: SessionId,
        cancellation: CancellationToken,
    ) -> Result<Vec<SubagentListEntry>, SubagentError> {
        if cancellation.is_cancelled() {
            return Err(SubagentError::Cancelled);
        }
        let mut corpus = self
            .persistence
            .list(cancellation.clone())
            .await?
            .into_iter()
            .map(|inspection| (inspection.header.id.clone(), inspection.header))
            .collect::<BTreeMap<_, _>>();
        for session in self.sessions.list() {
            corpus.insert(session.id(), session.header());
        }
        let parent_events = match self.sessions.get(&parent_session_id) {
            Some(parent) => parent.events(),
            None => match self
                .persistence
                .read_from(&parent_session_id, 0, cancellation.clone())
                .await
            {
                Ok(events) => events,
                Err(SessionError::NotFound(_)) => Vec::new(),
                Err(error) => return Err(error.into()),
            },
        };
        if cancellation.is_cancelled() {
            return Err(SubagentError::Cancelled);
        }
        let descriptor_rows = parent_events
            .into_iter()
            .filter(|event| event.event_type == "subagent/contained-start")
            .filter_map(|event| {
                serde_json::from_value::<SubagentDescriptor>(event.data.get("child")?.clone()).ok()
            })
            .collect::<Vec<_>>();
        let descriptors = descriptor_rows
            .into_iter()
            .map(|descriptor| (descriptor.child_session_id.clone(), descriptor))
            .collect::<BTreeMap<_, _>>();
        let candidates = corpus
            .iter()
            .filter(|(_, header)| {
                header.origin == Some(SessionOrigin::Subagent)
                    && header.parent_session.as_ref() == Some(&parent_session_id)
            })
            .collect::<Vec<_>>();
        let entries = candidates
            .into_iter()
            .map(|(child_id, _header)| {
                let has_children = corpus.values().any(|candidate| {
                    candidate.origin == Some(SessionOrigin::Subagent)
                        && candidate.parent_session.as_ref() == Some(child_id)
                });
                let Some(descriptor) = descriptors.get(child_id) else {
                    return SubagentListEntry::Diagnostic {
                        id: child_id.clone(),
                        reason: SubagentDiagnosticReason::Corrupt,
                    };
                };
                if descriptor.parent_session_id != parent_session_id
                    || descriptor.child_session_id != *child_id
                {
                    return SubagentListEntry::Diagnostic {
                        id: child_id.clone(),
                        reason: SubagentDiagnosticReason::Corrupt,
                    };
                }
                SubagentListEntry::Child {
                    id: child_id.clone(),
                    mode: descriptor.mode,
                    activity: self
                        .agents
                        .get(child_id)
                        .filter(|agent| {
                            !agent.is_disposed()
                                && agent.status() == crate::agent::AgentStatus::Running
                        })
                        .map_or(SubagentActivity::Inactive, |_| SubagentActivity::Running),
                    status: self.child_status(child_id),
                    has_children,
                    label: Some(descriptor.agent_id.clone()),
                }
            })
            .collect::<Vec<_>>();
        let mut entries = entries;
        entries.sort_by(|left, right| {
            let rank = |entry: &SubagentListEntry| match entry {
                SubagentListEntry::Child { status, .. } => match status {
                    SubagentStatus::Running => 0,
                    SubagentStatus::Idle => 1,
                    SubagentStatus::Ready => 2,
                },
                SubagentListEntry::Diagnostic { .. } => 3,
            };
            rank(left)
                .cmp(&rank(right))
                .then_with(|| left.id().cmp(right.id()))
        });
        Ok(entries)
    }

    async fn history(
        &self,
        request: SubagentHistoryRequest,
        cancellation: CancellationToken,
    ) -> Result<SubagentHistoryResult, SubagentError> {
        self.direct_child_header(
            &request.parent_session_id,
            &request.child_session_id,
            cancellation.clone(),
        )
        .await?;
        if let Some(descriptor) = self
            .direct_child_descriptor(
                &request.parent_session_id,
                &request.child_session_id,
                cancellation.clone(),
            )
            .await?
        {
            if descriptor.mode != request.mode {
                return Err(SubagentError::ModeMismatch);
            }
        }
        if request.max_messages == Some(0) {
            return Err(SubagentError::Protocol(TessivumError::new(
                "INVALID_SUBAGENT_HISTORY",
                "maxMessages must be positive when present",
                "subagent",
                Value::Null,
            )));
        }
        let mut events = match self.sessions.get(&request.child_session_id) {
            Some(session) => session.events(),
            None => {
                self.persistence
                    .read_from(&request.child_session_id, 0, cancellation.clone())
                    .await?
            }
        };
        if cancellation.is_cancelled() {
            return Err(SubagentError::Cancelled);
        }
        let tail = request.before_seq.is_none();
        if let Some(before_seq) = request.before_seq {
            events.retain(|event| event.seq < before_seq);
        }
        let cutoff = request.max_messages.map_or(0, |limit| {
            let mut messages = 0;
            for event in events.iter().rev() {
                if !matches!(
                    event.event_type.as_str(),
                    "user/message" | "assistant/message"
                ) || event.surface_op != Some(SurfaceOp::Append)
                {
                    continue;
                }
                messages += 1;
                if messages == limit {
                    return event
                        .source_event_seqs
                        .as_deref()
                        .filter(|sources| !sources.is_empty())
                        .map_or(event.seq, |sources| {
                            sources.iter().copied().fold(event.seq, u64::min)
                        });
                }
            }
            0
        });
        let has_more = cutoff != 0;
        events.retain(|event| event.seq >= cutoff);
        let events = events
            .into_iter()
            .map(|event| SubagentHistoryEntry { event, view: None })
            .collect::<Vec<_>>();
        let projections = tail.then(|| SessionProjectionsBlock {
            as_of_seq: events.last().map_or(-1, |entry| entry.event.seq as i64),
            values: BTreeMap::new(),
        });
        Ok(SubagentHistoryResult {
            events,
            has_more,
            projections,
        })
    }

    async fn prompt(
        self: &Arc<Self>,
        request: SubagentPromptRequest,
        cancellation: CancellationToken,
    ) -> Result<SubagentPromptResult, SubagentError> {
        if request.mode != SubagentMode::Continuable {
            return Err(SubagentError::ContinuableRequired);
        }
        let child_session_id = request.child_session_id.clone();
        let operation = self.rpc_operation(&child_session_id);
        let operation_guard = operation.lock().await;
        let header = self
            .direct_child_header(
                &request.parent_session_id,
                &request.child_session_id,
                cancellation.clone(),
            )
            .await?;
        let descriptor = self
            .direct_child_descriptor(
                &request.parent_session_id,
                &request.child_session_id,
                cancellation.clone(),
            )
            .await?;
        if descriptor
            .as_ref()
            .is_some_and(|descriptor| descriptor.mode != request.mode)
        {
            return Err(SubagentError::ModeMismatch);
        }
        let parent = self
            .agents
            .get(&request.parent_session_id)
            .ok_or(SubagentError::ParentRequired)?;
        self.require_live_parent(&parent)?;
        let child = match self.agents.get(&request.child_session_id) {
            Some(child) if !child.is_disposed() && child.cancel_options().is_none() => child,
            Some(child) => {
                tokio::select! {
                    result = child.when_idle() => result?,
                    _ = cancellation.cancelled() => return Err(SubagentError::Cancelled),
                }
                child.dispose().await?;
                lock(&self.rpc_owners).remove(&request.child_session_id);
                self.resume_continuable_child(
                    descriptor.ok_or(SubagentError::AlreadyRun)?,
                    header,
                    cancellation.clone(),
                )
                .await?
            }
            None => {
                self.resume_continuable_child(
                    descriptor.ok_or(SubagentError::AlreadyRun)?,
                    header,
                    cancellation.clone(),
                )
                .await?
            }
        };
        let message_id = MessageId::random();
        let message = Message {
            id: message_id.clone(),
            role: MessageRole::User,
            content: request.content,
            source: MessageSource::User {
                client_time_zone: request.client_time_zone,
            },
        };
        message.validate()?;
        let session = child.session();
        session
            .append_next(
                |seq| inbox_enqueued_event(seq, InboxTarget::Followup, &message),
                cancellation.clone(),
            )
            .await?;
        if let Err(error) = child.followup(message).await {
            return Err(match error {
                AgentError::Cancelled | AgentError::Disposed => SubagentError::DeliveryUnavailable,
                error => SubagentError::Agent(error),
            });
        }
        drop(operation_guard);
        let inner = Arc::clone(self);
        let cleanup_child = self.agents.get(&child_session_id);
        tokio::spawn(async move {
            if let Some(cleanup_child) = cleanup_child {
                let cleanup_authority = cleanup_child.authority();
                let _ = cleanup_child.when_idle().await;
                let _operation = operation.lock().await;
                let Some(current) = inner.agents.get(&child_session_id) else {
                    lock(&inner.rpc_owners).remove(&child_session_id);
                    return;
                };
                if !crate::agent::same_authority(&cleanup_authority, &current.authority())
                    || current.status() != crate::agent::AgentStatus::Idle
                {
                    return;
                }
                lock(&inner.rpc_owners).remove(&child_session_id);
                let _ = current.dispose().await;
            } else {
                lock(&inner.rpc_owners).remove(&child_session_id);
            }
        });
        Ok(SubagentPromptResult { message_id })
    }

    async fn interrupt(
        &self,
        request: SubagentInterruptRequest,
        cancellation: CancellationToken,
    ) -> Result<SubagentInterruptResult, SubagentError> {
        if request.mode != SubagentMode::Continuable {
            return Err(SubagentError::ContinuableRequired);
        }
        if cancellation.is_cancelled() {
            return Err(SubagentError::Cancelled);
        }
        if request.parent_session_id.as_str().trim().is_empty()
            || request.child_session_id.as_str().trim().is_empty()
            || request.parent_session_id == request.child_session_id
        {
            return Err(SubagentError::DirectParentMismatch);
        }
        let known = self.agents.get(&request.child_session_id).is_some()
            || self.sessions.get(&request.child_session_id).is_some()
            || self
                .persistence
                .load(&request.child_session_id, cancellation.clone())
                .await?
                .is_some();
        if !known {
            return Ok(SubagentInterruptResult { accepted: true });
        }
        if !self
            .is_descendant(
                &request.parent_session_id,
                &request.child_session_id,
                cancellation,
            )
            .await?
        {
            return Err(SubagentError::DirectParentMismatch);
        }
        let parent = self
            .agents
            .get(&request.parent_session_id)
            .ok_or(SubagentError::ParentRequired)?;
        self.require_live_parent(&parent)?;
        let Some(child) = self.agents.get(&request.child_session_id) else {
            return Ok(SubagentInterruptResult { accepted: true });
        };
        if lock(&self.children)
            .values()
            .find(|state| state.acceptance.descriptor.child_session_id == request.child_session_id)
            .is_some_and(|state| state.acceptance.descriptor.mode != SubagentMode::Continuable)
        {
            return Ok(SubagentInterruptResult { accepted: true });
        }
        child.cancel_including_idle(AgentCancelCause::Parent, true);
        Ok(SubagentInterruptResult { accepted: true })
    }

    /// Starts one private child. It becomes observable only after the durable
    /// contained-start commit; every earlier failure disposes it without an end.
    async fn start(
        self: &Arc<Self>,
        parent_agent: &AgentHandle,
        request: SubagentStartRequest,
        cancellation: CancellationToken,
        provided_seed: Option<Vec<SessionEvent>>,
        seed_parent_prefix: bool,
        allow_continuable_seed: bool,
    ) -> Result<(SubagentAcceptance, SubagentActivation), SubagentError> {
        let parent = self.require_live_parent(parent_agent)?;
        if request.cwd.is_some() {
            return Err(SubagentError::CwdOverrideUnsupported);
        }
        let descriptor = descriptor(&parent, &request)?;
        let workspace = self.parent_workspace(&parent)?;
        let cwd = workspace
            .as_ref()
            .map(|workspace| workspace.cwd.clone())
            .or_else(|| parent.header().cwd);
        if cancellation.is_cancelled() {
            return Err(SubagentError::CancelledBeforeAcceptance);
        }
        if (provided_seed.is_some() || seed_parent_prefix)
            && (request.resume
                || (request.mode != SubagentMode::OneShot && !allow_continuable_seed))
        {
            return Err(SubagentError::ModeMismatch);
        }
        let provider = self.select_provider(&descriptor)?;
        if request.resume {
            self.require_direct_parent(&descriptor, &cwd, workspace.as_ref())
                .await?;
        }
        if let Some(workspace) = &workspace {
            workspace.lease.validate_current()?;
        }
        if cancellation.is_cancelled() {
            return Err(SubagentError::CancelledBeforeAcceptance);
        }
        let seed_events = provided_seed
            .or_else(|| seed_parent_prefix.then(|| completed_parent_seed(&parent.events())));
        let seeded = seed_events.is_some();
        let parent_header = parent.header();
        let header = child_header(
            &descriptor,
            &request,
            cwd,
            seed_events.as_ref().map(|events| events.len() as u64),
            request
                .agent_mode
                .clone()
                .or_else(|| parent_header.agent_mode.clone()),
            Some(
                parent_header
                    .delegation_depth
                    .unwrap_or(0)
                    .saturating_add(1),
            ),
        )?;
        if let Some(seed_events) = seed_events {
            self.sessions
                .create_seeded(header.clone(), seed_events, cancellation.clone())
                .await?;
        }
        let agent = provider
            .start(
                ProviderStart {
                    descriptor: descriptor.clone(),
                    header,
                    resume: request.resume || seeded,
                },
                cancellation.clone(),
            )
            .await?;
        if let Err(error) =
            self.attach_child_workspace(workspace.as_ref(), &descriptor.child_session_id)
        {
            let _ = agent.dispose().await;
            return Err(error.into());
        }
        if cancellation.is_cancelled() {
            let _ = agent.dispose().await;
            return Err(SubagentError::CancelledBeforeAcceptance);
        }
        // A requested initial turn is part of admission. A delivery failure has
        // no durable start, so it must not manufacture a contained-end either.
        if let Some(message) = request.initial_message {
            if let Err(error) = agent.followup(message).await {
                let _ = agent.dispose().await;
                return Err(SubagentError::Agent(error));
            }
        }
        if cancellation.is_cancelled() {
            let _ = agent.dispose().await;
            return Err(SubagentError::CancelledBeforeAcceptance);
        }
        if self.require_live_parent(parent_agent).is_err() {
            let _ = agent.dispose().await;
            return Err(SubagentError::ParentRequired);
        }

        let acceptance = SubagentAcceptance {
            acceptance_id: self
                .next_acceptance
                .fetch_add(1, Ordering::Relaxed)
                .checked_add(1)
                .unwrap_or(1),
            descriptor,
        };
        if let Err(error) = append_event(
            &parent,
            "subagent/contained-start",
            json!({"acceptanceId": acceptance.acceptance_id, "child": acceptance.descriptor}),
        )
        .await
        {
            let _ = agent.dispose().await;
            return Err(error);
        }
        let state = Arc::new(ChildState {
            acceptance: acceptance.clone(),
            parent,
            cancellation: agent.cancellation(),
            agent: AsyncMutex::new(Some(Arc::new(agent))),
            operation: AsyncMutex::new(()),
            terminal: Mutex::new(None),
            service: Arc::downgrade(self),
        });
        lock(&self.children).insert(acceptance.acceptance_id, Arc::clone(&state));
        Ok((acceptance, SubagentActivation { state }))
    }

    async fn children(
        &self,
        parent: &AgentHandle,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SubagentError> {
        let parent = self.require_live_parent(parent)?;
        let mut children = self
            .persistence
            .list(cancellation)
            .await?
            .into_iter()
            .filter(|entry| {
                entry.header.origin == Some(SessionOrigin::Subagent)
                    && entry.header.parent_session.as_ref() == Some(&parent.id())
            })
            .collect::<Vec<_>>();
        children.sort_by(|left, right| left.header.id.cmp(&right.header.id));
        Ok(children)
    }

    async fn descendants(
        &self,
        parent: &AgentHandle,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SubagentError> {
        let parent = self.require_live_parent(parent)?;
        let all = self.persistence.list(cancellation).await?;
        let mut parents = BTreeSet::from([parent.id()]);
        let mut descendants = Vec::new();
        loop {
            let next = all
                .iter()
                .filter(|entry| {
                    entry.header.origin == Some(SessionOrigin::Subagent)
                        && entry
                            .header
                            .parent_session
                            .as_ref()
                            .is_some_and(|parent| parents.contains(parent))
                        && !parents.contains(&entry.header.id)
                })
                .cloned()
                .collect::<Vec<_>>();
            if next.is_empty() {
                break;
            }
            for entry in next {
                parents.insert(entry.header.id.clone());
                descendants.push(entry);
            }
        }
        descendants.sort_by(|left, right| left.header.id.cmp(&right.header.id));
        Ok(descendants)
    }

    fn select_provider(
        &self,
        descriptor: &SubagentDescriptor,
    ) -> Result<Arc<dyn SubagentProvider>, SubagentError> {
        let provider = lock(&self.providers)
            .providers
            .get(&descriptor.provider)
            .map(|entry| Arc::clone(&entry.provider))
            .ok_or_else(|| SubagentError::ProviderNotFound(descriptor.provider.clone()))?;
        let allowed = provider.capabilities();
        if let Some(capability) = descriptor
            .capabilities
            .iter()
            .find(|capability| !allowed.contains(*capability))
        {
            return Err(SubagentError::CapabilityDenied {
                provider: descriptor.provider.clone(),
                capability: capability.clone(),
            });
        }
        Ok(provider)
    }

    fn parent_workspace(&self, parent: &Session) -> Result<Option<ParentWorkspace>, SubagentError> {
        let Some(resources) = &self.workspace else {
            return Ok(None);
        };
        let lease = resources.resolver.resolve(parent.id())?;
        let cwd = lease.validate_current()?.to_string_lossy().into_owned();
        Ok(Some(ParentWorkspace {
            registry: resources.registry.clone(),
            lease,
            cwd,
        }))
    }

    fn attach_child_workspace(
        &self,
        workspace: Option<&ParentWorkspace>,
        child_session_id: &SessionId,
    ) -> Result<(), WorkspaceError> {
        let Some(workspace) = workspace else {
            return Ok(());
        };
        workspace.registry.recognize_session(child_session_id)?;
        workspace.lease.validate_current()?;
        workspace
            .registry
            .attach_session(workspace.lease.workspace_id(), child_session_id, None)
    }

    async fn require_direct_parent(
        &self,
        descriptor: &SubagentDescriptor,
        expected_cwd: &Option<String>,
        workspace: Option<&ParentWorkspace>,
    ) -> Result<(), SubagentError> {
        let header = self
            .persistence
            .load(
                &descriptor.child_session_id,
                ContextHandle::root().scope().cancellation(),
            )
            .await?
            .ok_or(SubagentError::ResumeParentMismatch)?;
        if header.parent_session.as_ref() != Some(&descriptor.parent_session_id)
            || header.origin != Some(SessionOrigin::Subagent)
        {
            return Err(SubagentError::ResumeParentMismatch);
        }
        if !same_canonical_cwd(header.cwd.as_deref(), expected_cwd.as_deref())
            || workspace.is_some_and(|workspace| {
                workspace
                    .registry
                    .workspace_for_session(&descriptor.child_session_id)
                    .is_none_or(|child| child.workspace_id != *workspace.lease.workspace_id())
            })
        {
            return Err(SubagentError::ResumeWorkspaceMismatch);
        }
        Ok(())
    }
}

fn same_canonical_cwd(stored: Option<&str>, expected: Option<&str>) -> bool {
    match (stored, expected) {
        (None, None) => true,
        (Some(stored), Some(expected)) => {
            fs::canonicalize(Path::new(stored)).is_ok_and(|stored| stored == Path::new(expected))
        }
        _ => false,
    }
}

fn descriptor(
    parent: &Session,
    request: &SubagentStartRequest,
) -> Result<SubagentDescriptor, SubagentError> {
    if request.provider.trim().is_empty() {
        return Err(SubagentError::InvalidProviderName);
    }
    if request.agent_id.trim().is_empty() {
        return Err(SubagentError::InvalidAgentId);
    }
    if request.child_session_id.as_str().trim().is_empty() {
        return Err(SubagentError::InvalidChildSessionId);
    }
    if request.child_session_id == parent.id() {
        return Err(SubagentError::SelfParent);
    }
    if request.options.provider.trim().is_empty() {
        return Err(SubagentError::InvalidOptions("provider must not be empty"));
    }
    if request.options.model.trim().is_empty() {
        return Err(SubagentError::InvalidOptions("model must not be empty"));
    }
    if request.options.max_tokens == Some(0) {
        return Err(SubagentError::InvalidOptions(
            "max_tokens must be positive when present",
        ));
    }
    let mut capabilities = BTreeSet::new();
    for capability in &request.capabilities {
        if capability.trim().is_empty() {
            return Err(SubagentError::InvalidCapability);
        }
        capabilities.insert(capability.clone());
    }
    if let Some(message) = &request.initial_message {
        message.validate()?;
    }
    Ok(SubagentDescriptor {
        provider: request.provider.clone(),
        agent_id: request.agent_id.clone(),
        parent_session_id: parent.id(),
        child_session_id: request.child_session_id.clone(),
        mode: request.mode,
        capabilities,
        options: request.options.clone(),
    })
}
fn child_header(
    descriptor: &SubagentDescriptor,
    request: &SubagentStartRequest,
    cwd: Option<String>,
    seed_length: Option<u64>,
    agent_mode: Option<AgentModeId>,
    delegation_depth: Option<u64>,
) -> Result<SessionHeader, SubagentError> {
    let header = SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: descriptor.child_session_id.clone(),
        created_at: request.created_at,
        cwd,
        parent_session: Some(descriptor.parent_session_id.clone()),
        seed_length,
        origin: Some(SessionOrigin::Subagent),
        delegation_depth,
        agent_mode,
    };
    header.validate()?;
    Ok(header)
}

fn completed_parent_seed(events: &[SessionEvent]) -> Vec<SessionEvent> {
    let Some(boundary) = events
        .iter()
        .rposition(|event| event.event_type == "turn/end")
    else {
        return Vec::new();
    };
    let mut end = boundary + 1;
    while end < events.len() && events[end].event_type != "turn/start" {
        end += 1;
    }
    let events = &events[..end];
    let mut sequences = BTreeMap::new();
    for event in events {
        if event.event_type != "session/end-seed" {
            sequences.insert(event.seq, sequences.len() as u64);
        }
    }
    events
        .iter()
        .filter_map(|event| {
            let sequence = sequences.get(&event.seq)?;
            let mut event = event.clone();
            event.seq = *sequence;
            if let Some(sources) = &event.source_event_seqs {
                let mapped = sources
                    .iter()
                    .filter_map(|source| sequences.get(source).copied())
                    .collect::<Vec<_>>();
                event.source_event_seqs =
                    (!mapped.is_empty() || sources.is_empty()).then_some(mapped);
            }
            Some(event)
        })
        .collect()
}

async fn append_event(
    parent: &Session,
    event_type: &str,
    data: Value,
) -> Result<(), SubagentError> {
    let event = SessionEvent {
        event_type: event_type.into(),
        seq: parent.next_seq()?,
        time: 0,
        data,
        ignorable: Some(true),
        source_event_seqs: None,
        surface_op: None,
    };
    parent
        .append(event, ContextHandle::root().scope().cancellation())
        .await?;
    Ok(())
}

fn completed_result(session: &Session) -> SubagentRunResult {
    SubagentRunResult {
        status: SubagentRunStatus::Completed,
        error: None,
        last_assistant_message: last_assistant_message(session),
    }
}

fn cancelled_result() -> SubagentRunResult {
    SubagentRunResult {
        status: SubagentRunStatus::Cancelled,
        error: None,
        last_assistant_message: None,
    }
}

fn error_result(code: impl Into<String>, message: impl Into<String>) -> SubagentRunResult {
    SubagentRunResult {
        status: SubagentRunStatus::Error,
        error: Some(SubagentFailure {
            code: code.into(),
            message: message.into(),
        }),
        last_assistant_message: None,
    }
}

fn last_assistant_message(session: &Session) -> Option<Vec<ContentBlock>> {
    session
        .surface()
        .into_iter()
        .rev()
        .find(|entry| entry.message.role == MessageRole::Assistant)
        .map(|entry| entry.message.content)
}

fn lock<T>(mutex: &Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    mutex.lock()
}

#[cfg(test)]
mod delegation_tests {
    use super::*;

    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use async_trait::async_trait;

    use crate::{
        agent::{AgentFactory, AgentRuntime, AgentStatus, Inbox},
        jobs::{JobStart, JobStatus, LocalJobRegistry},
        session::MemorySessionPersistence,
    };

    fn cancellation() -> CancellationToken {
        ContextHandle::root().scope().cancellation()
    }

    fn header(id: &str) -> SessionHeader {
        SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: SessionId::from(id),
            created_at: 0,
            cwd: None,
            parent_session: None,
            seed_length: None,
            origin: None,
            delegation_depth: None,
            agent_mode: None,
        }
    }

    fn options() -> AgentOptions {
        AgentOptions {
            provider: "test".into(),
            model: "test".into(),
            reasoning_effort: None,
            max_tokens: None,
        }
    }

    fn request(id: &str, mode: SubagentMode) -> SubagentStartRequest {
        SubagentStartRequest {
            provider: "native".into(),
            agent_id: "test-child".into(),
            child_session_id: SessionId::from(id),
            agent_mode: None,
            mode,
            capabilities: Vec::new(),
            options: options(),
            created_at: 0,
            cwd: None,
            resume: false,
            initial_message: None,
        }
    }

    struct Idle;

    #[async_trait]
    impl AgentRuntime for Idle {
        fn status(&self) -> AgentStatus {
            AgentStatus::Idle
        }

        async fn wake(&self) -> Result<(), AgentError> {
            Ok(())
        }

        async fn when_idle(&self) -> Result<(), AgentError> {
            Ok(())
        }

        async fn dispose(&self) -> Result<(), AgentError> {
            Ok(())
        }
    }

    struct IdleFactory;

    #[async_trait]
    impl AgentFactory for IdleFactory {
        async fn create(
            &self,
            _: Arc<Session>,
            _: AgentOptions,
            _: Inbox,
            _: CancellationToken,
        ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
            Ok(Arc::new(Idle))
        }
    }

    struct Blocking {
        started: Arc<Notify>,
        disposals: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentRuntime for Blocking {
        fn status(&self) -> AgentStatus {
            AgentStatus::Running
        }

        async fn wake(&self) -> Result<(), AgentError> {
            Ok(())
        }

        async fn when_idle(&self) -> Result<(), AgentError> {
            self.started.notify_one();
            std::future::pending::<Result<(), AgentError>>().await
        }

        async fn dispose(&self) -> Result<(), AgentError> {
            self.disposals.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    struct BlockingFactory {
        started: Arc<Notify>,
        disposals: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentFactory for BlockingFactory {
        async fn create(
            &self,
            _: Arc<Session>,
            _: AgentOptions,
            _: Inbox,
            _: CancellationToken,
        ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
            Ok(Arc::new(Blocking {
                started: Arc::clone(&self.started),
                disposals: Arc::clone(&self.disposals),
            }))
        }
    }

    async fn setup(
        factory: Arc<dyn AgentFactory>,
    ) -> (SubagentService, Arc<AgentHandle>, AgentRegistry) {
        let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
        let sessions = SessionStore::new(Arc::clone(&persistence));
        let agents = AgentRegistry::new(sessions.clone());
        std::mem::forget(agents.register_factory(factory).unwrap());
        let service = SubagentService::new(agents.clone(), sessions, persistence);
        std::mem::forget(
            service
                .register(
                    "native",
                    Arc::new(NativeSubagentProvider::new(
                        agents.clone(),
                        std::iter::empty::<String>(),
                    )),
                )
                .unwrap(),
        );
        let parent = Arc::new(
            agents
                .create(header("parent"), options(), cancellation())
                .await
                .unwrap(),
        );
        (service, parent, agents)
    }

    #[tokio::test]
    async fn continuable_child_survives_a_foreground_wait_and_parent_cleanup() {
        let (service, parent_agent, agents) = setup(Arc::new(IdleFactory)).await;
        let parent = service.continuable_parent(parent_agent).unwrap();
        let child_id = SessionId::from("continuable-child");
        let (_, child) = parent
            .start(
                request(child_id.as_str(), SubagentMode::Continuable),
                cancellation(),
            )
            .await
            .unwrap();

        assert_eq!(
            child.wait_for_idle().await.unwrap().status,
            SubagentRunStatus::Completed
        );
        assert!(agents.get(&child_id).is_some());
        assert!(matches!(
            service.list(SessionId::from("parent"), cancellation()).await.unwrap().as_slice(),
            [SubagentListEntry::Child { id, mode: SubagentMode::Continuable, .. }] if id == &child_id
        ));

        parent.dispose().await;
        assert!(parent.state.watcher_closed.load(Ordering::Acquire));
        assert!(agents.get(&child_id).is_none());
    }

    #[tokio::test]
    async fn forked_child_receives_only_the_completed_parent_prefix() {
        let (service, parent_agent, agents) = setup(Arc::new(IdleFactory)).await;
        parent_agent
            .session()
            .append(
                SessionEvent {
                    event_type: "turn/end".into(),
                    seq: 0,
                    time: 0,
                    data: json!({"turn": 0}),
                    ignorable: None,
                    source_event_seqs: None,
                    surface_op: None,
                },
                cancellation(),
            )
            .await
            .unwrap();
        let parent = service.attach(parent_agent).unwrap();
        let child_id = SessionId::from("fork-child");
        parent
            .start_forked(
                request(child_id.as_str(), SubagentMode::OneShot),
                cancellation(),
            )
            .await
            .unwrap();
        let child = agents.get(&child_id).unwrap();

        assert_eq!(child.session().header().seed_length, Some(1));
        assert_eq!(child.session().seed_events()[0].event_type, "turn/end");
        parent.dispose().await;
    }

    #[test]
    fn ralph_round_cap_is_positive_and_bounded() {
        assert_eq!(ralph_max_rounds(&json!({})).unwrap(), MAX_RALPH_ROUNDS);
        assert_eq!(ralph_max_rounds(&json!({"maxRounds": 2})).unwrap(), 2);
        assert!(ralph_max_rounds(&json!({"maxRounds": 0})).is_err());
        assert!(ralph_max_rounds(&json!({"maxRounds": MAX_RALPH_ROUNDS + 1})).is_err());
    }

    #[tokio::test]
    async fn killed_job_disposes_an_accepted_child_without_waiting_for_idle() {
        let started = Arc::new(Notify::new());
        let disposals = Arc::new(AtomicUsize::new(0));
        let (service, parent_agent, agents) = setup(Arc::new(BlockingFactory {
            started: Arc::clone(&started),
            disposals: Arc::clone(&disposals),
        }))
        .await;
        let controller = ContextHandle::root();
        let registry = LocalJobRegistry::new();
        let owner = registry
            .attach_owner(&parent_agent.authority(), &controller)
            .unwrap();
        let parent = service.attach(parent_agent).unwrap();
        let child_id = SessionId::from("cancelled-child");
        let child_request = request(child_id.as_str(), SubagentMode::OneShot);
        let job_parent = parent.clone();
        let job = owner
            .start(JobStart::new(
                "subagent",
                "cancelled child",
                64,
                move |control| {
                    let parent = job_parent.clone();
                    let request = child_request.clone();
                    async move {
                        run_one_shot_delegation(parent, request, control.cancellation(), false)
                            .await?;
                        Ok(Value::Null)
                    }
                },
            ))
            .unwrap();

        started.notified().await;
        owner.kill(&job.id).unwrap();
        assert_eq!(
            owner
                .wait(&job.id, Some(Duration::from_secs(1)), None)
                .await
                .unwrap()
                .status,
            JobStatus::Killed
        );
        assert_eq!(disposals.load(Ordering::Acquire), 1);
        assert!(agents.get(&child_id).is_none());
        parent.dispose().await;
        assert!(parent.state.watcher_closed.load(Ordering::Acquire));
    }
}

//! Durable, parent-owned child agent orchestration.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Weak,
    },
};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::{
    agent::{AgentError, AgentHandle, AgentOptions, AgentRegistry},
    protocol::{
        AgentCancelCause, ContentBlock, Message, MessageRole, SessionEvent, SessionHeader,
        SessionId, SessionOrigin, SESSION_FORMAT_VERSION,
    },
    session::{Session, SessionError, SessionInspection, SessionPersistence, SessionStore},
    TessivumError,
};

/// Stable key for the parent-owned subagent capability.
pub fn subagents_service_key() -> ServiceKey {
    ServiceKey::new("harness.subagents", "1")
}

/// A durable continuation descriptor recorded before a child can be reported as started.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDescriptor {
    pub provider: String,
    pub agent_id: String,
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    pub capabilities: BTreeSet<String>,
    pub options: AgentOptions,
}

/// Caller input for one child activation. All fields are untrusted.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentStartRequest {
    pub provider: String,
    pub agent_id: String,
    pub child_session_id: SessionId,
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
    #[error("a child activation can run only once")]
    AlreadyRun,
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Protocol(#[from] TessivumError),
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
    cleanup_started: AtomicBool,
    cleanup_done: AtomicBool,
    cleanup_results: Mutex<Vec<SubagentRunResult>>,
}

struct SubagentAdmissionPermit {
    state: Arc<SubagentParentState>,
    released: bool,
}

impl SubagentAdmissionPermit {
    fn admit_or_queue(&self, activation: SubagentActivation) -> bool {
        let mut admissions = self.state.admissions.lock();
        if admissions.closing
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
        let mut permit = match self.state.reserve_admission() {
            Some(permit) => permit,
            None => {
                self.state.begin_cleanup();
                return Err(SubagentError::ParentRequired);
            }
        };
        let service = self.service()?;
        let (acceptance, activation) = service
            .start(&self.state.parent, request, cancellation)
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
}

impl SubagentParentState {
    fn reserve_admission(self: &Arc<Self>) -> Option<SubagentAdmissionPermit> {
        let mut admissions = self.admissions.lock();
        if admissions.closing
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
        tokio::spawn(async move {
            let results = cleanup_parent_children(Arc::clone(&state)).await;
            *state.cleanup_results.lock() = results;
            state.cleanup_done.store(true, Ordering::Release);
            state.cleanup_finished.notify_waiters();
        });
    }

    async fn close_and_dispose(self: &Arc<Self>) -> Vec<SubagentRunResult> {
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
            agent.cancel(cause, false)
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

    pub async fn dispose(&self) -> Result<SubagentRunResult, SubagentError> {
        Ok(self.state.dispose().await)
    }
}

struct SubagentInner {
    sessions: SessionStore,
    persistence: Arc<dyn SessionPersistence>,
    providers: Arc<Mutex<ProviderState>>,
    children: Mutex<BTreeMap<u64, Arc<ChildState>>>,
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
        _agents: AgentRegistry,
        sessions: SessionStore,
        persistence: Arc<dyn SessionPersistence>,
    ) -> Self {
        Self {
            inner: Arc::new(SubagentInner {
                sessions,
                persistence,
                providers: Arc::new(Mutex::new(ProviderState::default())),
                children: Mutex::new(BTreeMap::new()),
                next_acceptance: AtomicU64::new(0),
            }),
        }
    }

    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(subagents_service_key(), self)
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
        let state = Arc::new(SubagentParentState {
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
            cleanup_started: AtomicBool::new(false),
            cleanup_done: AtomicBool::new(false),
            cleanup_results: Mutex::new(Vec::new()),
        });
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| SubagentError::ParentRuntimeRequired)?;
        let cleanup = Arc::clone(&state);
        let cancellation = parent.cancellation();
        runtime.spawn(async move {
            cancellation.cancelled().await;
            let _ = cleanup.close_and_dispose().await;
        });
        Ok(SubagentParent { state })
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

    /// Starts one private child. It becomes observable only after the durable
    /// contained-start commit; every earlier failure disposes it without an end.
    async fn start(
        self: &Arc<Self>,
        parent_agent: &AgentHandle,
        request: SubagentStartRequest,
        cancellation: CancellationToken,
    ) -> Result<(SubagentAcceptance, SubagentActivation), SubagentError> {
        let parent = self.require_live_parent(parent_agent)?;
        let descriptor = descriptor(&parent, &request)?;
        if cancellation.is_cancelled() {
            return Err(SubagentError::CancelledBeforeAcceptance);
        }
        let provider = self.select_provider(&descriptor)?;
        if request.resume {
            self.require_direct_parent(&descriptor).await?;
        }
        if cancellation.is_cancelled() {
            return Err(SubagentError::CancelledBeforeAcceptance);
        }
        let header = child_header(&descriptor, &request)?;
        let agent = provider
            .start(
                ProviderStart {
                    descriptor: descriptor.clone(),
                    header,
                    resume: request.resume,
                },
                cancellation.clone(),
            )
            .await?;
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

    async fn require_direct_parent(
        &self,
        descriptor: &SubagentDescriptor,
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
        Ok(())
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
        capabilities,
        options: request.options.clone(),
    })
}

fn child_header(
    descriptor: &SubagentDescriptor,
    request: &SubagentStartRequest,
) -> Result<SessionHeader, SubagentError> {
    let header = SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: descriptor.child_session_id.clone(),
        created_at: request.created_at,
        cwd: request.cwd.clone(),
        parent_session: Some(descriptor.parent_session_id.clone()),
        seed_length: None,
        origin: Some(SessionOrigin::Subagent),
        delegation_depth: None,
        agent_preset: Some(descriptor.agent_id.clone()),
    };
    header.validate()?;
    Ok(header)
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

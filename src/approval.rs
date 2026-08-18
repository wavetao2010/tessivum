//! Fail-closed, durable approval decisions and deny-only scoped hooks.

use std::{
    collections::{BTreeMap, BTreeSet},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
};

use async_trait::async_trait;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    agent::{same_authority, AgentAuthority, AgentHandle},
    session::{Session, SessionError},
    tools::{ToolApproval, ToolApprovalResult, ToolRunContext},
    SessionEvent, SessionId, TessivumError, ToolCallId, ToolSchema,
};

/// Stable key for the agent-owned approval service.
pub fn approval_service_key() -> ServiceKey {
    ServiceKey::new("harness.approval", "1")
}

/// The only approval policies. `Ask` does not permit an absent response.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalPolicy {
    Allow,
    #[default]
    Ask,
    Never,
}

/// A one-shot decision result. Every variant other than `AllowedOnce` is denying.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

impl ApprovalOutcome {
    pub fn allows(self) -> bool {
        self == Self::AllowedOnce
    }
}

/// Opaque durable identity joining one asked event to its final decision.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ApprovalId(String);

impl ApprovalId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Untrusted action and lossless arguments requesting a one-shot decision.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub action: String,
    pub details: Value,
}

impl ApprovalRequest {
    pub fn validate(&self) -> Result<(), ApprovalError> {
        if self.action.trim().is_empty() || self.action.len() > 256 {
            return Err(ApprovalError::InvalidRequest);
        }
        Ok(())
    }
}

/// Durable policy payload. `next_step` records a single-use override.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalPolicyChange {
    pub policy: ApprovalPolicy,
    #[serde(default)]
    pub next_step: bool,
}

/// Durable audit event emitted before answerers are called.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalAsked {
    #[serde(default)]
    pub approval_id: ApprovalId,
    pub turn: u64,
    pub policy: ApprovalPolicy,
    pub request: ApprovalRequest,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<ToolCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Durable audit event emitted after the final decision is known.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecision {
    #[serde(default)]
    pub approval_id: ApprovalId,
    pub turn: u64,
    pub outcome: ApprovalOutcome,
}

/// An answerer may decline with `None`; the next registered answerer is then tried.
#[async_trait]
pub trait ApprovalAnswerer: Send + Sync {
    async fn answer(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<Option<bool>, TessivumError>;
}

#[async_trait]
impl<T> ApprovalAnswerer for Arc<T>
where
    T: ApprovalAnswerer + ?Sized,
{
    async fn answer(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        (**self).answer(request, cancellation).await
    }
}

/// A hook may only veto by returning a reason. It has no allow result.
pub trait ApprovalHook: Send + Sync {
    fn deny(&self, request: &ApprovalRequest) -> Option<String>;
}

impl<F> ApprovalHook for F
where
    F: Fn(&ApprovalRequest) -> Option<String> + Send + Sync,
{
    fn deny(&self, request: &ApprovalRequest) -> Option<String> {
        self(request)
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ApprovalError {
    #[error("approval service is not owned by its live agent")]
    NotLive,
    #[error("approval requires an active turn")]
    NoActiveTurn,
    #[error("approval request is invalid")]
    InvalidRequest,
    #[error("approval operation was cancelled")]
    Cancelled,
    #[error("durable approval payload is invalid")]
    InvalidReplay,
    #[error(transparent)]
    Session(#[from] SessionError),
}

#[derive(Default)]
struct ApprovalState {
    policy: ApprovalPolicy,
    next_step: Option<ApprovalPolicy>,
    next_id: u64,
    pending_decisions: BTreeSet<u64>,
    answerers: BTreeMap<u64, Arc<dyn ApprovalAnswerer>>,
    hooks: BTreeMap<u64, Arc<dyn ApprovalHook>>,
}

struct ApprovalInner {
    _agent: AgentHandle,
    authority: AgentAuthority,
    session: Arc<Session>,
    state: Mutex<ApprovalState>,
    write_gate: AsyncMutex<()>,
}

/// A fail-closed approval coordinator tied to exactly one agent's session.
#[derive(Clone)]
pub struct ApprovalService {
    inner: Arc<ApprovalInner>,
}

impl ApprovalService {
    /// Restores the persistent policy and, when present, the single next-step override.
    pub fn new(agent: AgentHandle) -> Result<Self, ApprovalError> {
        let authority = agent.authority();
        let session = agent.session();
        if !authority.is_live() || authority.id() != session.id() {
            return Err(ApprovalError::NotLive);
        }
        let mut state = ApprovalState::default();
        for event in session.events() {
            match event.event_type.as_str() {
                "approval/policy" => {
                    let change: ApprovalPolicyChange = serde_json::from_value(event.data)
                        .map_err(|_| ApprovalError::InvalidReplay)?;
                    if change.next_step {
                        state.next_step = Some(change.policy);
                    } else {
                        state.policy = change.policy;
                    }
                }
                "approval/asked" => {
                    let _: ApprovalAsked = serde_json::from_value(event.data)
                        .map_err(|_| ApprovalError::InvalidReplay)?;
                    state.next_step = None;
                }
                _ => {}
            }
        }
        Ok(Self {
            inner: Arc::new(ApprovalInner {
                _agent: agent,
                authority,
                session,
                state: Mutex::new(state),
                write_gate: AsyncMutex::new(()),
            }),
        })
    }

    /// Publishes this service under `harness.approval@1` for the owning scope lifetime.
    pub fn publish(&self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(approval_service_key(), self.clone())
    }

    pub fn agent_id(&self) -> SessionId {
        self.inner.authority.id()
    }

    pub fn policy(&self) -> ApprovalPolicy {
        lock(&self.inner.state).policy
    }

    /// Changes the durable default policy.
    pub async fn set_policy(
        &self,
        policy: ApprovalPolicy,
        cancellation: CancellationToken,
    ) -> Result<(), ApprovalError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        let _gate = self.inner.write_gate.lock().await;
        check_cancellation(&cancellation)?;
        self.require_live()?;
        append(
            &self.inner.session,
            "approval/policy",
            serde_json::to_value(ApprovalPolicyChange {
                policy,
                next_step: false,
            })
            .expect("approval policy is serializable"),
            cancellation,
        )
        .await?;
        lock(&self.inner.state).policy = policy;

        Ok(())
    }

    /// Sets one durable override that is consumed by the next active-turn decision.
    pub async fn override_next_step(
        &self,
        policy: ApprovalPolicy,
        cancellation: CancellationToken,
    ) -> Result<(), ApprovalError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        let _gate = self.inner.write_gate.lock().await;
        check_cancellation(&cancellation)?;
        self.require_live()?;
        append(
            &self.inner.session,
            "approval/policy",
            serde_json::to_value(ApprovalPolicyChange {
                policy,
                next_step: true,
            })
            .expect("approval policy is serializable"),
            cancellation,
        )
        .await?;
        lock(&self.inner.state).next_step = Some(policy);

        Ok(())
    }

    /// Registers an owner-scoped waterfall answerer until the returned handle is closed or dropped.
    pub fn register_answerer(
        &self,
        owner: &AgentAuthority,
        answerer: Arc<dyn ApprovalAnswerer>,
    ) -> Result<ApprovalRegistration, ApprovalError> {
        self.require_owner(owner)?;
        let id = next_id(&mut lock(&self.inner.state));
        lock(&self.inner.state).answerers.insert(id, answerer);
        Ok(ApprovalRegistration::answerer(
            Arc::downgrade(&self.inner),
            id,
        ))
    }

    /// Registers an owner-scoped deny-only hook until the returned handle is closed or dropped.
    pub fn register_hook(
        &self,
        owner: &AgentAuthority,
        hook: Arc<dyn ApprovalHook>,
    ) -> Result<ApprovalRegistration, ApprovalError> {
        self.require_owner(owner)?;
        let id = next_id(&mut lock(&self.inner.state));
        lock(&self.inner.state).hooks.insert(id, hook);
        Ok(ApprovalRegistration::hook(Arc::downgrade(&self.inner), id))
    }

    /// Decides one request only while a turn is active, always denying on infrastructure failure.
    pub async fn approve(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> ApprovalOutcome {
        let tool_name = request.action.clone();
        self.approve_with_audit(request, cancellation, tool_name, None, None)
            .await
    }

    async fn approve_with_audit(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
        tool_name: String,
        call_id: Option<ToolCallId>,
        reason: Option<String>,
    ) -> ApprovalOutcome {
        if self.require_live().is_err() || request.validate().is_err() {
            return ApprovalOutcome::Unavailable;
        }
        if cancellation.is_cancelled() {
            return ApprovalOutcome::Cancelled;
        }
        let Some(turn) = active_turn(&self.inner.session) else {
            return ApprovalOutcome::Unavailable;
        };
        let approval_id = ApprovalId::random();

        let (generation, policy, hooks, answerers) = {
            let _gate = self.inner.write_gate.lock().await;
            if cancellation.is_cancelled() {
                return ApprovalOutcome::Cancelled;
            }
            if self.require_live().is_err() || active_turn(&self.inner.session) != Some(turn) {
                return ApprovalOutcome::Rejected;
            }

            let (generation, policy, consumes_next_step, hooks, answerers) = {
                let mut state = lock(&self.inner.state);
                let generation = next_id(&mut state);
                let consumes_next_step = state.next_step.is_some();
                let policy = state.next_step.unwrap_or(state.policy);
                let hooks = state.hooks.values().cloned().collect::<Vec<_>>();
                let answerers = state.answerers.values().cloned().collect::<Vec<_>>();
                (generation, policy, consumes_next_step, hooks, answerers)
            };
            let asked = ApprovalAsked {
                approval_id: approval_id.clone(),
                turn,
                policy,
                request: request.clone(),
                tool_name,
                call_id,
                reason,
            };
            if append(
                &self.inner.session,
                "approval/asked",
                serde_json::to_value(asked).expect("approval request is serializable"),
                cancellation.clone(),
            )
            .await
            .is_err()
            {
                return ApprovalOutcome::Rejected;
            }
            let mut state = lock(&self.inner.state);
            if consumes_next_step {
                state.next_step = None;
            }
            state.pending_decisions.insert(generation);
            (generation, policy, hooks, answerers)
        };

        // Hooks and answerers may re-enter this service, so the write gate is released here.
        let outcome = if cancellation.is_cancelled() {
            ApprovalOutcome::Cancelled
        } else if hooks.iter().any(|hook| {
            catch_unwind(AssertUnwindSafe(|| hook.deny(&request)))
                .map_or(true, |denial| denial.is_some())
        }) {
            ApprovalOutcome::Rejected
        } else {
            match policy {
                ApprovalPolicy::Allow => ApprovalOutcome::AllowedOnce,
                ApprovalPolicy::Never => ApprovalOutcome::Rejected,
                ApprovalPolicy::Ask => {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => ApprovalOutcome::Cancelled,
                        answer = waterfall(answerers, request, cancellation.clone()) => answer,
                    }
                }
            }
        };

        let _gate = self.inner.write_gate.lock().await;
        let matches_reservation = lock(&self.inner.state)
            .pending_decisions
            .remove(&generation);
        let outcome = if !matches_reservation {
            ApprovalOutcome::Rejected
        } else if outcome == ApprovalOutcome::AllowedOnce && cancellation.is_cancelled() {
            ApprovalOutcome::Cancelled
        } else if outcome == ApprovalOutcome::AllowedOnce
            && (self.require_live().is_err() || active_turn(&self.inner.session) != Some(turn))
        {
            ApprovalOutcome::Rejected
        } else {
            outcome
        };
        let finalization = ContextHandle::root().scope().cancellation();
        if append(
            &self.inner.session,
            "approval/decided",
            serde_json::to_value(ApprovalDecision {
                approval_id,
                turn,
                outcome,
            })
            .expect("approval decision is serializable"),
            finalization,
        )
        .await
        .is_err()
        {
            ApprovalOutcome::Rejected
        } else {
            outcome
        }
    }

    /// Adapts this service to the existing `ToolRuntime` approval hook.
    pub fn tool_approval(&self) -> Arc<dyn ToolApproval> {
        Arc::new(ApprovalToolGate {
            approvals: self.clone(),
        })
    }

    fn require_live(&self) -> Result<(), ApprovalError> {
        if self.inner.authority.is_live() {
            Ok(())
        } else {
            Err(ApprovalError::NotLive)
        }
    }

    fn require_owner(&self, owner: &AgentAuthority) -> Result<(), ApprovalError> {
        self.require_live()?;
        if owner.is_live() && same_authority(owner, &self.inner.authority) {
            Ok(())
        } else {
            Err(ApprovalError::NotLive)
        }
    }
    fn matches_owner(&self, owner: &AgentAuthority) -> bool {
        same_authority(owner, &self.inner.authority)
    }
}

/// One owned answerer or hook registration. Drop removes exactly that registration.
pub struct ApprovalRegistration {
    inner: Weak<ApprovalInner>,
    id: u64,
    kind: RegistrationKind,
    closed: AtomicBool,
}

#[derive(Clone, Copy)]
enum RegistrationKind {
    Answerer,
    Hook,
}

impl ApprovalRegistration {
    fn answerer(inner: Weak<ApprovalInner>, id: u64) -> Self {
        Self {
            inner,
            id,
            kind: RegistrationKind::Answerer,
            closed: AtomicBool::new(false),
        }
    }

    fn hook(inner: Weak<ApprovalInner>, id: u64) -> Self {
        Self {
            inner,
            id,
            kind: RegistrationKind::Hook,
            closed: AtomicBool::new(false),
        }
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut state = lock(&inner.state);
        match self.kind {
            RegistrationKind::Answerer => {
                state.answerers.remove(&self.id);
            }
            RegistrationKind::Hook => {
                state.hooks.remove(&self.id);
            }
        }
    }
}

impl Drop for ApprovalRegistration {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum HostApprovalError {
    #[error("approval owner is not live")]
    NotLive,
    #[error("approval service does not belong to the supplied owner")]
    OwnerMismatch,
    #[error("an approval service is already installed for this session")]
    AlreadyInstalled,
    #[error(transparent)]
    Approval(#[from] ApprovalError),
}

struct HostApprovalSlot {
    authority: AgentAuthority,
    approvals: ApprovalService,
}

struct HostApprovalRegistryInner {
    slots: Mutex<BTreeMap<SessionId, HostApprovalSlot>>,
}

/// Host-wide router for approval services owned by exact live agent generations.
#[derive(Clone)]
pub struct HostApprovalRegistry {
    inner: Arc<HostApprovalRegistryInner>,
}

impl Default for HostApprovalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HostApprovalRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HostApprovalRegistryInner {
                slots: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Installs the only service slot for one exact live agent generation.
    pub fn install(
        &self,
        owner: &AgentAuthority,
        approvals: ApprovalService,
    ) -> Result<HostApprovalRegistration, HostApprovalError> {
        if !owner.is_live() {
            return Err(HostApprovalError::NotLive);
        }
        if !approvals.matches_owner(owner) {
            return Err(HostApprovalError::OwnerMismatch);
        }
        let session = owner.id();
        let mut slots = lock(&self.inner.slots);
        if slots.contains_key(&session) {
            return Err(HostApprovalError::AlreadyInstalled);
        }
        slots.insert(
            session.clone(),
            HostApprovalSlot {
                authority: owner.clone(),
                approvals,
            },
        );
        Ok(HostApprovalRegistration {
            inner: Arc::downgrade(&self.inner),
            session,
            authority: owner.clone(),
            closed: AtomicBool::new(false),
        })
    }

    /// Looks up the currently live generation's approval service for trusted host code.
    pub fn lookup(&self, session: &SessionId) -> Option<ApprovalService> {
        let slots = lock(&self.inner.slots);
        slots
            .get(session)
            .filter(|slot| slot.authority.is_live())
            .map(|slot| slot.approvals.clone())
    }

    /// Registers an API-owned answerer against the current exact agent generation.
    pub fn register_answerer(
        &self,
        session: &SessionId,
        answerer: Arc<dyn ApprovalAnswerer>,
    ) -> Result<ApprovalRegistration, HostApprovalError> {
        let (authority, approvals) = {
            let slots = lock(&self.inner.slots);
            let Some(slot) = slots.get(session) else {
                return Err(HostApprovalError::NotLive);
            };
            if !slot.authority.is_live() {
                return Err(HostApprovalError::NotLive);
            }
            (slot.authority.clone(), slot.approvals.clone())
        };
        Ok(approvals.register_answerer(&authority, answerer)?)
    }
}

/// Lifetime owner of one exact host approval service slot.
pub struct HostApprovalRegistration {
    inner: Weak<HostApprovalRegistryInner>,
    session: SessionId,
    authority: AgentAuthority,
    closed: AtomicBool,
}

impl HostApprovalRegistration {
    /// Removes this exact slot once, never a later generation for the same session.
    pub fn close(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.inner.upgrade().is_some_and(|inner| {
            let mut slots = lock(&inner.slots);
            if slots
                .get(&self.session)
                .is_some_and(|slot| same_authority(&slot.authority, &self.authority))
            {
                slots.remove(&self.session);
                true
            } else {
                false
            }
        })
    }
}

impl Drop for HostApprovalRegistration {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[async_trait]
impl ToolApproval for HostApprovalRegistry {
    async fn approve(
        &self,
        context: &ToolRunContext,
        schema: &ToolSchema,
        arguments: &Value,
    ) -> ToolApprovalResult {
        let Some(approvals) = self.lookup(&context.session) else {
            return Ok(Some(false));
        };
        let outcome = approvals
            .approve_with_audit(
                ApprovalRequest {
                    action: schema.name.clone(),
                    details: arguments.clone(),
                },
                context.cancellation.clone(),
                schema.name.clone(),
                Some(context.call.clone()),
                None,
            )
            .await;
        Ok(Some(outcome.allows()))
    }
}

struct ApprovalToolGate {
    approvals: ApprovalService,
}

#[async_trait]
impl ToolApproval for ApprovalToolGate {
    async fn approve(
        &self,
        context: &ToolRunContext,
        schema: &ToolSchema,
        arguments: &Value,
    ) -> ToolApprovalResult {
        if context.session != self.approvals.agent_id() {
            return Ok(Some(false));
        }
        let outcome = self
            .approvals
            .approve_with_audit(
                ApprovalRequest {
                    action: schema.name.clone(),
                    details: arguments.clone(),
                },
                context.cancellation.clone(),
                schema.name.clone(),
                Some(context.call.clone()),
                None,
            )
            .await;
        Ok(Some(outcome.allows()))
    }
}

async fn waterfall(
    answerers: Vec<Arc<dyn ApprovalAnswerer>>,
    request: ApprovalRequest,
    cancellation: CancellationToken,
) -> ApprovalOutcome {
    for answerer in answerers {
        match AssertUnwindSafe(answerer.answer(request.clone(), cancellation.clone()))
            .catch_unwind()
            .await
        {
            Ok(Ok(Some(true))) => return ApprovalOutcome::AllowedOnce,
            Ok(Ok(Some(false))) => return ApprovalOutcome::Rejected,
            Ok(Ok(None)) => {}
            Ok(Err(_)) | Err(_) => return ApprovalOutcome::Unavailable,
        }
    }
    ApprovalOutcome::Unavailable
}

fn active_turn(session: &Session) -> Option<u64> {
    let mut active = None;
    for event in session.events() {
        match event.event_type.as_str() {
            "turn/start" => active = event.data.get("turn").and_then(Value::as_u64),
            "turn/end" => active = None,
            _ => {}
        }
    }
    active
}

async fn append(
    session: &Session,
    event_type: &str,
    data: Value,
    cancellation: CancellationToken,
) -> Result<(), ApprovalError> {
    session
        .append(
            SessionEvent {
                event_type: event_type.into(),
                seq: session.next_seq()?,
                time: 0,
                data,
                ignorable: Some(true),
                source_event_seqs: None,
                surface_op: None,
            },
            cancellation,
        )
        .await?;
    Ok(())
}

fn next_id(state: &mut ApprovalState) -> u64 {
    state.next_id = state.next_id.checked_add(1).unwrap_or(1);
    state.next_id
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), ApprovalError> {
    if cancellation.is_cancelled() {
        Err(ApprovalError::Cancelled)
    } else {
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

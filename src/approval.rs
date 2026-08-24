//! Fail-closed, durable approval decisions and deny-only scoped hooks.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::{
    sync::{broadcast, oneshot, Mutex as AsyncMutex},
    time::{sleep_until, Instant},
};

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
#[derive(Clone, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
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
    #[serde(default)]
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

fn serialize_durable_request<S>(request: &ApprovalRequest, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    #[derive(Serialize)]
    struct DurableRequest<'a> {
        action: &'a str,
    }

    DurableRequest {
        action: &request.action,
    }
    .serialize(serializer)
}

/// Durable whole-value approval policy payload.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalPolicyChange {
    pub policy: ApprovalPolicy,
}

/// Durable audit event emitted before answerers are called.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalAsked {
    #[serde(default)]
    pub approval_id: ApprovalId,
    #[serde(default = "missing_session_id")]
    pub session_id: SessionId,
    pub turn: u64,
    pub policy: ApprovalPolicy,
    #[serde(serialize_with = "serialize_durable_request")]
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
        asked: ApprovalAsked,
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
        asked: ApprovalAsked,
        cancellation: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        (**self).answer(asked, cancellation).await
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
    next_id: u64,
    pending_decisions: BTreeSet<u64>,
    answerers: BTreeMap<u64, Arc<dyn ApprovalAnswerer>>,
    host_answerers: BTreeMap<u64, Arc<dyn ApprovalAnswerer>>,
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
    /// Restores the persistent fail-closed policy.
    pub fn new(agent: AgentHandle) -> Result<Self, ApprovalError> {
        let authority = agent.authority();
        let session = agent.session();
        if !authority.is_live() || authority.id() != session.id() {
            return Err(ApprovalError::NotLive);
        }
        let mut state = ApprovalState::default();
        for event in session.events() {
            if event.event_type == "approval/policy" {
                let change: ApprovalPolicyChange =
                    serde_json::from_value(event.data).map_err(|_| ApprovalError::InvalidReplay)?;
                state.policy = change.policy;
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
            serde_json::to_value(ApprovalPolicyChange { policy })
                .expect("approval policy is serializable"),
            cancellation,
        )
        .await?;
        lock(&self.inner.state).policy = policy;

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

    /// Registers the host authority after ordinary answerers so trusted local
    /// integrations can still answer without entering the browser flow.
    fn register_host_answerer(
        &self,
        owner: &AgentAuthority,
        answerer: Arc<dyn ApprovalAnswerer>,
    ) -> Result<ApprovalRegistration, ApprovalError> {
        self.require_owner(owner)?;
        let id = next_id(&mut lock(&self.inner.state));
        lock(&self.inner.state).host_answerers.insert(id, answerer);
        Ok(ApprovalRegistration::host_answerer(
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

        let (generation, policy, hooks, answerers, asked) = {
            let _gate = self.inner.write_gate.lock().await;
            if cancellation.is_cancelled() {
                return ApprovalOutcome::Cancelled;
            }
            if self.require_live().is_err() || active_turn(&self.inner.session) != Some(turn) {
                return ApprovalOutcome::Rejected;
            }

            let (generation, policy, hooks, answerers) = {
                let mut state = lock(&self.inner.state);
                let generation = next_id(&mut state);
                let policy = state.policy;
                let hooks = state.hooks.values().cloned().collect::<Vec<_>>();
                let answerers = state
                    .answerers
                    .values()
                    .chain(state.host_answerers.values())
                    .cloned()
                    .collect::<Vec<_>>();
                (generation, policy, hooks, answerers)
            };
            let asked = ApprovalAsked {
                approval_id: approval_id.clone(),
                session_id: self.inner.authority.id(),
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
                serde_json::to_value(asked.clone()).expect("approval request is serializable"),
                cancellation.clone(),
            )
            .await
            .is_err()
            {
                return ApprovalOutcome::Rejected;
            }
            let mut state = lock(&self.inner.state);
            state.pending_decisions.insert(generation);
            (generation, policy, hooks, answerers, asked)
        };

        // Hooks and answerers may re-enter this service, so the write gate is released here.
        let outcome = if cancellation.is_cancelled() {
            ApprovalOutcome::Cancelled
        } else if hooks.iter().any(|hook| {
            catch_unwind(AssertUnwindSafe(|| hook.deny(&asked.request)))
                .map_or(true, |denial| denial.is_some())
        }) {
            ApprovalOutcome::Rejected
        } else {
            match policy {
                ApprovalPolicy::Never => ApprovalOutcome::Rejected,
                ApprovalPolicy::Ask => {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => ApprovalOutcome::Cancelled,
                        answer = waterfall(answerers, asked, cancellation.clone()) => answer,
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

enum RegistrationKind {
    Answerer,
    HostAnswerer,
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

    fn host_answerer(inner: Weak<ApprovalInner>, id: u64) -> Self {
        Self {
            inner,
            id,
            kind: RegistrationKind::HostAnswerer,
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
            RegistrationKind::HostAnswerer => {
                state.host_answerers.remove(&self.id);
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

pub const DEFAULT_MAX_PENDING_APPROVALS: usize = 256;
pub const DEFAULT_APPROVAL_DEADLINE: Duration = Duration::from_secs(60);

/// Minimal browser-visible metadata. Tool arguments deliberately never cross
/// this authority boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequested {
    pub rpc_id: String,
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<ToolCallId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A final host-side notice emitted only after the matching decision is durable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalResolved {
    pub rpc_id: String,
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    pub outcome: ApprovalOutcome,
}

#[derive(Clone, Debug)]
pub enum ApprovalNotification {
    Requested(ApprovalRequested),
    Resolved(ApprovalResolved),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RpcReceiptReason {
    NotPending,
    BadResponse,
}

/// The exceptional raw receipt returned by `/api/respond`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcReceipt {
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<RpcReceiptReason>,
}

impl RpcReceipt {
    pub(crate) fn accepted() -> Self {
        Self {
            accepted: true,
            reason: None,
        }
    }

    pub fn not_pending() -> Self {
        Self {
            accepted: false,
            reason: Some(RpcReceiptReason::NotPending),
        }
    }

    pub fn bad_response() -> Self {
        Self {
            accepted: false,
            reason: Some(RpcReceiptReason::BadResponse),
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PendingKey {
    session_id: SessionId,
    approval_id: ApprovalId,
}

struct PendingInteraction {
    requested: ApprovalRequested,
    generation: AgentAuthority,
    turn: u64,
    cancellation: CancellationToken,
    sender: Option<oneshot::Sender<ApprovalOutcome>>,
    deadline: Instant,
    claimed: bool,
}

#[derive(Default)]
struct PendingState {
    entries: BTreeMap<String, PendingInteraction>,
    relayed_asked: BTreeSet<PendingKey>,
    relayed_order: VecDeque<PendingKey>,
}

struct HostApprovalSlot {
    authority: AgentAuthority,
    approvals: ApprovalService,
    _answerer: ApprovalRegistration,
}

struct HostApprovalRegistryInner {
    slots: Mutex<BTreeMap<SessionId, HostApprovalSlot>>,
    pending: Mutex<PendingState>,
    notices: broadcast::Sender<ApprovalNotification>,
    max_pending: usize,
    deadline: Duration,
}

/// Host-wide, generation-bound authority for pending browser approvals.
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
        Self::with_limits(DEFAULT_MAX_PENDING_APPROVALS, DEFAULT_APPROVAL_DEADLINE)
    }

    pub fn with_limits(max_pending: usize, deadline: Duration) -> Self {
        let (notices, _) = broadcast::channel(DEFAULT_MAX_PENDING_APPROVALS);
        Self {
            inner: Arc::new(HostApprovalRegistryInner {
                slots: Mutex::new(BTreeMap::new()),
                pending: Mutex::new(PendingState::default()),
                notices,
                max_pending,
                deadline,
            }),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ApprovalNotification> {
        self.inner.notices.subscribe()
    }

    /// Installs the only service slot and its browser answerer for one exact live generation.
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
        let answerer = approvals.register_host_answerer(
            owner,
            Arc::new(HostRegistryAnswerer {
                registry: Arc::downgrade(&self.inner),
                session: session.clone(),
                authority: owner.clone(),
            }),
        )?;
        let mut slots = lock(&self.inner.slots);
        if slots.contains_key(&session) {
            return Err(HostApprovalError::AlreadyInstalled);
        }
        slots.insert(
            session.clone(),
            HostApprovalSlot {
                authority: owner.clone(),
                approvals,
                _answerer: answerer,
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

    /// Registers a trusted local answerer against the current exact generation.
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

    /// Current unclaimed requests are replayed to a reconnecting mux client with their original rpc ids.
    pub fn snapshots(&self) -> Vec<ApprovalRequested> {
        let now = Instant::now();
        lock(&self.inner.pending)
            .entries
            .values()
            .filter(|entry| !entry.claimed && entry.deadline > now)
            .map(|entry| entry.requested.clone())
            .collect()
    }

    /// Whether an rpc id is still eligible for an approval response.
    pub fn is_pending(&self, rpc_id: &str) -> bool {
        lock(&self.inner.pending)
            .entries
            .get(rpc_id)
            .is_some_and(|entry| !entry.claimed && entry.deadline > Instant::now())
    }

    /// First valid browser response wins. Mismatches never disclose or mutate a pending request.
    pub fn respond(
        &self,
        rpc_id: &str,
        session_id: &SessionId,
        approval_id: &ApprovalId,
        outcome: ApprovalOutcome,
    ) -> RpcReceipt {
        if !matches!(
            outcome,
            ApprovalOutcome::AllowedOnce | ApprovalOutcome::Rejected
        ) {
            return RpcReceipt::bad_response();
        }
        let sender = {
            let mut pending = lock(&self.inner.pending);
            let Some(entry) = pending.entries.get_mut(rpc_id) else {
                return RpcReceipt::not_pending();
            };
            if entry.claimed || entry.deadline <= Instant::now() {
                return RpcReceipt::not_pending();
            }
            if &entry.requested.session_id != session_id
                || &entry.requested.approval_id != approval_id
            {
                return RpcReceipt::bad_response();
            }
            entry.claimed = true;
            entry.sender.take()
        };
        if sender.is_some_and(|sender| sender.send(outcome).is_err()) {
            return RpcReceipt::not_pending();
        }
        RpcReceipt::accepted()
    }

    /// Called by the host's durable session relay after it has published the asked event.
    pub fn observe_asked(&self, asked: &ApprovalAsked) {
        let key = pending_key(asked);
        let requested = {
            let mut pending = lock(&self.inner.pending);
            remember_relayed_asked(&mut pending, key.clone(), self.inner.max_pending);
            pending
                .entries
                .values()
                .find(|entry| pending_key_from_requested(&entry.requested) == key && !entry.claimed)
                .map(|entry| entry.requested.clone())
        };
        if let Some(requested) = requested {
            let _ = self
                .inner
                .notices
                .send(ApprovalNotification::Requested(requested));
        }
    }

    /// Called by the host's durable session relay after it has published the decision event.
    pub fn observe_decided(&self, session_id: &SessionId, decision: &ApprovalDecision) {
        let rpc_id = {
            let pending = lock(&self.inner.pending);
            pending.entries.iter().find_map(|(rpc_id, entry)| {
                (entry.requested.session_id == *session_id
                    && entry.requested.approval_id == decision.approval_id)
                    .then(|| rpc_id.clone())
            })
        };
        let resolved = rpc_id
            .and_then(|rpc_id| lock(&self.inner.pending).entries.remove(&rpc_id))
            .map(|entry| ApprovalResolved {
                rpc_id: entry.requested.rpc_id,
                session_id: entry.requested.session_id,
                approval_id: entry.requested.approval_id,
                outcome: decision.outcome,
            });
        if let Some(resolved) = resolved {
            let _ = self
                .inner
                .notices
                .send(ApprovalNotification::Resolved(resolved));
        }
    }

    /// Cancellation is an authority decision: browser responses arriving afterwards are not pending.
    pub fn cancel_session(&self, session: &SessionId) {
        cancel_pending(&self.inner, |entry| &entry.requested.session_id == session);
    }

    /// Cancels browser authority for one completed turn without disturbing later turns.
    pub fn cancel_turn(&self, session: &SessionId, turn: u64) {
        cancel_pending(&self.inner, |entry| {
            &entry.requested.session_id == session && entry.turn == turn
        });
    }

    pub fn cancel_all(&self) {
        cancel_pending(&self.inner, |_| true);
    }

    async fn answer(
        &self,
        session: &SessionId,
        authority: &AgentAuthority,
        asked: ApprovalAsked,
        cancellation: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        let Some((receiver, deadline)) =
            self.register_pending(session, authority, &asked, cancellation.clone())
        else {
            return Ok(None);
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Ok(None),
            answer = receiver => Ok(match answer {
                Ok(ApprovalOutcome::AllowedOnce) => Some(true),
                Ok(ApprovalOutcome::Rejected) => Some(false),
                Ok(ApprovalOutcome::Cancelled | ApprovalOutcome::Unavailable) | Err(_) => None,
            }),
            _ = sleep_until(deadline) => {
                self.claim_timeout(&asked.approval_id, session);
                Ok(None)
            }
        }
    }

    fn register_pending(
        &self,
        session: &SessionId,
        authority: &AgentAuthority,
        asked: &ApprovalAsked,
        cancellation: CancellationToken,
    ) -> Option<(oneshot::Receiver<ApprovalOutcome>, Instant)> {
        if asked.session_id != *session || asked.approval_id.as_str().is_empty() {
            return None;
        }
        let (sender, receiver) = oneshot::channel();
        let deadline = Instant::now() + self.inner.deadline;
        let requested = ApprovalRequested {
            rpc_id: uuid::Uuid::new_v4().to_string(),
            session_id: session.clone(),
            approval_id: asked.approval_id.clone(),
            tool_name: asked.tool_name.clone(),
            call_id: asked.call_id.clone(),
            reason: asked.reason.clone(),
        };
        let emit = {
            let slots = lock(&self.inner.slots);
            let slot = slots.get(session)?;
            if !same_authority(&slot.authority, authority) || !authority.is_live() {
                return None;
            }
            let mut pending = lock(&self.inner.pending);
            if self.inner.max_pending == 0 || pending.entries.len() >= self.inner.max_pending {
                return None;
            }
            let key = pending_key(asked);
            let emit = pending.relayed_asked.contains(&key);
            pending.entries.insert(
                requested.rpc_id.clone(),
                PendingInteraction {
                    requested: requested.clone(),
                    generation: authority.clone(),
                    cancellation,
                    turn: asked.turn,
                    sender: Some(sender),
                    deadline,
                    claimed: false,
                },
            );
            emit
        };
        if emit {
            let _ = self
                .inner
                .notices
                .send(ApprovalNotification::Requested(requested));
        }
        Some((receiver, deadline))
    }

    fn claim_timeout(&self, approval_id: &ApprovalId, session: &SessionId) {
        let mut pending = lock(&self.inner.pending);
        if let Some(entry) = pending.entries.values_mut().find(|entry| {
            entry.requested.session_id == *session && entry.requested.approval_id == *approval_id
        }) {
            entry.claimed = true;
            entry.sender.take();
        }
    }
}

struct HostRegistryAnswerer {
    registry: Weak<HostApprovalRegistryInner>,
    session: SessionId,
    authority: AgentAuthority,
}

#[async_trait]
impl ApprovalAnswerer for HostRegistryAnswerer {
    async fn answer(
        &self,
        asked: ApprovalAsked,
        cancellation: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        let Some(inner) = self.registry.upgrade() else {
            return Ok(None);
        };
        HostApprovalRegistry { inner }
            .answer(&self.session, &self.authority, asked, cancellation)
            .await
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
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let removed = {
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
        };
        if removed {
            cancel_pending(&inner, |entry| {
                same_authority(&entry.generation, &self.authority)
            });
        }
        removed
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

fn pending_key(asked: &ApprovalAsked) -> PendingKey {
    PendingKey {
        session_id: asked.session_id.clone(),
        approval_id: asked.approval_id.clone(),
    }
}

fn pending_key_from_requested(requested: &ApprovalRequested) -> PendingKey {
    PendingKey {
        session_id: requested.session_id.clone(),
        approval_id: requested.approval_id.clone(),
    }
}

fn remember_relayed_asked(state: &mut PendingState, key: PendingKey, limit: usize) {
    if limit == 0 || !state.relayed_asked.insert(key.clone()) {
        return;
    }
    state.relayed_order.push_back(key);
    while state.relayed_order.len() > limit {
        if let Some(oldest) = state.relayed_order.pop_front() {
            state.relayed_asked.remove(&oldest);
        }
    }
}

fn cancel_pending(
    inner: &HostApprovalRegistryInner,
    matches: impl Fn(&PendingInteraction) -> bool,
) {
    let mut pending = lock(&inner.pending);
    for entry in pending.entries.values_mut().filter(|entry| matches(entry)) {
        if !entry.claimed {
            entry.claimed = true;
            entry.cancellation.cancel();
            entry.sender.take();
        }
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
    asked: ApprovalAsked,
    cancellation: CancellationToken,
) -> ApprovalOutcome {
    for answerer in answerers {
        match AssertUnwindSafe(answerer.answer(asked.clone(), cancellation.clone()))
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

fn missing_session_id() -> SessionId {
    SessionId::from("")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

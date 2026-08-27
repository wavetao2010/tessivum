//! Process-local agent ownership, inbox delivery, and registry coordination.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
};

pub use crate::protocol::AgentCancelCause;
use async_trait::async_trait;
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::{
    agent_mode::AgentModeId,
    protocol::{ContentBlock, Message, MessageId, SessionHeader, SessionId},
    session::{RestoreMode, Session, SessionError, SessionStore},
    TessivumError,
};

/// The stable service key for the process-local agent registry.
pub fn agents_service_key() -> ServiceKey {
    ServiceKey::new("harness.agents", "1")
}

/// The two observable states of an agent driver.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Running,
}

/// The point in agent execution at which an inbox message is consumed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InboxTarget {
    /// Starts one replacement turn after the current work becomes idle.
    Followup,
    /// Is consumed by the nearest next agent step.
    Steer,
    /// Is consumed by the next pre-step without starting work itself.
    Inject,
}

/// One bounded, id-addressed pending inbox mutation.
#[derive(Clone, Debug, PartialEq)]
pub enum InboxUpdate {
    Edit { content: Vec<ContentBlock> },
    Remove,
    Steer,
}

/// The committed result of one [`InboxUpdate`].
#[derive(Clone, Debug, PartialEq)]
pub enum InboxUpdateResult {
    Updated {
        target: InboxTarget,
        message: Message,
    },
    NotPending,
    SteerUnavailable,
}

/// A claimed inbox batch held until its durable deletion splice commits.
pub(crate) struct InboxClaimReservation {
    inbox: Inbox,
    target: &'static str,
    entries: Vec<(InboxTarget, MessageId)>,
    messages: Vec<Message>,
    active: bool,
}

impl InboxClaimReservation {
    pub(crate) fn target(&self) -> &'static str {
        self.target
    }

    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub(crate) fn commit(mut self) -> Option<Vec<Message>> {
        if !self.inbox.commit_claim(&self) {
            return None;
        }
        self.active = false;
        Some(std::mem::take(&mut self.messages))
    }
}

impl Drop for InboxClaimReservation {
    fn drop(&mut self) {
        if self.active {
            self.inbox.abort_claim(&self.entries);
        }
    }
}

/// A pending queue mutation held out of agent consumption until its durable event commits.
pub(crate) struct InboxUpdateReservation {
    inbox: Inbox,
    item_id: MessageId,
    update: InboxUpdate,
    target: InboxTarget,
    message: Message,
    source_target: InboxTarget,
    start: usize,
    destination_start: Option<usize>,
    wakes: bool,
    active: bool,
}

impl InboxUpdateReservation {
    pub(crate) fn message(&self) -> &Message {
        &self.message
    }

    pub(crate) fn source_target(&self) -> InboxTarget {
        self.source_target
    }

    pub(crate) fn start(&self) -> usize {
        self.start
    }

    pub(crate) fn destination_start(&self) -> Option<usize> {
        self.destination_start
    }

    fn commits_wake(&self) -> bool {
        self.wakes
    }

    fn commit(mut self) -> bool {
        let committed = self.inbox.commit_reservation(&self);
        self.active = false;
        committed
    }
}

impl Drop for InboxUpdateReservation {
    fn drop(&mut self) {
        if self.active {
            self.inbox.abort_reservation(&self.item_id);
        }
    }
}

// Boxing the one-shot reservation would add a heap allocation to every successful update.
#[allow(clippy::large_enum_variant)]
pub(crate) enum InboxReservationResult {
    Reserved(InboxUpdateReservation),
    NotPending,
    SteerUnavailable,
}
/// Model route options owned by an agent, never a separate session identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOptions {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

impl AgentOptions {
    fn validate(&self) -> Result<(), AgentError> {
        if self.provider.trim().is_empty() {
            return Err(AgentError::InvalidOptions("provider must not be empty"));
        }
        if self.model.trim().is_empty() {
            return Err(AgentError::InvalidOptions("model must not be empty"));
        }
        if self
            .reasoning_effort
            .as_deref()
            .is_some_and(|effort| effort.trim().is_empty())
        {
            return Err(AgentError::InvalidOptions(
                "reasoning_effort must not be blank when present",
            ));
        }
        if self.max_tokens == Some(0) {
            return Err(AgentError::InvalidOptions(
                "max_tokens must be positive when present",
            ));
        }
        Ok(())
    }
}

/// First-wins cancellation details for an active agent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCancelOptions {
    pub cause: AgentCancelCause,
    pub keep_inbox: bool,
}

/// Errors at the process-local agent boundary.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum AgentError {
    #[error("an agent factory is already registered")]
    DuplicateFactory,
    #[error("no agent factory is registered")]
    FactoryNotRegistered,
    #[error("agent is not live for session {0}")]
    NotFound(SessionId),
    #[error("an inbox message id is already pending: {0}")]
    DuplicateInboxMessage(MessageId),
    #[error("agent setup was cancelled")]
    Cancelled,
    #[error("agent is disposed")]
    Disposed,
    #[error("invalid agent options: {0}")]
    InvalidOptions(&'static str),
    #[error("agent setup reservation was lost")]
    SetupReservationLost,
    #[error("agent runtime failed: {0}")]
    Runtime(String),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Message(#[from] TessivumError),
}

#[derive(Default)]
struct InboxState {
    followups: VecDeque<Message>,
    steers: VecDeque<Message>,
    injections: VecDeque<Message>,
    reservations: BTreeSet<MessageId>,
    step_order: VecDeque<(InboxTarget, MessageId)>,
    changes: u64,
    wakes: u64,
}

struct InboxInner {
    state: Mutex<InboxState>,
    changed: Notify,
    woke: Notify,
}

/// An owned, targeted FIFO inbox. Each target keeps its own delivery order.
#[derive(Clone)]
pub struct Inbox {
    inner: Arc<InboxInner>,
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Inbox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Inbox")
            .field("len", &self.len())
            .field("wake_revision", &self.wake_revision())
            .finish()
    }
}

impl Inbox {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(InboxInner {
                state: Mutex::new(InboxState::default()),
                changed: Notify::new(),
                woke: Notify::new(),
            }),
        }
    }

    /// Enqueues one validated message. `wakeup` controls the observable wake signal.
    pub fn send(
        &self,
        message: Message,
        target: InboxTarget,
        wakeup: bool,
    ) -> Result<(), AgentError> {
        message.validate()?;
        let mut state = lock(&self.inner.state);
        if pending_contains(&state, &message.id) {
            return Err(AgentError::DuplicateInboxMessage(message.id));
        }
        match target {
            InboxTarget::Followup => state.followups.push_back(message),
            InboxTarget::Steer => {
                state.step_order.push_back((target, message.id.clone()));
                state.steers.push_back(message);
            }
            InboxTarget::Inject => {
                state.step_order.push_back((target, message.id.clone()));
                state.injections.push_back(message);
            }
        }
        bump(&mut state.changes);
        if wakeup {
            bump(&mut state.wakes);
        }
        drop(state);
        self.inner.changed.notify_waiters();
        if wakeup {
            self.inner.woke.notify_waiters();
        }
        Ok(())
    }

    pub fn followup(&self, message: Message) -> Result<(), AgentError> {
        self.send(message, InboxTarget::Followup, true)
    }

    pub fn steer(&self, message: Message) -> Result<(), AgentError> {
        self.send(message, InboxTarget::Steer, true)
    }

    pub fn inject(&self, message: Message) -> Result<(), AgentError> {
        self.send(message, InboxTarget::Inject, false)
    }

    /// Removes the next unreserved message for `target`, preserving that target's FIFO order.
    pub fn take(&self, target: InboxTarget) -> Option<Message> {
        let mut state = lock(&self.inner.state);
        let blocked = match target {
            InboxTarget::Followup => state.followups.front(),
            InboxTarget::Steer => state.steers.front(),
            InboxTarget::Inject => state.injections.front(),
        }
        .is_some_and(|message| state.reservations.contains(&message.id));
        if blocked {
            return None;
        }
        let message = match target {
            InboxTarget::Followup => state.followups.pop_front(),
            InboxTarget::Steer => state.steers.pop_front(),
            InboxTarget::Inject => state.injections.pop_front(),
        }?;
        if target != InboxTarget::Followup {
            state
                .step_order
                .retain(|(queued_target, id)| *queued_target != target || *id != message.id);
        }
        bump(&mut state.changes);
        drop(state);
        self.inner.changed.notify_waiters();
        Some(message)
    }

    pub fn take_pre_step(&self) -> Option<Message> {
        self.take(InboxTarget::Inject)
    }

    pub fn take_next_step(&self) -> Option<Message> {
        self.take(InboxTarget::Steer)
    }

    pub fn has_next_step(&self) -> bool {
        !lock(&self.inner.state).step_order.is_empty()
    }

    /// Claims all next-step work in original arrival order, regardless of source.
    pub fn take_step_batch(&self) -> Vec<Message> {
        let mut state = lock(&self.inner.state);
        let mut messages = Vec::with_capacity(state.step_order.len());
        while let Some((target, id)) = state.step_order.pop_front() {
            let queue = match target {
                InboxTarget::Steer => &mut state.steers,
                InboxTarget::Inject => &mut state.injections,
                InboxTarget::Followup => continue,
            };
            if let Some(index) = pending_index(queue, &id) {
                messages.push(
                    queue
                        .remove(index)
                        .expect("step inbox index remains valid while the inbox lock is held"),
                );
            }
        }
        if messages.is_empty() {
            return messages;
        }
        bump(&mut state.changes);
        drop(state);
        self.inner.changed.notify_waiters();
        messages
    }

    pub fn take_next_turn(&self) -> Option<Message> {
        self.take(InboxTarget::Followup)
    }

    /// Reserves one next-turn message until its durable claim event commits.
    pub(crate) fn reserve_next_turn_claim(&self) -> Option<InboxClaimReservation> {
        let mut state = lock(&self.inner.state);
        let message = state.followups.front()?.clone();
        if state.reservations.contains(&message.id) {
            return None;
        }
        state.reservations.insert(message.id.clone());
        Some(InboxClaimReservation {
            inbox: self.clone(),
            target: "next-turn",
            entries: vec![(InboxTarget::Followup, message.id.clone())],
            messages: vec![message],
            active: true,
        })
    }

    /// Reserves all next-step work in original arrival order until its durable claim commits.
    pub(crate) fn reserve_step_batch_claim(&self) -> Option<InboxClaimReservation> {
        let mut state = lock(&self.inner.state);
        if state.step_order.is_empty()
            || state
                .step_order
                .iter()
                .any(|(_, item_id)| state.reservations.contains(item_id))
        {
            return None;
        }
        let mut entries = Vec::with_capacity(state.step_order.len());
        let mut messages = Vec::with_capacity(state.step_order.len());
        for (target, item_id) in &state.step_order {
            let queue = match target {
                InboxTarget::Steer => &state.steers,
                InboxTarget::Inject => &state.injections,
                InboxTarget::Followup => return None,
            };
            let message = queue.iter().find(|message| message.id == *item_id)?.clone();
            entries.push((*target, item_id.clone()));
            messages.push(message);
        }
        for (_, item_id) in &entries {
            state.reservations.insert(item_id.clone());
        }
        Some(InboxClaimReservation {
            inbox: self.clone(),
            target: "next-step",
            entries,
            messages,
            active: true,
        })
    }

    /// Returns every pending occurrence, preserving next-step source order.
    pub fn pending(&self) -> Vec<(InboxTarget, Message)> {
        let state = lock(&self.inner.state);
        let mut messages =
            Vec::with_capacity(state.followups.len() + state.steers.len() + state.injections.len());
        messages.extend(
            state
                .followups
                .iter()
                .cloned()
                .map(|message| (InboxTarget::Followup, message)),
        );
        for (target, id) in &state.step_order {
            let queue = match target {
                InboxTarget::Steer => &state.steers,
                InboxTarget::Inject => &state.injections,
                InboxTarget::Followup => continue,
            };
            if let Some(message) = queue.iter().find(|message| message.id == *id) {
                messages.push((*target, message.clone()));
            }
        }
        messages
    }

    /// Reserves a pending user queue entry until the caller commits or drops the reservation.
    fn reserve_update(
        &self,
        item_id: &MessageId,
        update: InboxUpdate,
        steer_enabled: bool,
    ) -> InboxReservationResult {
        let mut state = lock(&self.inner.state);
        if state.reservations.contains(item_id) {
            return InboxReservationResult::NotPending;
        }
        let (target, start, original) = if let Some((index, message)) = state
            .followups
            .iter()
            .enumerate()
            .find(|(_, message)| &message.id == item_id)
        {
            (InboxTarget::Followup, index, message.clone())
        } else if let Some(message) = state.steers.iter().find(|message| &message.id == item_id) {
            let start = state
                .step_order
                .iter()
                .position(|(_, queued_id)| queued_id == item_id)
                .expect("pending steer remains in step order");
            (InboxTarget::Steer, start, message.clone())
        } else {
            return InboxReservationResult::NotPending;
        };
        let source_target = target;
        let (target, message, wakes) = match &update {
            InboxUpdate::Edit { content } => {
                let mut message = original;
                message.content = content.clone();
                if message.validate().is_err() {
                    return InboxReservationResult::NotPending;
                }
                (target, message, false)
            }
            InboxUpdate::Remove => (target, original, false),
            InboxUpdate::Steer if target == InboxTarget::Followup && steer_enabled => {
                (InboxTarget::Steer, original, true)
            }
            InboxUpdate::Steer => return InboxReservationResult::SteerUnavailable,
        };
        let destination_start =
            matches!(update, InboxUpdate::Steer).then_some(state.step_order.len());
        state.reservations.insert(item_id.clone());
        InboxReservationResult::Reserved(InboxUpdateReservation {
            inbox: self.clone(),
            item_id: item_id.clone(),
            update,
            target,
            message,
            source_target,
            start,
            destination_start,
            wakes,
            active: true,
        })
    }

    fn commit_reservation(&self, reservation: &InboxUpdateReservation) -> bool {
        let mut state = lock(&self.inner.state);
        if !state.reservations.remove(&reservation.item_id) {
            return false;
        }
        let queue = match &reservation.update {
            InboxUpdate::Edit { .. } | InboxUpdate::Remove => reservation.target,
            InboxUpdate::Steer => InboxTarget::Followup,
        };
        let index = match queue {
            InboxTarget::Followup => pending_index(&state.followups, &reservation.item_id),
            InboxTarget::Steer => pending_index(&state.steers, &reservation.item_id),
            InboxTarget::Inject => None,
        };
        let Some(index) = index else {
            return false;
        };
        match &reservation.update {
            InboxUpdate::Edit { .. } => match queue {
                InboxTarget::Followup => state.followups[index] = reservation.message.clone(),
                InboxTarget::Steer => state.steers[index] = reservation.message.clone(),
                InboxTarget::Inject => unreachable!("injected messages cannot be reserved"),
            },
            InboxUpdate::Remove => match queue {
                InboxTarget::Followup => {
                    state.followups.remove(index);
                }
                InboxTarget::Steer => {
                    let message = state
                        .steers
                        .remove(index)
                        .expect("reserved steer remains pending");
                    state
                        .step_order
                        .retain(|(_, item_id)| *item_id != message.id);
                }
                InboxTarget::Inject => unreachable!("injected messages cannot be reserved"),
            },
            InboxUpdate::Steer => {
                let message = state
                    .followups
                    .remove(index)
                    .expect("reserved followup remains pending");
                state
                    .step_order
                    .push_back((InboxTarget::Steer, message.id.clone()));
                state.steers.push_back(message);
            }
        }
        bump(&mut state.changes);
        if reservation.wakes {
            bump(&mut state.wakes);
        }
        drop(state);
        self.inner.changed.notify_waiters();
        if reservation.wakes {
            self.inner.woke.notify_waiters();
        }
        true
    }

    fn commit_claim(&self, reservation: &InboxClaimReservation) -> bool {
        let mut state = lock(&self.inner.state);
        if !reservation
            .entries
            .iter()
            .all(|(_, item_id)| state.reservations.contains(item_id))
        {
            return false;
        }
        for (target, item_id) in &reservation.entries {
            let queue = match target {
                InboxTarget::Followup => &state.followups,
                InboxTarget::Steer => &state.steers,
                InboxTarget::Inject => &state.injections,
            };
            if pending_index(queue, item_id).is_none() {
                return false;
            }
        }
        for (target, item_id) in &reservation.entries {
            let queue = match target {
                InboxTarget::Followup => &mut state.followups,
                InboxTarget::Steer => &mut state.steers,
                InboxTarget::Inject => &mut state.injections,
            };
            let index = pending_index(queue, item_id).expect("reserved inbox item remains pending");
            queue.remove(index);
            state.reservations.remove(item_id);
        }
        if reservation.target == "next-step" {
            state.step_order.retain(|(_, item_id)| {
                !reservation
                    .entries
                    .iter()
                    .any(|(_, claimed_id)| claimed_id == item_id)
            });
        }
        bump(&mut state.changes);
        drop(state);
        self.inner.changed.notify_waiters();
        true
    }

    fn abort_claim(&self, entries: &[(InboxTarget, MessageId)]) {
        let mut state = lock(&self.inner.state);
        for (_, item_id) in entries {
            state.reservations.remove(item_id);
        }
    }

    fn abort_reservation(&self, item_id: &MessageId) {
        let mut state = lock(&self.inner.state);
        if !state.reservations.remove(item_id) || !pending_contains(&state, item_id) {
            return;
        }
        bump(&mut state.wakes);
        drop(state);
        self.inner.woke.notify_waiters();
    }

    /// Mutates one pending next-turn or next-step user message by its stable id.
    /// Injected pre-step context remains intentionally outside this user queue verb.
    pub fn update(&self, item_id: &MessageId, update: InboxUpdate) -> InboxUpdateResult {
        let mut state = lock(&self.inner.state);
        if state.reservations.contains(item_id) {
            return InboxUpdateResult::NotPending;
        }
        let (target, queue) = if let Some(index) = pending_index(&state.followups, item_id) {
            (InboxTarget::Followup, QueueRef::Followup(index))
        } else if let Some(index) = pending_index(&state.steers, item_id) {
            (InboxTarget::Steer, QueueRef::Steer(index))
        } else {
            return InboxUpdateResult::NotPending;
        };

        let wakes = matches!(&update, InboxUpdate::Steer);
        let result = match update {
            InboxUpdate::Edit { content } => {
                let mut updated = queue.message(&state).clone();
                updated.content = content;
                if updated.validate().is_err() {
                    return InboxUpdateResult::NotPending;
                }
                *queue.message_mut(&mut state) = updated.clone();
                InboxUpdateResult::Updated {
                    target,
                    message: updated,
                }
            }
            InboxUpdate::Remove => {
                let message = queue.remove(&mut state);
                if target != InboxTarget::Followup {
                    state.step_order.retain(|(queued_target, id)| {
                        *queued_target != target || *id != message.id
                    });
                }
                InboxUpdateResult::Updated { target, message }
            }
            InboxUpdate::Steer => {
                if target != InboxTarget::Followup {
                    return InboxUpdateResult::SteerUnavailable;
                }
                let message = queue.remove(&mut state);
                state
                    .step_order
                    .push_back((InboxTarget::Steer, message.id.clone()));
                state.steers.push_back(message.clone());
                InboxUpdateResult::Updated {
                    target: InboxTarget::Steer,
                    message,
                }
            }
        };
        bump(&mut state.changes);
        if wakes {
            bump(&mut state.wakes);
        }
        drop(state);
        self.inner.changed.notify_waiters();
        if wakes {
            self.inner.woke.notify_waiters();
        }
        result
    }

    pub fn len(&self) -> usize {
        let state = lock(&self.inner.state);
        state.followups.len() + state.steers.len() + state.injections.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn clear(&self) {
        let mut state = lock(&self.inner.state);
        if state.followups.is_empty() && state.steers.is_empty() && state.injections.is_empty() {
            return;
        }
        state.followups.clear();
        state.steers.clear();
        state.injections.clear();
        state.reservations.clear();
        state.step_order.clear();
        bump(&mut state.changes);
        drop(state);
        self.inner.changed.notify_waiters();
    }

    /// Revision for every accepted inbox mutation.
    pub fn change_revision(&self) -> u64 {
        lock(&self.inner.state).changes
    }

    /// Revision for mutations that explicitly wake work.
    pub fn wake_revision(&self) -> u64 {
        lock(&self.inner.state).wakes
    }

    /// Waits for an explicit wake after `observed`, without polling.
    pub async fn wait_for_wake(&self, observed: u64) -> u64 {
        loop {
            let notified = self.inner.woke.notified();
            let revision = self.wake_revision();
            if revision != observed {
                return revision;
            }
            notified.await;
        }
    }

    /// Waits for any inbox mutation after `observed`, without polling.
    pub async fn wait_for_change(&self, observed: u64) -> u64 {
        loop {
            let notified = self.inner.changed.notified();
            let revision = self.change_revision();
            if revision != observed {
                return revision;
            }
            notified.await;
        }
    }
}

/// The process-local driver behind an agent handle.
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    fn status(&self) -> AgentStatus;

    /// Prompts the driver to process newly wakeable inbox work.
    async fn wake(&self) -> Result<(), AgentError>;

    /// Resolves only after current driver work is idle.
    async fn when_idle(&self) -> Result<(), AgentError>;

    /// Records the first explicit cancellation cause before shared cancellation fires.
    fn cancel(&self, _cause: AgentCancelCause) {}
    /// Stops and awaits all driver-owned work.
    async fn dispose(&self) -> Result<(), AgentError>;
}

/// Constructs one process-local driver using the session identity supplied by the registry.
#[async_trait]
pub trait AgentFactory: Send + Sync {
    async fn create(
        &self,
        session: Arc<Session>,
        options: AgentOptions,
        inbox: Inbox,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError>;
}

struct FactorySlot {
    generation: u64,
    factory: Arc<dyn AgentFactory>,
}

struct LiveAgent {
    generation: u64,
    inner: Arc<AgentInner>,
}

#[derive(Default)]
struct RegistryState {
    next_generation: u64,
    factory: Option<FactorySlot>,
    starting: BTreeSet<SessionId>,
    live: BTreeMap<SessionId, LiveAgent>,
}

struct RegistryInner {
    sessions: SessionStore,
    state: Mutex<RegistryState>,
}

/// Lifetime owner for the sole active agent factory registration.
pub struct AgentFactoryRegistration {
    inner: Weak<RegistryInner>,
    generation: u64,
    closed: AtomicBool,
}

impl fmt::Debug for AgentFactoryRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentFactoryRegistration")
            .field("active", &self.is_active())
            .finish()
    }
}

impl AgentFactoryRegistration {
    /// Removes this factory once, without disturbing a later generation.
    pub fn close(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.inner.upgrade().is_some_and(|inner| {
            let mut state = lock(&inner.state);
            if state
                .factory
                .as_ref()
                .is_some_and(|slot| slot.generation == self.generation)
            {
                state.factory = None;
                true
            } else {
                false
            }
        })
    }

    pub fn is_active(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && self.inner.upgrade().is_some_and(|inner| {
                lock(&inner.state)
                    .factory
                    .as_ref()
                    .is_some_and(|slot| slot.generation == self.generation)
            })
    }
}

impl Drop for AgentFactoryRegistration {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Thread-safe owner of all currently live process-local agents.
#[derive(Clone)]
pub struct AgentRegistry {
    inner: Arc<RegistryInner>,
}

impl fmt::Debug for AgentRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentRegistry")
            .field("live_count", &lock(&self.inner.state).live.len())
            .finish_non_exhaustive()
    }
}

impl AgentRegistry {
    pub fn new(sessions: SessionStore) -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                sessions,
                state: Mutex::new(RegistryState::default()),
            }),
        }
    }

    /// Publishes this registry as a scope-owned `harness.agents@1` service.
    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(agents_service_key(), self)
    }

    /// Registers the sole factory until the returned lifetime handle closes or drops.
    pub fn register_factory(
        &self,
        factory: Arc<dyn AgentFactory>,
    ) -> Result<AgentFactoryRegistration, AgentError> {
        let mut state = lock(&self.inner.state);
        if state.factory.is_some() {
            return Err(AgentError::DuplicateFactory);
        }
        let generation = next_generation(&mut state);
        state.factory = Some(FactorySlot {
            generation,
            factory,
        });
        Ok(AgentFactoryRegistration {
            inner: Arc::downgrade(&self.inner),
            generation,
            closed: AtomicBool::new(false),
        })
    }

    /// Alias for [`Self::register_factory`].
    pub fn register(
        &self,
        factory: Arc<dyn AgentFactory>,
    ) -> Result<AgentFactoryRegistration, AgentError> {
        self.register_factory(factory)
    }

    /// Creates an empty durable session, then publishes the agent only after driver setup succeeds.
    pub async fn create(
        &self,
        header: SessionHeader,
        options: AgentOptions,
        setup_cancellation: CancellationToken,
    ) -> Result<AgentHandle, AgentError> {
        options.validate()?;
        check_setup_cancellation(&setup_cancellation)?;
        let id = header.id.clone();
        let factory = self.reserve(&id)?;
        let session = match self
            .inner
            .sessions
            .create(header, setup_cancellation.clone())
            .await
        {
            Ok(session) => session,
            Err(error) => {
                self.unreserve(&id);
                return Err(error.into());
            }
        };
        let agent_mode = session.header().agent_mode;
        self.finish_setup(
            id,
            session,
            options,
            factory,
            agent_mode,
            setup_cancellation,
        )
        .await
    }

    /// Resumes a durable session using its existing session identity.
    pub async fn resume(
        &self,
        session_id: SessionId,
        options: AgentOptions,
        setup_cancellation: CancellationToken,
    ) -> Result<AgentHandle, AgentError> {
        options.validate()?;
        check_setup_cancellation(&setup_cancellation)?;
        let factory = self.reserve(&session_id)?;
        let session = match self.inner.sessions.get(&session_id) {
            Some(session) => session,
            None => match self
                .inner
                .sessions
                .restore(&session_id, RestoreMode::Live, setup_cancellation.clone())
                .await
            {
                Ok(session) => session,
                Err(error) => {
                    self.unreserve(&session_id);
                    return Err(error.into());
                }
            },
        };
        let agent_mode = session.header().agent_mode;
        self.finish_setup(
            session_id,
            session,
            options,
            factory,
            agent_mode,
            setup_cancellation,
        )
        .await
    }

    /// Uses the already-live or durable session when present; otherwise creates `header`.
    pub async fn create_or_resume(
        &self,
        header: SessionHeader,
        options: AgentOptions,
        setup_cancellation: CancellationToken,
    ) -> Result<AgentHandle, AgentError> {
        options.validate()?;
        check_setup_cancellation(&setup_cancellation)?;
        let id = header.id.clone();
        let factory = self.reserve(&id)?;
        let session = match self.inner.sessions.get(&id) {
            Some(session) => Ok(session),
            None => match self
                .inner
                .sessions
                .restore(&id, RestoreMode::Live, setup_cancellation.clone())
                .await
            {
                Ok(session) => Ok(session),
                Err(SessionError::NotFound(_)) => {
                    self.inner
                        .sessions
                        .create(header, setup_cancellation.clone())
                        .await
                }
                Err(error) => Err(error),
            },
        };
        let session = match session {
            Ok(session) => session,
            Err(error) => {
                self.unreserve(&id);
                return Err(error.into());
            }
        };
        let agent_mode = session.header().agent_mode;
        self.finish_setup(
            id,
            session,
            options,
            factory,
            agent_mode,
            setup_cancellation,
        )
        .await
    }

    pub fn get(&self, session_id: &SessionId) -> Option<AgentHandle> {
        lock(&self.inner.state)
            .live
            .get(session_id)
            .map(|live| AgentHandle::observe(Arc::clone(&live.inner)))
    }

    /// Lists live agents in deterministic session-id order.
    pub fn list(&self) -> Vec<AgentHandle> {
        lock(&self.inner.state)
            .live
            .values()
            .map(|live| AgentHandle::observe(Arc::clone(&live.inner)))
            .collect()
    }

    pub async fn send(
        &self,
        session_id: &SessionId,
        message: Message,
        target: InboxTarget,
        wakeup: bool,
    ) -> Result<(), AgentError> {
        let agent = self.live_inner(session_id)?;
        agent.send(message, target, wakeup).await
    }

    /// Applies first-wins cancellation to an active agent by session identity.
    pub fn cancel(
        &self,
        session_id: &SessionId,
        cause: AgentCancelCause,
        keep_inbox: bool,
    ) -> Result<bool, AgentError> {
        Ok(self
            .live_inner(session_id)?
            .cancel(AgentCancelOptions { cause, keep_inbox }))
    }

    /// Signals cancellation to every live generation without waiting for disposal.
    pub fn cancel_all(&self, cause: AgentCancelCause, keep_inbox: bool) -> usize {
        let agents = lock(&self.inner.state)
            .live
            .values()
            .map(|live| Arc::clone(&live.inner))
            .collect::<Vec<_>>();
        agents
            .into_iter()
            .filter(|agent| {
                agent.cancel(AgentCancelOptions {
                    cause: cause.clone(),
                    keep_inbox,
                })
            })
            .count()
    }

    /// Cancels and awaits every current runtime. All are awaited even if one fails.
    pub async fn dispose_all(&self) -> Result<(), AgentError> {
        let agents = lock(&self.inner.state)
            .live
            .values()
            .map(|live| Arc::clone(&live.inner))
            .collect::<Vec<_>>();
        let results = join_all(
            agents
                .into_iter()
                .map(|inner| async move { inner.dispose().await }),
        )
        .await;
        results
            .into_iter()
            .find_map(Result::err)
            .map_or(Ok(()), Err)
    }

    /// Alias for [`Self::dispose_all`].
    pub async fn shutdown(&self) -> Result<(), AgentError> {
        self.dispose_all().await
    }

    fn reserve(&self, session_id: &SessionId) -> Result<Arc<dyn AgentFactory>, AgentError> {
        let mut state = lock(&self.inner.state);
        let factory = state
            .factory
            .as_ref()
            .map(|slot| Arc::clone(&slot.factory))
            .ok_or(AgentError::FactoryNotRegistered)?;
        if state.live.contains_key(session_id) || state.starting.contains(session_id) {
            return Err(AgentError::Session(SessionError::DuplicateLive(
                session_id.clone(),
            )));
        }
        state.starting.insert(session_id.clone());
        Ok(factory)
    }

    fn unreserve(&self, session_id: &SessionId) {
        lock(&self.inner.state).starting.remove(session_id);
    }

    async fn finish_setup(
        &self,
        session_id: SessionId,
        session: Arc<Session>,
        options: AgentOptions,
        factory: Arc<dyn AgentFactory>,
        agent_mode: Option<AgentModeId>,
        setup_cancellation: CancellationToken,
    ) -> Result<AgentHandle, AgentError> {
        let inbox = Inbox::new();
        for message in session.pending_next_turn_inbox()? {
            inbox.send(message, InboxTarget::Followup, false)?;
        }
        let cancellation = ContextHandle::root().scope().cancellation();
        let runtime = match factory
            .create(
                Arc::clone(&session),
                options.clone(),
                inbox.clone(),
                cancellation.clone(),
            )
            .await
        {
            Ok(runtime) => runtime,
            Err(error) => {
                self.unreserve(&session_id);
                return Err(error);
            }
        };
        if setup_cancellation.is_cancelled() {
            let _ = runtime.dispose().await;
            self.unreserve(&session_id);
            return Err(AgentError::Cancelled);
        }

        let inner = Arc::new(AgentInner {
            id: session_id.clone(),
            agent_mode,
            session,
            options,
            inbox,
            cancellation,
            runtime,
            state: Mutex::new(AgentState::default()),
            dispose_gate: AsyncMutex::new(()),
            registry: Arc::downgrade(&self.inner),
            generation: AtomicU64::new(0),
        });
        let generation = {
            let mut state = lock(&self.inner.state);
            if !state.starting.remove(&session_id) || state.live.contains_key(&session_id) {
                None
            } else {
                let generation = next_generation(&mut state);
                inner.generation.store(generation, Ordering::Release);
                state.live.insert(
                    session_id,
                    LiveAgent {
                        generation,
                        inner: Arc::clone(&inner),
                    },
                );
                Some(generation)
            }
        };
        let Some(generation) = generation else {
            let _ = inner.dispose().await;
            return Err(AgentError::SetupReservationLost);
        };
        debug_assert_ne!(generation, 0);
        Ok(AgentHandle::own(inner))
    }

    fn live_inner(&self, session_id: &SessionId) -> Result<Arc<AgentInner>, AgentError> {
        lock(&self.inner.state)
            .live
            .get(session_id)
            .map(|live| Arc::clone(&live.inner))
            .ok_or_else(|| AgentError::NotFound(session_id.clone()))
    }
}

#[derive(Default)]
struct AgentState {
    cancellation: Option<AgentCancelOptions>,
    disposed: bool,
    cleanup_complete: bool,
}

struct AgentInner {
    id: SessionId,
    agent_mode: Option<AgentModeId>,
    session: Arc<Session>,
    options: AgentOptions,
    inbox: Inbox,
    cancellation: CancellationToken,
    runtime: Arc<dyn AgentRuntime>,
    state: Mutex<AgentState>,
    dispose_gate: AsyncMutex<()>,
    registry: Weak<RegistryInner>,
    generation: AtomicU64,
}

impl AgentInner {
    fn cancel(&self, options: AgentCancelOptions) -> bool {
        if self.runtime.status() == AgentStatus::Idle {
            return false;
        }
        self.cancel_including_idle(options)
    }

    fn cancel_including_idle(&self, options: AgentCancelOptions) -> bool {
        let cause = options.cause.clone();
        let clear_inbox = {
            let mut state = lock(&self.state);
            if state.disposed || state.cancellation.is_some() {
                return false;
            }
            let clear_inbox = !options.keep_inbox;
            state.cancellation = Some(options);
            clear_inbox
        };
        if clear_inbox {
            self.inbox.clear();
        }
        self.runtime.cancel(cause);
        self.cancellation.cancel()
    }

    async fn send(
        &self,
        message: Message,
        target: InboxTarget,
        wakeup: bool,
    ) -> Result<(), AgentError> {
        {
            let state = lock(&self.state);
            if state.disposed {
                return Err(AgentError::Disposed);
            }
            if state.cancellation.is_some() {
                return Err(AgentError::Cancelled);
            }
            self.inbox.send(message, target, wakeup)?;
        }
        if wakeup {
            self.runtime.wake().await?;
        }
        Ok(())
    }

    async fn update_inbox(
        &self,
        item_id: &MessageId,
        update: InboxUpdate,
    ) -> Result<InboxUpdateResult, AgentError> {
        let wakes = matches!(&update, InboxUpdate::Steer);
        let result = {
            let state = lock(&self.state);
            if state.disposed {
                return Err(AgentError::Disposed);
            }
            if state.cancellation.is_some() {
                return Err(AgentError::Cancelled);
            }
            if wakes && self.runtime.status() != AgentStatus::Running {
                return Ok(InboxUpdateResult::SteerUnavailable);
            }
            self.inbox.update(item_id, update)
        };
        if wakes && matches!(&result, InboxUpdateResult::Updated { .. }) {
            self.runtime.wake().await?;
        }
        Ok(result)
    }

    async fn reserve_inbox_update(
        &self,
        item_id: &MessageId,
        update: InboxUpdate,
    ) -> Result<InboxReservationResult, AgentError> {
        let state = lock(&self.state);
        if state.disposed {
            return Err(AgentError::Disposed);
        }
        if state.cancellation.is_some() {
            return Err(AgentError::Cancelled);
        }
        Ok(self.inbox.reserve_update(
            item_id,
            update,
            self.runtime.status() == AgentStatus::Running,
        ))
    }

    async fn commit_inbox_update(
        &self,
        reservation: InboxUpdateReservation,
    ) -> Result<bool, AgentError> {
        let wakes = reservation.commits_wake();
        let committed = reservation.commit();
        if committed && wakes {
            self.runtime.wake().await?;
        }
        Ok(committed)
    }

    async fn when_idle(&self) -> Result<(), AgentError> {
        loop {
            let observed_wake = self.inbox.wake_revision();
            self.runtime.when_idle().await?;
            if self.cancellation.is_cancelled() || lock(&self.state).disposed {
                return Ok(());
            }
            if self.inbox.wake_revision() == observed_wake {
                return Ok(());
            }
        }
    }

    async fn dispose(&self) -> Result<(), AgentError> {
        let _gate = self.dispose_gate.lock().await;
        let clear_inbox = {
            let mut state = lock(&self.state);
            if state.cleanup_complete {
                return Ok(());
            }
            if state.disposed {
                false
            } else {
                state.disposed = true;
                if state.cancellation.is_none() {
                    state.cancellation = Some(AgentCancelOptions {
                        cause: AgentCancelCause::Disposed,
                        keep_inbox: false,
                    });
                    true
                } else {
                    false
                }
            }
        };
        if clear_inbox {
            self.inbox.clear();
            self.runtime.cancel(AgentCancelCause::Disposed);
        }
        self.cancellation.cancel();
        self.runtime.dispose().await?;
        lock(&self.state).cleanup_complete = true;
        self.remove_from_registry();
        Ok(())
    }

    fn remove_from_registry(&self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let generation = self.generation.load(Ordering::Acquire);
        let mut state = lock(&registry.state);
        if state
            .live
            .get(&self.id)
            .is_some_and(|live| live.generation == generation)
        {
            state.live.remove(&self.id);
        }
    }
}

/// Opaque authority for one exact live agent generation.
#[derive(Clone)]
pub struct AgentAuthority {
    inner: Weak<AgentInner>,
    id: SessionId,
    generation: u64,
}
impl fmt::Debug for AgentAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentAuthority")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl AgentAuthority {
    /// Returns the attached agent's session identity for routing diagnostics.
    pub fn id(&self) -> SessionId {
        self.id.clone()
    }

    /// Returns whether this exact agent generation is still live and undisposed.
    pub fn is_live(&self) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        if inner.id != self.id || inner.generation.load(Ordering::Acquire) != self.generation {
            return false;
        }
        let Some(registry) = inner.registry.upgrade() else {
            return false;
        };
        let state = lock(&registry.state);
        state.live.get(&self.id).is_some_and(|live| {
            live.generation == self.generation
                && Arc::ptr_eq(&live.inner, &inner)
                && !lock(&inner.state).disposed
        })
    }
    pub(crate) fn same_authority(&self, other: &Self) -> bool {
        self.id == other.id
            && self.generation == other.generation
            && Weak::ptr_eq(&self.inner, &other.inner)
    }
}

pub(crate) fn same_authority(left: &AgentAuthority, right: &AgentAuthority) -> bool {
    left.same_authority(right)
}

/// A handle to one registry-owned agent. Creation handles cancel on Drop; query handles do not.
pub struct AgentHandle {
    inner: Arc<AgentInner>,
    drop_cancels: bool,
}

impl fmt::Debug for AgentHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentHandle")
            .field("id", &self.id())
            .field("status", &self.status())
            .field("disposed", &self.is_disposed())
            .finish()
    }
}

impl AgentHandle {
    fn own(inner: Arc<AgentInner>) -> Self {
        Self {
            inner,
            drop_cancels: true,
        }
    }

    fn observe(inner: Arc<AgentInner>) -> Self {
        Self {
            inner,
            drop_cancels: false,
        }
    }
    /// Derives an opaque capability for this exact live agent generation.
    pub fn authority(&self) -> AgentAuthority {
        AgentAuthority {
            inner: Arc::downgrade(&self.inner),
            id: self.id(),
            generation: self.inner.generation.load(Ordering::Acquire),
        }
    }

    pub fn id(&self) -> SessionId {
        self.inner.id.clone()
    }

    pub fn session(&self) -> Arc<Session> {
        Arc::clone(&self.inner.session)
    }

    pub fn agent_mode(&self) -> Option<AgentModeId> {
        self.inner.agent_mode.clone()
    }

    pub fn options(&self) -> AgentOptions {
        self.inner.options.clone()
    }

    pub fn inbox(&self) -> Inbox {
        self.inner.inbox.clone()
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.inner.cancellation.clone()
    }

    pub fn cancel_options(&self) -> Option<AgentCancelOptions> {
        lock(&self.inner.state).cancellation.clone()
    }

    pub fn status(&self) -> AgentStatus {
        self.inner.runtime.status()
    }

    pub fn is_disposed(&self) -> bool {
        lock(&self.inner.state).disposed
    }

    pub async fn send(
        &self,
        message: Message,
        target: InboxTarget,
        wakeup: bool,
    ) -> Result<(), AgentError> {
        self.inner.send(message, target, wakeup).await
    }

    pub async fn followup(&self, message: Message) -> Result<(), AgentError> {
        self.send(message, InboxTarget::Followup, true).await
    }

    pub async fn steer(&self, message: Message) -> Result<(), AgentError> {
        self.send(message, InboxTarget::Steer, true).await
    }

    pub async fn inject(&self, message: Message) -> Result<(), AgentError> {
        self.send(message, InboxTarget::Inject, false).await
    }

    /// Applies cancellation only if no earlier cancellation has won.
    pub fn cancel(&self, cause: AgentCancelCause, keep_inbox: bool) -> bool {
        self.inner.cancel(AgentCancelOptions { cause, keep_inbox })
    }

    pub(crate) fn cancel_including_idle(&self, cause: AgentCancelCause, keep_inbox: bool) -> bool {
        self.inner
            .cancel_including_idle(AgentCancelOptions { cause, keep_inbox })
    }

    /// Applies one id-addressed mutation to this agent's pending user inbox.
    pub async fn update_inbox(
        &self,
        item_id: &MessageId,
        update: InboxUpdate,
    ) -> Result<InboxUpdateResult, AgentError> {
        self.inner.update_inbox(item_id, update).await
    }

    /// Reserves one pending queue mutation until the caller durably records it.
    pub(crate) async fn reserve_inbox_update(
        &self,
        item_id: &MessageId,
        update: InboxUpdate,
    ) -> Result<InboxReservationResult, AgentError> {
        self.inner.reserve_inbox_update(item_id, update).await
    }

    /// Publishes a queue mutation after its durable journal append commits.
    pub(crate) async fn commit_inbox_update(
        &self,
        reservation: InboxUpdateReservation,
    ) -> Result<bool, AgentError> {
        self.inner.commit_inbox_update(reservation).await
    }

    /// Waits through replacement wakeups until the driver reaches quiescence.
    pub async fn when_idle(&self) -> Result<(), AgentError> {
        self.inner.when_idle().await
    }

    /// Stops the runtime and removes this generation only after cleanup succeeds; failures may be retried.
    pub async fn dispose(&self) -> Result<(), AgentError> {
        self.inner.dispose().await
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        if self.drop_cancels {
            self.inner.cancel(AgentCancelOptions {
                cause: AgentCancelCause::Disposed,
                keep_inbox: false,
            });
        }
    }
}

fn check_setup_cancellation(cancellation: &CancellationToken) -> Result<(), AgentError> {
    if cancellation.is_cancelled() {
        Err(AgentError::Cancelled)
    } else {
        Ok(())
    }
}

fn next_generation(state: &mut RegistryState) -> u64 {
    state.next_generation = state.next_generation.checked_add(1).unwrap_or(1);
    state.next_generation
}

fn bump(value: &mut u64) {
    *value = value.checked_add(1).unwrap_or(1);
}

#[derive(Clone, Copy)]
enum QueueRef {
    Followup(usize),
    Steer(usize),
}

impl QueueRef {
    fn message(self, state: &InboxState) -> &Message {
        match self {
            Self::Followup(index) => &state.followups[index],
            Self::Steer(index) => &state.steers[index],
        }
    }

    fn message_mut(self, state: &mut InboxState) -> &mut Message {
        match self {
            Self::Followup(index) => &mut state.followups[index],
            Self::Steer(index) => &mut state.steers[index],
        }
    }

    fn remove(self, state: &mut InboxState) -> Message {
        match self {
            Self::Followup(index) => state.followups.remove(index),
            Self::Steer(index) => state.steers.remove(index),
        }
        .expect("pending inbox index remains valid while the inbox lock is held")
    }
}

fn pending_contains(state: &InboxState, item_id: &MessageId) -> bool {
    pending_index(&state.followups, item_id).is_some()
        || pending_index(&state.steers, item_id).is_some()
        || pending_index(&state.injections, item_id).is_some()
}

fn pending_index(queue: &VecDeque<Message>, item_id: &MessageId) -> Option<usize> {
    queue.iter().position(|message| &message.id == item_id)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MessageRole, MessageSource};

    fn message(id: &str) -> Message {
        Message {
            id: MessageId::from(id),
            role: MessageRole::User,
            content: vec![ContentBlock::Text { text: id.into() }],
            source: MessageSource::User {
                client_time_zone: None,
            },
        }
    }

    #[test]
    fn steer_updates_use_combined_step_order_and_remove_entries() {
        let inbox = Inbox::new();
        let injection = message("injection");
        let steer = message("steer");
        let trailing_steer = message("trailing-steer");
        let edited = vec![ContentBlock::Text {
            text: "edited".into(),
        }];
        inbox.inject(injection.clone()).unwrap();
        inbox.steer(steer.clone()).unwrap();
        inbox.steer(trailing_steer.clone()).unwrap();

        let edit = match inbox.reserve_update(
            &steer.id,
            InboxUpdate::Edit {
                content: edited.clone(),
            },
            true,
        ) {
            InboxReservationResult::Reserved(reservation) => reservation,
            _ => panic!("steer remains pending"),
        };
        assert_eq!(edit.start(), 1);
        assert!(edit.commit());
        let pending = inbox.pending();
        assert_eq!(
            pending
                .iter()
                .map(|(target, message)| (*target, message.id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (InboxTarget::Inject, "injection"),
                (InboxTarget::Steer, "steer"),
                (InboxTarget::Steer, "trailing-steer"),
            ]
        );
        assert_eq!(pending[1].1.content, edited);

        let remove = match inbox.reserve_update(&steer.id, InboxUpdate::Remove, true) {
            InboxReservationResult::Reserved(reservation) => reservation,
            _ => panic!("edited steer remains pending"),
        };
        assert_eq!(remove.start(), 1);
        assert!(remove.commit());
        assert_eq!(
            lock(&inbox.inner.state)
                .step_order
                .iter()
                .map(|(target, item_id)| (*target, item_id.as_str().to_owned()))
                .collect::<Vec<_>>(),
            vec![
                (InboxTarget::Inject, "injection".to_owned()),
                (InboxTarget::Steer, "trailing-steer".to_owned()),
            ]
        );
        assert_eq!(
            inbox
                .pending()
                .iter()
                .map(|(target, message)| (*target, message.id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (InboxTarget::Inject, "injection"),
                (InboxTarget::Steer, "trailing-steer"),
            ]
        );
    }
}

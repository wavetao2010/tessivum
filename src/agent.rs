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
    protocol::{Message, SessionHeader, SessionId},
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

/// Model route options owned by an agent, never a separate session identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOptions {
    pub provider: String,
    pub model: String,
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
    #[error("an agent is already live or starting for session {0}")]
    DuplicateLive(SessionId),
    #[error("agent is not live for session {0}")]
    NotFound(SessionId),
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
        match target {
            InboxTarget::Followup => state.followups.push_back(message),
            InboxTarget::Steer => state.steers.push_back(message),
            InboxTarget::Inject => state.injections.push_back(message),
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

    /// Removes the next message for `target`, preserving that target's FIFO order.
    pub fn take(&self, target: InboxTarget) -> Option<Message> {
        let mut state = lock(&self.inner.state);
        match target {
            InboxTarget::Followup => state.followups.pop_front(),
            InboxTarget::Steer => state.steers.pop_front(),
            InboxTarget::Inject => state.injections.pop_front(),
        }
    }

    pub fn take_pre_step(&self) -> Option<Message> {
        self.take(InboxTarget::Inject)
    }

    pub fn take_next_step(&self) -> Option<Message> {
        self.take(InboxTarget::Steer)
    }

    pub fn take_next_turn(&self) -> Option<Message> {
        self.take(InboxTarget::Followup)
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
        self.finish_setup(id, session, options, factory, setup_cancellation)
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
        self.finish_setup(session_id, session, options, factory, setup_cancellation)
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
        self.finish_setup(id, session, options, factory, setup_cancellation)
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
            return Err(AgentError::DuplicateLive(session_id.clone()));
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
        setup_cancellation: CancellationToken,
    ) -> Result<AgentHandle, AgentError> {
        let inbox = Inbox::new();
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
}

struct AgentInner {
    id: SessionId,
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
            if state.disposed {
                return Ok(());
            }
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
        };
        if clear_inbox {
            self.inbox.clear();
        }
        self.cancellation.cancel();
        let result = self.runtime.dispose().await;
        self.remove_from_registry();
        result
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

    /// Waits through replacement wakeups until the driver reaches quiescence.
    pub async fn when_idle(&self) -> Result<(), AgentError> {
        self.inner.when_idle().await
    }

    /// Explicitly stops the runtime once and removes only this registry generation.
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

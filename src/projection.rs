//! Synchronous JSON projections over durable session event streams.
//!
//! Projection callbacks are pure functions over owned JSON values. Their work is
//! deliberately outside registry locks; a callback cannot block event admission.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    sync::{broadcast, watch},
    task::JoinHandle,
};

use crate::{
    protocol::{SessionEvent, SessionHeader, SessionId},
    session::{Session, SessionStore},
};

/// Stable failure from projection registration, replay, or restoration.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum ProjectionError {
    #[error("projection key must not be empty")]
    EmptyKey,
    #[error("projection is already registered: {0}")]
    AlreadyRegistered(String),
    #[error("projection is not registered: {0}")]
    NotRegistered(String),
    #[error("projection is not attached to session {session_id}: {key}")]
    NotAttached { session_id: SessionId, key: String },
    #[error("projection callback failed: {0}")]
    Callback(String),
    #[error("projection checkpoint has an unusable nonzero base; a full reread is required")]
    FullRereadRequired {
        session_id: SessionId,
        key: String,
        base_seq: u64,
    },
    #[error("projection registry has shut down")]
    Shutdown,
}

impl ProjectionError {
    /// Stable code suitable for host boundaries.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyKey => "INVALID_PROJECTION_KEY",
            Self::AlreadyRegistered(_) => "PROJECTION_ALREADY_REGISTERED",
            Self::NotRegistered(_) => "PROJECTION_NOT_REGISTERED",
            Self::NotAttached { .. } => "PROJECTION_NOT_ATTACHED",
            Self::Callback(_) => "PROJECTION_CALLBACK_FAILED",
            Self::FullRereadRequired { .. } => "PROJECTION_FULL_REREAD_REQUIRED",
            Self::Shutdown => "PROJECTION_SHUTDOWN",
        }
    }
}

type Init = dyn Fn(&SessionHeader) -> Result<Value, ProjectionError> + Send + Sync;
type Apply = dyn Fn(&Value, &SessionEvent) -> Result<Value, ProjectionError> + Send + Sync;
type View = dyn Fn(&Value) -> Result<Value, ProjectionError> + Send + Sync;

/// A named, versioned, pure JSON projection.
#[derive(Clone)]
pub struct ProjectionDefinition {
    pub key: String,
    pub state_version: u64,
    init: Arc<Init>,
    apply: Arc<Apply>,
    view: Arc<View>,
}

impl std::fmt::Debug for ProjectionDefinition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectionDefinition")
            .field("key", &self.key)
            .field("state_version", &self.state_version)
            .finish_non_exhaustive()
    }
}

impl ProjectionDefinition {
    /// Builds a projection from pure callbacks.
    pub fn new<I, A, V>(
        key: impl Into<String>,
        state_version: u64,
        init: I,
        apply: A,
        view: V,
    ) -> Self
    where
        I: Fn(&SessionHeader) -> Result<Value, ProjectionError> + Send + Sync + 'static,
        A: Fn(&Value, &SessionEvent) -> Result<Value, ProjectionError> + Send + Sync + 'static,
        V: Fn(&Value) -> Result<Value, ProjectionError> + Send + Sync + 'static,
    {
        Self {
            key: key.into(),
            state_version,
            init: Arc::new(init),
            apply: Arc::new(apply),
            view: Arc::new(view),
        }
    }
}

/// Serializable recovery point for one session projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionCheckpoint {
    pub session_id: SessionId,
    pub key: String,
    pub state_version: u64,
    /// The last event included in `state`; `None` is the initialized state.
    pub as_of_seq: Option<u64>,
    pub state: Value,
}

/// Immutable projection result at a concrete log point.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectionSnapshot {
    pub session_id: SessionId,
    pub key: String,
    pub state_version: u64,
    pub as_of_seq: Option<u64>,
    pub state: Value,
    pub view: Value,
}

/// Registry owning synchronous projection state and eager session subscriptions.
#[derive(Clone)]
pub struct ProjectionRegistry {
    inner: Arc<Inner>,
}

struct Inner {
    definitions: Mutex<BTreeMap<String, ProjectionDefinition>>,
    sessions: Mutex<BTreeMap<SessionId, AttachedSession>>,
    subscriptions: Mutex<Vec<JoinHandle<()>>>,
    shutdown: watch::Sender<bool>,
}

struct AttachedSession {
    header: SessionHeader,
    projections: BTreeMap<String, ProjectionState>,
}

struct ProjectionState {
    state_version: u64,
    state: Value,
    last_seq: Option<u64>,
    /// Frames exist only for observable state changes. An equal JSON result still
    /// advances `last_seq`, without allocating a duplicate state snapshot.
    frames: Vec<StateFrame>,
    last_error: Option<ProjectionError>,
}

#[derive(Clone)]
struct StateFrame {
    as_of_seq: Option<u64>,
    state: Value,
}

impl ProjectionRegistry {
    pub fn new() -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                definitions: Mutex::new(BTreeMap::new()),
                sessions: Mutex::new(BTreeMap::new()),
                subscriptions: Mutex::new(Vec::new()),
                shutdown,
            }),
        }
    }

    /// Registers a definition before attaching sessions.
    pub fn register(&self, definition: ProjectionDefinition) -> Result<(), ProjectionError> {
        if definition.key.trim().is_empty() {
            return Err(ProjectionError::EmptyKey);
        }
        let mut definitions = lock(&self.inner.definitions);
        if definitions.contains_key(&definition.key) {
            return Err(ProjectionError::AlreadyRegistered(definition.key));
        }
        definitions.insert(definition.key.clone(), definition);
        Ok(())
    }

    /// Eagerly attaches every session currently live in `store`.
    pub fn attach_store(&self, store: &SessionStore) -> Result<(), ProjectionError> {
        for session in store.list() {
            self.attach(session)?;
        }
        Ok(())
    }

    /// Builds the current state from the committed prefix and eagerly observes later appends.
    pub fn attach(&self, session: Arc<Session>) -> Result<(), ProjectionError> {
        if *self.inner.shutdown.borrow() {
            return Err(ProjectionError::Shutdown);
        }
        let header = session.header();
        let id = header.id.clone();
        let definitions = lock(&self.inner.definitions).clone();
        let mut projections = BTreeMap::new();
        for definition in definitions.values() {
            let initial = (definition.init)(&header)?;
            projections.insert(
                definition.key.clone(),
                ProjectionState {
                    state_version: definition.state_version,
                    state: initial.clone(),
                    last_seq: None,
                    frames: vec![StateFrame {
                        as_of_seq: None,
                        state: initial,
                    }],
                    last_error: None,
                },
            );
        }

        // Subscribe before taking the event snapshot. Events admitted in between
        // are either in the snapshot or delivered by the receiver; sequence checks
        // discard the duplicate case.
        let receiver = session.subscribe();
        {
            let mut sessions = lock(&self.inner.sessions);
            sessions.insert(
                id.clone(),
                AttachedSession {
                    header,
                    projections,
                },
            );
        }
        self.replay(&session)?;

        let inner = Arc::clone(&self.inner);
        let mut shutdown = inner.shutdown.subscribe();
        let task = tokio::spawn(async move {
            observe_session(inner, session, receiver, &mut shutdown).await;
        });
        lock(&self.inner.subscriptions).push(task);
        Ok(())
    }

    /// Detaches workers and waits for all observer tasks to exit.
    pub async fn shutdown(&self) {
        let _ = self.inner.shutdown.send(true);
        let tasks = std::mem::take(&mut *lock(&self.inner.subscriptions));
        for task in tasks {
            let _ = task.await;
        }
    }

    /// Alias for [`Self::shutdown`], for drain-oriented host shutdown code.
    pub async fn drain(&self) {
        self.shutdown().await;
    }

    /// Returns the latest snapshot and view for one projection.
    pub fn snapshot(
        &self,
        session_id: &SessionId,
        key: &str,
    ) -> Result<ProjectionSnapshot, ProjectionError> {
        self.snapshot_as_of(session_id, key, None)
    }

    /// Returns the state after the greatest committed event at or before `as_of_seq`.
    pub fn as_of_seq(
        &self,
        session_id: &SessionId,
        key: &str,
        as_of_seq: u64,
    ) -> Result<ProjectionSnapshot, ProjectionError> {
        self.snapshot_as_of(session_id, key, Some(as_of_seq))
    }

    /// Captures a JSON-only durable checkpoint.
    pub fn checkpoint(
        &self,
        session_id: &SessionId,
        key: &str,
    ) -> Result<ProjectionCheckpoint, ProjectionError> {
        let snapshot = self.snapshot(session_id, key)?;
        Ok(ProjectionCheckpoint {
            session_id: snapshot.session_id,
            key: snapshot.key,
            state_version: snapshot.state_version,
            as_of_seq: snapshot.as_of_seq,
            state: snapshot.state,
        })
    }

    /// Restores a compatible checkpoint, then replays the remaining committed events.
    pub fn restore(
        &self,
        session: &Session,
        checkpoint: ProjectionCheckpoint,
    ) -> Result<(), ProjectionError> {
        let base = checkpoint.as_of_seq.map_or(0, |seq| seq.saturating_add(1));
        self.restore_floor(session, checkpoint, base)
    }

    /// Restores from `checkpoint` at a known retained-log floor.
    ///
    /// An absent or version-incompatible checkpoint is only safe when the retained
    /// floor is zero: then init plus a full reread can recreate state. At any
    /// nonzero floor, history is unavailable and the exact failure is surfaced.
    pub fn restore_floor(
        &self,
        session: &Session,
        checkpoint: ProjectionCheckpoint,
        floor: u64,
    ) -> Result<(), ProjectionError> {
        let key = checkpoint.key.clone();
        let definition = definition(&self.inner, &key)?;
        if checkpoint.session_id != session.id()
            || checkpoint.state_version != definition.state_version
        {
            if floor != 0 {
                return Err(ProjectionError::FullRereadRequired {
                    session_id: session.id(),
                    key,
                    base_seq: floor,
                });
            }
            self.reset_projection(session, &definition)?;
            return self.replay(session);
        }
        let checkpoint_next = checkpoint.as_of_seq.map_or(0, |seq| seq.saturating_add(1));
        if checkpoint_next < floor {
            return Err(ProjectionError::FullRereadRequired {
                session_id: session.id(),
                key,
                base_seq: floor,
            });
        }
        {
            let mut sessions = lock(&self.inner.sessions);
            let attached =
                sessions
                    .get_mut(&session.id())
                    .ok_or_else(|| ProjectionError::NotAttached {
                        session_id: session.id(),
                        key: definition.key.clone(),
                    })?;
            attached.projections.insert(
                definition.key.clone(),
                ProjectionState {
                    state_version: definition.state_version,
                    state: checkpoint.state.clone(),
                    last_seq: checkpoint.as_of_seq,
                    frames: vec![StateFrame {
                        as_of_seq: checkpoint.as_of_seq,
                        state: checkpoint.state,
                    }],
                    last_error: None,
                },
            );
        }
        self.replay(session)
    }

    /// Returns a contained callback failure, if an asynchronous observer encountered one.
    pub fn last_error(
        &self,
        session_id: &SessionId,
        key: &str,
    ) -> Result<Option<ProjectionError>, ProjectionError> {
        let sessions = lock(&self.inner.sessions);
        let projection = sessions
            .get(session_id)
            .and_then(|attached| attached.projections.get(key))
            .ok_or_else(|| ProjectionError::NotAttached {
                session_id: session_id.clone(),
                key: key.to_owned(),
            })?;
        Ok(projection.last_error.clone())
    }

    fn replay(&self, session: &Session) -> Result<(), ProjectionError> {
        for event in session.events() {
            self.apply_event(&session.id(), &event)?;
        }
        Ok(())
    }

    fn reset_projection(
        &self,
        session: &Session,
        definition: &ProjectionDefinition,
    ) -> Result<(), ProjectionError> {
        let initial = (definition.init)(&session.header())?;
        let mut sessions = lock(&self.inner.sessions);
        let attached =
            sessions
                .get_mut(&session.id())
                .ok_or_else(|| ProjectionError::NotAttached {
                    session_id: session.id(),
                    key: definition.key.clone(),
                })?;
        attached.projections.insert(
            definition.key.clone(),
            ProjectionState {
                state_version: definition.state_version,
                state: initial.clone(),
                last_seq: None,
                frames: vec![StateFrame {
                    as_of_seq: None,
                    state: initial,
                }],
                last_error: None,
            },
        );
        Ok(())
    }

    fn apply_event(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
    ) -> Result<(), ProjectionError> {
        let work = {
            let sessions = lock(&self.inner.sessions);
            let attached =
                sessions
                    .get(session_id)
                    .ok_or_else(|| ProjectionError::NotAttached {
                        session_id: session_id.clone(),
                        key: "*".into(),
                    })?;
            lock(&self.inner.definitions)
                .iter()
                .filter_map(|(key, definition)| {
                    let state = attached.projections.get(key)?;
                    (state.last_seq.is_none_or(|seq| event.seq > seq))
                        .then(|| (definition.clone(), state.state.clone()))
                })
                .collect::<Vec<_>>()
        };
        for (definition, current) in work {
            let next = (definition.apply)(&current, event)?;
            let mut sessions = lock(&self.inner.sessions);
            let attached =
                sessions
                    .get_mut(session_id)
                    .ok_or_else(|| ProjectionError::NotAttached {
                        session_id: session_id.clone(),
                        key: definition.key.clone(),
                    })?;
            let state = attached
                .projections
                .get_mut(&definition.key)
                .ok_or_else(|| ProjectionError::NotAttached {
                    session_id: session_id.clone(),
                    key: definition.key.clone(),
                })?;
            if state.last_seq.is_some_and(|seq| event.seq <= seq) {
                continue;
            }
            state.last_seq = Some(event.seq);
            if state.state != next {
                state.state = next.clone();
                state.frames.push(StateFrame {
                    as_of_seq: Some(event.seq),
                    state: next,
                });
            }
        }
        Ok(())
    }

    fn snapshot_as_of(
        &self,
        session_id: &SessionId,
        key: &str,
        as_of: Option<u64>,
    ) -> Result<ProjectionSnapshot, ProjectionError> {
        let (header, state_version, as_of_seq, state, definition) = {
            let sessions = lock(&self.inner.sessions);
            let attached =
                sessions
                    .get(session_id)
                    .ok_or_else(|| ProjectionError::NotAttached {
                        session_id: session_id.clone(),
                        key: key.to_owned(),
                    })?;
            let projection =
                attached
                    .projections
                    .get(key)
                    .ok_or_else(|| ProjectionError::NotAttached {
                        session_id: session_id.clone(),
                        key: key.to_owned(),
                    })?;
            let frame = match as_of {
                None => projection.frames.last(),
                Some(seq) => projection
                    .frames
                    .iter()
                    .rev()
                    .find(|frame| frame.as_of_seq.is_none_or(|at| at <= seq)),
            }
            .expect("initialized projections always retain an initial frame");
            (
                attached.header.clone(),
                projection.state_version,
                frame.as_of_seq,
                frame.state.clone(),
                definition(&self.inner, key)?,
            )
        };
        let view = (definition.view)(&state)?;
        Ok(ProjectionSnapshot {
            session_id: header.id,
            key: key.to_owned(),
            state_version,
            as_of_seq,
            state,
            view,
        })
    }
}

impl Default for ProjectionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

async fn observe_session(
    inner: Arc<Inner>,
    session: Arc<Session>,
    mut receiver: broadcast::Receiver<SessionEvent>,
    shutdown: &mut watch::Receiver<bool>,
) {
    let registry = ProjectionRegistry {
        inner: Arc::clone(&inner),
    };
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            event = receiver.recv() => match event {
                Ok(event) => {
                    if let Err(error) = registry.apply_event(&session.id(), &event) {
                        record_error(&inner, &session.id(), error);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if let Err(error) = registry.replay(&session) {
                        record_error(&inner, &session.id(), error);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}

fn record_error(inner: &Inner, session_id: &SessionId, error: ProjectionError) {
    let mut sessions = lock(&inner.sessions);
    if let Some(attached) = sessions.get_mut(session_id) {
        for state in attached.projections.values_mut() {
            state.last_error = Some(error.clone());
        }
    }
}

fn definition(inner: &Inner, key: &str) -> Result<ProjectionDefinition, ProjectionError> {
    lock(&inner.definitions)
        .get(key)
        .cloned()
        .ok_or_else(|| ProjectionError::NotRegistered(key.to_owned()))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

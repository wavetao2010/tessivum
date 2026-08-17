//! Durable, in-memory session history and its persistence boundary.
//!
//! A [`Session`] never exposes mutable references to admitted events. Writes are
//! serialized through one async gate, persisted first, then made visible to
//! readers and subscribers.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use async_trait::async_trait;
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ServiceKey};
use thiserror::Error;
use tokio::sync::{broadcast, Mutex as AsyncMutex};

use crate::{
    error::TessivumError,
    protocol::{Message, MessageRole, SessionEvent, SessionHeader, SessionId, SurfaceOp},
};

/// Stable key for the native session service.
pub fn session_service_key() -> ServiceKey {
    ServiceKey::new("harness.sessions", "1")
}

/// Whether restoration is resuming a live process or recovering a cold one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreMode {
    /// A running host is resuming the exact committed log and must not repair it.
    Live,
    /// A stopped host may close one otherwise orphaned turn with a synthetic event.
    Cold,
}

/// One derived, model-visible message with its durable provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceMessage {
    /// Sequence number of the message-producing event.
    pub event_seq: u64,
    /// Immutable model-visible message decoded from the event payload.
    pub message: Message,
    /// The event sequences cited by the producing event, preserving absent versus empty.
    pub source_event_seqs: Option<Vec<u64>>,
}

/// Persistent session metadata without replaying its log.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInspection {
    pub header: SessionHeader,
    pub event_count: u64,
    pub next_seq: u64,
    pub flush_count: u64,
}

/// Failures at the session and persistence boundary.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum SessionError {
    #[error(transparent)]
    Protocol(#[from] TessivumError),
    #[error("session operation was cancelled")]
    Cancelled,
    #[error("session does not exist: {0}")]
    NotFound(SessionId),
    #[error("session is already live: {0}")]
    DuplicateLive(SessionId),
    #[error("session already exists: {0}")]
    AlreadyExists(SessionId),
    #[error("event sequence is not contiguous: expected {expected}, got {actual}")]
    SequenceGap { expected: u64, actual: u64 },
    #[error("session event sequence space is exhausted")]
    SequenceExhausted,
    #[error("session header identity does not match the requested session")]
    HeaderIdMismatch,
    #[error("session id must not be empty")]
    EmptySessionId,
    #[error("seed length {seed_length} exceeds {event_count} committed events")]
    InvalidSeedLength { seed_length: u64, event_count: u64 },
    #[error("session/end-seed does not match the declared seed boundary")]
    InvalidSeedBoundary,
    #[error("session has more than one seed boundary")]
    DuplicateSeedBoundary,
    #[error("surface changed before conditional append")]
    StaleSurface {
        expected: Vec<u64>,
        actual: Vec<u64>,
    },
    #[error("surface replacement range [{start}, {end}) is outside a surface of length {len}")]
    SurfaceRange { start: u64, end: u64, len: u64 },
    #[error("surface source event sequence {source_seq} is not committed")]
    MissingSourceEvent { source_seq: u64 },
    #[error("surface event payload does not contain a valid message")]
    InvalidSurfaceMessage,
    #[error("surface event role does not match its event type")]
    InvalidSurfaceRole,
    #[error("turn lifecycle event is missing a numeric turn id")]
    InvalidTurnId,
    #[error("turn lifecycle is inconsistent")]
    InvalidTurnLifecycle,
    #[error("a live restoration cannot resume an orphaned turn")]
    OrphanTurn,
    #[error("a non-empty seed must be created through create_seeded")]
    SeedRequiresEvents,
}

impl SessionError {
    /// A stable diagnostic code suitable for service and host boundaries.
    pub fn code(&self) -> &str {
        match self {
            Self::Protocol(error) => &error.code,
            Self::Cancelled => "CANCELLED",
            Self::NotFound(_) => "SESSION_NOT_FOUND",
            Self::DuplicateLive(_) => "DUPLICATE_LIVE_SESSION",
            Self::AlreadyExists(_) => "SESSION_ALREADY_EXISTS",
            Self::SequenceGap { .. } => "NON_CONTIGUOUS_EVENT_SEQUENCE",
            Self::SequenceExhausted => "SESSION_SEQUENCE_EXHAUSTED",
            Self::HeaderIdMismatch => "SESSION_HEADER_ID_MISMATCH",
            Self::EmptySessionId => "INVALID_SESSION_ID",
            Self::InvalidSeedLength { .. } => "INVALID_SEED_LENGTH",
            Self::InvalidSeedBoundary => "INVALID_SEED_BOUNDARY",
            Self::DuplicateSeedBoundary => "DUPLICATE_SEED_BOUNDARY",
            Self::StaleSurface { .. } => "STALE_SESSION_SURFACE",
            Self::SurfaceRange { .. } => "INVALID_SURFACE_RANGE",
            Self::MissingSourceEvent { .. } => "MISSING_SOURCE_EVENT",
            Self::InvalidSurfaceMessage => "INVALID_SURFACE_MESSAGE",
            Self::InvalidSurfaceRole => "INVALID_SURFACE_ROLE",
            Self::InvalidTurnId => "INVALID_TURN_ID",
            Self::InvalidTurnLifecycle => "INVALID_TURN_LIFECYCLE",
            Self::OrphanTurn => "ORPHAN_TURN",
            Self::SeedRequiresEvents => "SEED_REQUIRES_EVENTS",
        }
    }
}

/// The durable storage contract behind sessions.
///
/// Every method receives the core cancellation primitive. Implementations must
/// leave a committed prefix intact when an operation fails or is cancelled.
#[async_trait]
pub trait SessionPersistence: Send + Sync {
    async fn create(
        &self,
        header: &SessionHeader,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError>;

    async fn append(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError>;

    async fn load(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionHeader>, SessionError>;

    async fn inspect(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionInspection>, SessionError>;

    /// Reads all committed events whose sequence is at least `from_seq`.
    async fn read_from(
        &self,
        session_id: &SessionId,
        from_seq: u64,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionEvent>, SessionError>;

    async fn flush(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError>;

    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SessionError>;
}

#[derive(Clone, Default)]
pub struct MemorySessionPersistence {
    sessions: Arc<Mutex<BTreeMap<SessionId, MemorySession>>>,
}

#[derive(Clone)]
struct MemorySession {
    header: SessionHeader,
    events: Vec<SessionEvent>,
    flush_count: u64,
}

impl MemorySessionPersistence {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionPersistence for MemorySessionPersistence {
    async fn create(
        &self,
        header: &SessionHeader,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        validate_header(header)?;
        let mut sessions = lock(&self.sessions);
        if sessions.contains_key(&header.id) {
            return Err(SessionError::AlreadyExists(header.id.clone()));
        }
        sessions.insert(
            header.id.clone(),
            MemorySession {
                header: header.clone(),
                events: Vec::new(),
                flush_count: 0,
            },
        );
        Ok(())
    }

    async fn append(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        event.validate()?;
        let mut sessions = lock(&self.sessions);
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))?;
        let expected = next_seq(&session.events)?;
        if event.seq != expected {
            return Err(SessionError::SequenceGap {
                expected,
                actual: event.seq,
            });
        }
        session.events.push(event.clone());
        Ok(())
    }

    async fn load(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionHeader>, SessionError> {
        check_cancellation(&cancellation)?;
        Ok(lock(&self.sessions)
            .get(session_id)
            .map(|session| session.header.clone()))
    }

    async fn inspect(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionInspection>, SessionError> {
        check_cancellation(&cancellation)?;
        Ok(lock(&self.sessions)
            .get(session_id)
            .map(inspect_memory_session)
            .transpose()?)
    }

    async fn read_from(
        &self,
        session_id: &SessionId,
        from_seq: u64,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        check_cancellation(&cancellation)?;
        let sessions = lock(&self.sessions);
        let session = sessions
            .get(session_id)
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))?;
        Ok(session
            .events
            .iter()
            .filter(|event| event.seq >= from_seq)
            .cloned()
            .collect())
    }

    async fn flush(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        let mut sessions = lock(&self.sessions);
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))?;
        session.flush_count = session
            .flush_count
            .checked_add(1)
            .ok_or(SessionError::SequenceExhausted)?;
        Ok(())
    }

    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SessionError> {
        check_cancellation(&cancellation)?;
        lock(&self.sessions)
            .values()
            .map(inspect_memory_session)
            .collect()
    }
}

/// An in-memory projection of one durable session log.
pub struct Session {
    header: SessionHeader,
    persistence: Arc<dyn SessionPersistence>,
    state: RwLock<SessionState>,
    write_gate: AsyncMutex<()>,
    updates: broadcast::Sender<SessionEvent>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Session")
            .field("header", &self.header)
            .field("event_count", &read_lock(&self.state).events.len())
            .finish_non_exhaustive()
    }
}

struct SessionState {
    events: Vec<SessionEvent>,
    surface: Vec<SurfaceMessage>,
    seed_length: usize,
    end_seed_seen: bool,
}

impl Session {
    fn from_committed(
        header: SessionHeader,
        events: Vec<SessionEvent>,
        persistence: Arc<dyn SessionPersistence>,
    ) -> Result<Self, SessionError> {
        let state = build_state(&header, events)?;
        let (updates, _) = broadcast::channel(128);
        Ok(Self {
            header,
            persistence,
            state: RwLock::new(state),
            write_gate: AsyncMutex::new(()),
            updates,
        })
    }

    /// Returns a clone so the admitted header stays immutable.
    pub fn header(&self) -> SessionHeader {
        self.header.clone()
    }

    pub fn id(&self) -> SessionId {
        self.header.id.clone()
    }

    /// Returns immutable snapshots of every admitted event, including seed events.
    pub fn events(&self) -> Vec<SessionEvent> {
        read_lock(&self.state).events.clone()
    }

    /// Returns the immutable seed prefix declared in the header.
    pub fn seed_events(&self) -> Vec<SessionEvent> {
        let state = read_lock(&self.state);
        state.events[..state.seed_length].to_vec()
    }

    /// Returns all events after the immutable seed prefix.
    pub fn live_events(&self) -> Vec<SessionEvent> {
        let state = read_lock(&self.state);
        state.events[state.seed_length..].to_vec()
    }

    /// Returns the current derived surface, including durable source sequences.
    pub fn surface(&self) -> Vec<SurfaceMessage> {
        read_lock(&self.state).surface.clone()
    }

    /// Returns model messages in exact surface order.
    pub fn derive_messages(&self) -> Vec<Message> {
        read_lock(&self.state)
            .surface
            .iter()
            .map(|entry| entry.message.clone())
            .collect()
    }

    /// Returns the only sequence number admissible for the next append.
    pub fn next_seq(&self) -> Result<u64, SessionError> {
        next_seq(&read_lock(&self.state).events)
    }

    /// Subscribes to events admitted after this call.
    ///
    /// Seed replay is intentionally explicit through [`Self::seed_events`]; a
    /// receiver observes only live admission and owns its registration by Drop.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.updates.subscribe()
    }

    /// Persists and then atomically admits one new event.
    pub async fn append(
        &self,
        event: SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.append_inner(event, None, cancellation).await
    }

    /// Persists and atomically admits one new event only if the complete
    /// current surface still has exactly these event sequence numbers.
    pub async fn append_if_surface(
        &self,
        event: SessionEvent,
        expected_surface_event_seqs: &[u64],
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.append_inner(event, Some(expected_surface_event_seqs), cancellation)
            .await
    }

    async fn append_inner(
        &self,
        event: SessionEvent,
        expected_surface_event_seqs: Option<&[u64]>,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        let _gate = self.write_gate.lock().await;
        check_cancellation(&cancellation)?;

        let projection = {
            let state = read_lock(&self.state);
            if let Some(expected) = expected_surface_event_seqs {
                if !state
                    .surface
                    .iter()
                    .map(|entry| entry.event_seq)
                    .eq(expected.iter().copied())
                {
                    return Err(SessionError::StaleSurface {
                        expected: expected.to_vec(),
                        actual: state.surface.iter().map(|entry| entry.event_seq).collect(),
                    });
                }
            }
            event.validate()?;
            let expected = next_seq(&state.events)?;
            if event.seq != expected {
                return Err(SessionError::SequenceGap {
                    expected,
                    actual: event.seq,
                });
            }
            validate_seed_boundary(&self.header, &state, &event)?;
            validate_sources(&event, &state.events)?;
            let projection = decode_surface_event(&event)?;
            validate_surface_operation(&event, projection.as_ref(), &state.surface)?;
            projection
        };

        self.persistence
            .append(&self.header.id, &event, cancellation.clone())
            .await?;
        // The persistence append is the commit point. Cancellation observed
        // afterwards cannot roll it back, so the committed event must be admitted.

        {
            let mut state = write_lock(&self.state);
            // The async gate guarantees that the pre-persistence snapshot is still current.
            if event.event_type == "session/end-seed" {
                state.end_seed_seen = true;
            }
            apply_surface_operation(&event, projection, &mut state.surface);
            state.events.push(event.clone());
        }
        let _ = self.updates.send(event);
        Ok(())
    }

    /// Delegates an explicit durability boundary to the persistence implementation.
    pub async fn flush(&self, cancellation: CancellationToken) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        let _gate = self.write_gate.lock().await;
        self.persistence.flush(&self.header.id, cancellation).await
    }
}

/// A service-owned collection of currently live session instances.
#[derive(Clone)]
pub struct SessionStore {
    persistence: Arc<dyn SessionPersistence>,
    live: Arc<Mutex<BTreeMap<SessionId, Arc<Session>>>>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionStore")
            .field("live_count", &lock(&self.live).len())
            .finish_non_exhaustive()
    }
}

impl SessionStore {
    pub fn new(persistence: Arc<dyn SessionPersistence>) -> Self {
        Self {
            persistence,
            live: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Creates a new empty, live session. Non-empty seeds use [`Self::create_seeded`].
    pub async fn create(
        &self,
        header: SessionHeader,
        cancellation: CancellationToken,
    ) -> Result<Arc<Session>, SessionError> {
        check_cancellation(&cancellation)?;
        validate_header(&header)?;
        if header.seed_length.unwrap_or_default() != 0 {
            return Err(SessionError::SeedRequiresEvents);
        }
        self.ensure_not_live(&header.id)?;
        self.persistence
            .create(&header, cancellation.clone())
            .await?;
        let session = Arc::new(Session::from_committed(
            header.clone(),
            Vec::new(),
            Arc::clone(&self.persistence),
        )?);
        self.insert_live(header.id, Arc::clone(&session))?;
        Ok(session)
    }

    /// Creates a session with an already-validated immutable seed prefix.
    pub async fn create_seeded(
        &self,
        header: SessionHeader,
        seed_events: Vec<SessionEvent>,
        cancellation: CancellationToken,
    ) -> Result<Arc<Session>, SessionError> {
        check_cancellation(&cancellation)?;
        validate_header(&header)?;
        if header.seed_length != Some(seed_events.len() as u64) {
            return Err(SessionError::InvalidSeedLength {
                seed_length: header.seed_length.unwrap_or_default(),
                event_count: seed_events.len() as u64,
            });
        }
        let session = Session::from_committed(
            header.clone(),
            seed_events.clone(),
            Arc::clone(&self.persistence),
        )?;
        self.ensure_not_live(&header.id)?;
        self.persistence
            .create(&header, cancellation.clone())
            .await?;
        for event in &seed_events {
            self.persistence
                .append(&header.id, event, cancellation.clone())
                .await?;
        }
        let session = Arc::new(session);
        self.insert_live(header.id, Arc::clone(&session))?;
        Ok(session)
    }

    /// Restores a durable session, optionally closing a single orphaned turn while cold.
    pub async fn restore(
        &self,
        session_id: &SessionId,
        mode: RestoreMode,
        cancellation: CancellationToken,
    ) -> Result<Arc<Session>, SessionError> {
        check_cancellation(&cancellation)?;
        self.ensure_not_live(session_id)?;
        let header = self
            .persistence
            .load(session_id, cancellation.clone())
            .await?
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))?;
        validate_header(&header)?;
        if header.id != *session_id {
            return Err(SessionError::HeaderIdMismatch);
        }
        let events = self
            .persistence
            .read_from(session_id, 0, cancellation.clone())
            .await?;
        let session = Arc::new(Session::from_committed(
            header,
            events,
            Arc::clone(&self.persistence),
        )?);

        let committed_events = session.events();
        if let Some(turn) = orphan_turn(&committed_events)? {
            if mode == RestoreMode::Live {
                return Err(SessionError::OrphanTurn);
            }
            let event = SessionEvent {
                event_type: "turn/end".into(),
                seq: session.next_seq()?,
                time: session
                    .events()
                    .last()
                    .map(|event| event.time)
                    .unwrap_or(session.header.created_at),
                data: json!({
                    "turn": turn,
                    "reason": {"kind": "interrupted"},
                    "synthetic": true,
                }),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            };
            session.append(event, cancellation.clone()).await?;
        }

        self.insert_live(session_id.clone(), Arc::clone(&session))?;
        Ok(session)
    }

    pub fn get(&self, session_id: &SessionId) -> Option<Arc<Session>> {
        lock(&self.live).get(session_id).cloned()
    }

    /// Lists currently live sessions in stable session-id order.
    pub fn list(&self) -> Vec<Arc<Session>> {
        lock(&self.live).values().cloned().collect()
    }

    fn ensure_not_live(&self, session_id: &SessionId) -> Result<(), SessionError> {
        if lock(&self.live).contains_key(session_id) {
            return Err(SessionError::DuplicateLive(session_id.clone()));
        }
        Ok(())
    }

    fn insert_live(
        &self,
        session_id: SessionId,
        session: Arc<Session>,
    ) -> Result<(), SessionError> {
        let mut live = lock(&self.live);
        if live.contains_key(&session_id) {
            return Err(SessionError::DuplicateLive(session_id));
        }
        live.insert(session_id, session);
        Ok(())
    }
}

fn build_state(
    header: &SessionHeader,
    events: Vec<SessionEvent>,
) -> Result<SessionState, SessionError> {
    validate_header(header)?;
    let seed_length = usize::try_from(header.seed_length.unwrap_or_default()).map_err(|_| {
        SessionError::InvalidSeedLength {
            seed_length: header.seed_length.unwrap_or_default(),
            event_count: events.len() as u64,
        }
    })?;
    if seed_length > events.len() {
        return Err(SessionError::InvalidSeedLength {
            seed_length: header.seed_length.unwrap_or_default(),
            event_count: events.len() as u64,
        });
    }

    let mut state = SessionState {
        events: Vec::with_capacity(events.len()),
        surface: Vec::new(),
        seed_length,
        end_seed_seen: false,
    };
    for event in events {
        event.validate()?;
        let expected = next_seq(&state.events)?;
        if event.seq != expected {
            return Err(SessionError::SequenceGap {
                expected,
                actual: event.seq,
            });
        }
        validate_seed_boundary(header, &state, &event)?;
        validate_sources(&event, &state.events)?;
        let projection = decode_surface_event(&event)?;
        validate_surface_operation(&event, projection.as_ref(), &state.surface)?;
        if event.event_type == "session/end-seed" {
            state.end_seed_seen = true;
        }
        apply_surface_operation(&event, projection, &mut state.surface);
        state.events.push(event);
    }
    Ok(state)
}

fn validate_header(header: &SessionHeader) -> Result<(), SessionError> {
    header.validate()?;
    if header.id.as_str().is_empty() {
        return Err(SessionError::EmptySessionId);
    }
    Ok(())
}

fn next_seq(events: &[SessionEvent]) -> Result<u64, SessionError> {
    match events.last() {
        None => Ok(0),
        Some(event) => event
            .seq
            .checked_add(1)
            .ok_or(SessionError::SequenceExhausted),
    }
}

fn validate_seed_boundary(
    header: &SessionHeader,
    state: &SessionState,
    event: &SessionEvent,
) -> Result<(), SessionError> {
    if event.event_type != "session/end-seed" {
        return Ok(());
    }
    if state.end_seed_seen {
        return Err(SessionError::DuplicateSeedBoundary);
    }
    if state.events.len() != state.seed_length || header.seed_length.is_none() {
        return Err(SessionError::InvalidSeedBoundary);
    }
    Ok(())
}

fn validate_sources(
    event: &SessionEvent,
    prior_events: &[SessionEvent],
) -> Result<(), SessionError> {
    let Some(sources) = &event.source_event_seqs else {
        return Ok(());
    };
    for source_seq in sources {
        if !prior_events.iter().any(|prior| prior.seq == *source_seq) {
            return Err(SessionError::MissingSourceEvent {
                source_seq: *source_seq,
            });
        }
    }
    Ok(())
}

fn decode_surface_event(event: &SessionEvent) -> Result<Option<SurfaceMessage>, SessionError> {
    let expected_role = match event.event_type.as_str() {
        "user/message" => MessageRole::User,
        "assistant/message" => MessageRole::Assistant,
        "tool/result" => MessageRole::User,
        _ => return Ok(None),
    };
    let data = if event.event_type == "user/message" {
        event.data.clone()
    } else {
        event
            .data
            .get("message")
            .cloned()
            .ok_or(SessionError::InvalidSurfaceMessage)?
    };
    let message: Message =
        serde_json::from_value(data).map_err(|_| SessionError::InvalidSurfaceMessage)?;
    message
        .validate()
        .map_err(|_| SessionError::InvalidSurfaceMessage)?;
    if message.role != expected_role {
        return Err(SessionError::InvalidSurfaceRole);
    }
    Ok(Some(SurfaceMessage {
        event_seq: event.seq,
        message,
        source_event_seqs: event.source_event_seqs.clone(),
    }))
}

fn validate_surface_operation(
    event: &SessionEvent,
    projection: Option<&SurfaceMessage>,
    surface: &[SurfaceMessage],
) -> Result<(), SessionError> {
    let Some(_) = projection else {
        return Ok(());
    };
    match event.surface_op.as_ref() {
        Some(SurfaceOp::Append) => Ok(()),
        Some(SurfaceOp::Replace { start, end }) => {
            let len = surface.len() as u64;
            if start > end || *end > len {
                return Err(SessionError::SurfaceRange {
                    start: *start,
                    end: *end,
                    len,
                });
            }
            Ok(())
        }
        None => Err(SessionError::InvalidSurfaceMessage),
    }
}

fn apply_surface_operation(
    event: &SessionEvent,
    projection: Option<SurfaceMessage>,
    surface: &mut Vec<SurfaceMessage>,
) {
    let Some(projection) = projection else {
        return;
    };
    match event.surface_op.as_ref() {
        Some(SurfaceOp::Append) => surface.push(projection),
        Some(SurfaceOp::Replace { start, end }) => {
            surface.splice(*start as usize..*end as usize, std::iter::once(projection));
        }
        None => unreachable!("surface events are validated before admission"),
    }
}

fn orphan_turn(events: &[SessionEvent]) -> Result<Option<u64>, SessionError> {
    let mut open = None;
    for event in events {
        match event.event_type.as_str() {
            "turn/start" => {
                let turn = turn_id(event)?;
                if open.replace(turn).is_some() {
                    return Err(SessionError::InvalidTurnLifecycle);
                }
            }
            "turn/end" => {
                let turn = turn_id(event)?;
                if open != Some(turn) {
                    return Err(SessionError::InvalidTurnLifecycle);
                }
                open = None;
            }
            _ => {}
        }
    }
    Ok(open)
}

fn turn_id(event: &SessionEvent) -> Result<u64, SessionError> {
    event
        .data
        .get("turn")
        .and_then(Value::as_u64)
        .ok_or(SessionError::InvalidTurnId)
}

fn inspect_memory_session(session: &MemorySession) -> Result<SessionInspection, SessionError> {
    Ok(SessionInspection {
        header: session.header.clone(),
        event_count: session.events.len() as u64,
        next_seq: next_seq(&session.events)?,
        flush_count: session.flush_count,
    })
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), SessionError> {
    if cancellation.is_cancelled() {
        Err(SessionError::Cancelled)
    } else {
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poison| poison.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poison| poison.into_inner())
}

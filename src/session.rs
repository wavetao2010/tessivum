//! Durable, in-memory session history and its persistence boundary.
//!
//! A [`Session`] never exposes mutable references to admitted events. Writes are
//! serialized through one async gate, persisted first, then made visible to
//! readers and subscribers.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use async_trait::async_trait;
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ServiceKey};
use thiserror::Error;
use tokio::sync::{broadcast, Mutex as AsyncMutex};

use crate::{
    error::TessivumError,
    protocol::{
        ContentBlock, Message, MessageId, MessageRole, MessageSource, SessionEvent, SessionHeader,
        SessionId, SurfaceOp,
    },
};

/// Stable key for the native session service.
pub fn session_service_key() -> ServiceKey {
    ServiceKey::new("harness.sessions", "1")
}

/// Whether restoration is resuming an agent, reading metadata, or recovering a cold one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreMode {
    /// A running host is resuming the exact committed log and must not repair it.
    Live,
    /// Loads a durable session for metadata mutation without starting or repairing a turn.
    Metadata,
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
/// Backend-owned bytes for one materialized session log.
///
/// `bytes` are never reconstructed from parsed events. Backends with a physical
/// encoding decode only that encoding before returning the durable JSONL bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRawArtifact {
    pub filename: String,
    pub bytes: Vec<u8>,
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

    #[error("invalid durable inbox event: {reason}")]
    InvalidInboxEvent { reason: String },
    #[error("durable inbox mutation is out of order for message {item_id}")]
    InboxMutationOutOfOrder { item_id: MessageId },
    #[error("this persistence backend does not expose per-session raw artifacts")]
    RawArtifactsUnsupported,
    #[error("this persistence backend does not support session deletion")]
    DeleteUnsupported,
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
            Self::InvalidInboxEvent { .. } => "INVALID_INBOX_EVENT",
            Self::InboxMutationOutOfOrder { .. } => "INBOX_MUTATION_OUT_OF_ORDER",
            Self::RawArtifactsUnsupported => "SESSION_RAW_ARTIFACTS_UNSUPPORTED",
            Self::DeleteUnsupported => "SESSION_DELETE_UNSUPPORTED",
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
    /// Permanently removes one complete durable session.
    async fn delete(
        &self,
        _session_id: &SessionId,
        _cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        Err(SessionError::DeleteUnsupported)
    }

    /// Whether this backend exposes one verbatim materialized artifact per session.
    fn supports_raw_artifacts(&self) -> bool {
        false
    }

    /// Reads a backend-owned session artifact without reconstructing it from events.
    async fn read_raw(
        &self,
        _session_id: &SessionId,
        _cancellation: CancellationToken,
    ) -> Result<Option<SessionRawArtifact>, SessionError> {
        Err(SessionError::RawArtifactsUnsupported)
    }

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
    async fn delete(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        lock(&self.sessions)
            .remove(session_id)
            .map(|_| ())
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))
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

    /// Rebuilds the durable, not-yet-claimed next-turn inbox in FIFO order.
    /// Next-step entries are intentionally not resumed after a host restart.
    pub fn pending_next_turn_inbox(&self) -> Result<Vec<Message>, SessionError> {
        Ok(replay_inbox(&read_lock(&self.state).events)?
            .followups
            .into_iter()
            .collect())
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

    /// Builds, persists, and atomically admits the next event without exposing a racy sequence
    /// snapshot to the caller.
    pub async fn append_next(
        &self,
        build: impl FnOnce(u64) -> SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<u64, SessionError> {
        check_cancellation(&cancellation)?;
        let _gate = self.write_gate.lock().await;
        check_cancellation(&cancellation)?;
        let seq = next_seq(&read_lock(&self.state).events)?;
        self.append_under_gate(build(seq), None, cancellation)
            .await?;
        Ok(seq)
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
        self.append_under_gate(event, expected_surface_event_seqs, cancellation)
            .await
    }

    async fn append_under_gate(
        &self,
        event: SessionEvent,
        expected_surface_event_seqs: Option<&[u64]>,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
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
            if is_inbox_event(&event) {
                let mut inbox = replay_inbox(&state.events)?;
                inbox.apply(&event)?;
            }
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
        let closers = interrupted_turn_closers(&committed_events)?;
        if !closers.is_empty() {
            if mode == RestoreMode::Live {
                return Err(SessionError::OrphanTurn);
            }
            if mode != RestoreMode::Cold {
                self.insert_live(session_id.clone(), Arc::clone(&session))?;
                return Ok(session);
            }
            for event in closers {
                session.append(event, cancellation.clone()).await?;
            }
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
    /// Evicts the resident copy after its durable record is removed.
    pub(crate) fn remove(&self, session_id: &SessionId) {
        lock(&self.live).remove(session_id);
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
    let _ = replay_inbox(&state.events)?;
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

fn interrupted_turn_closers(events: &[SessionEvent]) -> Result<Vec<SessionEvent>, SessionError> {
    struct PendingCall {
        call_id: crate::protocol::ToolCallId,
        step: u64,
        call_seq: Option<u64>,
    }

    let mut open_turn = None;
    let mut open_step = None;
    let mut pending = Vec::<PendingCall>::new();
    for event in events {
        match event.event_type.as_str() {
            "turn/start" => {
                open_turn = Some(turn_id(event)?);
                open_step = None;
                pending.clear();
            }
            "turn/end" => {
                open_turn = None;
                open_step = None;
                pending.clear();
            }
            "step/start" => {
                open_step = event.data.get("step").and_then(Value::as_u64);
            }
            "step/end" => {
                open_step = None;
                pending.clear();
            }
            "assistant/message" => {
                let step = event
                    .data
                    .get("step")
                    .and_then(Value::as_u64)
                    .ok_or(SessionError::InvalidTurnLifecycle)?;
                let message =
                    decode_surface_event(event)?.ok_or(SessionError::InvalidSurfaceMessage)?;
                for block in message.message.content {
                    if let ContentBlock::ToolCall { id, .. } = block {
                        pending.push(PendingCall {
                            call_id: id,
                            step,
                            call_seq: None,
                        });
                    }
                }
            }
            "tool/call" => {
                let call_id = event.data.get("callId").and_then(Value::as_str);
                if let Some(call) = pending
                    .iter_mut()
                    .find(|call| Some(call.call_id.as_str()) == call_id)
                {
                    call.call_seq = Some(event.seq);
                }
            }
            "tool/result" => {
                let message =
                    decode_surface_event(event)?.ok_or(SessionError::InvalidSurfaceMessage)?;
                if let MessageSource::Tool { call_id } = message.message.source {
                    pending.retain(|call| call.call_id != call_id);
                }
            }
            _ => {}
        }
    }

    let Some(turn) = open_turn else {
        return Ok(Vec::new());
    };
    let Some(last) = events.last() else {
        return Ok(Vec::new());
    };
    let mut seq = last
        .seq
        .checked_add(1)
        .ok_or(SessionError::SequenceExhausted)?;
    let mut closers = Vec::with_capacity(pending.len() + usize::from(open_step.is_some()) + 1);
    for call in pending {
        let (code, name, text) = if call.call_seq.is_some() {
            (
                "TOOL_OUTCOME_UNKNOWN",
                "ToolOutcomeUnknownError",
                "The tool call was interrupted after it was recorded, but no result was durably recorded. Its outcome is unknown. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly.",
            )
        } else {
            (
                "TOOL_NOT_STARTED",
                "ToolNotStartedError",
                "The tool call was interrupted before the Harness recorded it as started. Retry it if it is still needed.",
            )
        };
        let call_id = call.call_id;
        let message = Message {
            id: MessageId::from(format!(
                "interrupted-tool-result-{}-{seq}",
                call_id.as_str()
            )),
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: call_id.clone(),
                content: vec![ContentBlock::Text { text: text.into() }],
                is_error: Some(true),
            }],
            source: MessageSource::Tool { call_id },
        };
        closers.push(SessionEvent {
            event_type: "tool/result".into(),
            seq,
            time: last.time,
            data: json!({
                "turn": turn,
                "step": call.step,
                "message": message,
                "error": {"name": name, "code": code},
            }),
            ignorable: None,
            source_event_seqs: call.call_seq.map(|source_seq| vec![source_seq]),
            surface_op: Some(SurfaceOp::Append),
        });
        seq = seq.checked_add(1).ok_or(SessionError::SequenceExhausted)?;
    }
    if let Some(step) = open_step {
        closers.push(SessionEvent {
            event_type: "step/end".into(),
            seq,
            time: last.time,
            data: json!({"turn": turn, "step": step}),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
        });
        seq = seq.checked_add(1).ok_or(SessionError::SequenceExhausted)?;
    }
    closers.push(SessionEvent {
        event_type: "turn/end".into(),
        seq,
        time: last.time,
        data: json!({"turn": turn, "reason": {"kind": "interrupted"}, "synthetic": true}),
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
    });
    Ok(closers)
}

fn turn_id(event: &SessionEvent) -> Result<u64, SessionError> {
    event
        .data
        .get("turn")
        .and_then(Value::as_u64)
        .ok_or(SessionError::InvalidTurnId)
}

const INBOX_ENQUEUED_EVENT: &str = "agent/inbox/enqueued";
const INBOX_SPLICED_EVENT: &str = "agent/inbox/spliced";

#[derive(Clone, Copy, Eq, PartialEq)]
enum InboxReplayTarget {
    Followup,
    Step,
}

struct InboxReplay {
    followups: VecDeque<Message>,
    steps: VecDeque<Message>,
}

impl InboxReplay {
    fn apply(&mut self, event: &SessionEvent) -> Result<(), SessionError> {
        match event.event_type.as_str() {
            INBOX_ENQUEUED_EVENT => {
                let target = inbox_target(event)?;
                let message = inbox_message(event)?;
                if self.find(&message.id).is_some() {
                    return Err(inbox_event("message id is already pending"));
                }
                self.queue_mut(target).push_back(message);
                Ok(())
            }
            INBOX_SPLICED_EVENT => {
                if event.data.get("action").is_none()
                    && (event.data.get("start").is_some() || event.data.get("inserted").is_some())
                {
                    let target = inbox_target(event)?;
                    let start = event
                        .data
                        .get("start")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| inbox_event("splice start is missing"))?;
                    let removed_count = event
                        .data
                        .get("removedCount")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let start = usize::try_from(start)
                        .map_err(|_| inbox_event("splice start is invalid"))?;
                    let removed_count = usize::try_from(removed_count)
                        .map_err(|_| inbox_event("splice removedCount is invalid"))?;
                    let inserted = event
                        .data
                        .get("inserted")
                        .and_then(Value::as_array)
                        .ok_or_else(|| inbox_event("splice inserted is missing"))?;
                    let inserted = inserted
                        .iter()
                        .cloned()
                        .map(|value| {
                            let message: Message = serde_json::from_value(value)
                                .map_err(|_| inbox_event("splice inserted message is invalid"))?;
                            message
                                .validate()
                                .map_err(|_| inbox_event("splice inserted message is invalid"))?;
                            Ok(message)
                        })
                        .collect::<Result<Vec<Message>, SessionError>>()?;
                    let queue_len = match target {
                        InboxReplayTarget::Followup => self.followups.len(),
                        InboxReplayTarget::Step => self.steps.len(),
                    };
                    if start > queue_len || removed_count > queue_len - start {
                        return Err(inbox_event("splice range is invalid"));
                    }
                    for (index, message) in inserted.iter().enumerate() {
                        let replaces_removed = self.find(&message.id).is_some_and(
                            |(existing_target, existing_index)| {
                                existing_target == target
                                    && (start..start + removed_count).contains(&existing_index)
                            },
                        );
                        if (self.find(&message.id).is_some() && !replaces_removed)
                            || inserted[..index]
                                .iter()
                                .any(|candidate| candidate.id == message.id)
                        {
                            return Err(inbox_event("message id is already pending"));
                        }
                    }
                    let queue = self.queue_mut(target);
                    queue.drain(start..start + removed_count);
                    for (offset, message) in inserted.into_iter().enumerate() {
                        queue.insert(start + offset, message);
                    }
                    return Ok(());
                }
                let item_id = inbox_item_id(event)?;
                let target = inbox_target(event)?;
                let message = inbox_message(event)?;
                if message.id != item_id {
                    return Err(inbox_event("splice message id does not match itemId"));
                }
                let action = event
                    .data
                    .get("action")
                    .and_then(Value::as_str)
                    .ok_or_else(|| inbox_event("splice action is missing"))?;
                let Some((current_target, index)) = self.find(&item_id) else {
                    return Err(SessionError::InboxMutationOutOfOrder { item_id });
                };
                if action == "steer" {
                    if current_target != InboxReplayTarget::Followup
                        || target != InboxReplayTarget::Step
                    {
                        return Err(inbox_event(
                            "steer does not move a next-turn item to next-step",
                        ));
                    }
                    let original = self.followups.remove(index).expect("validated inbox index");
                    if message != original {
                        return Err(inbox_event("steer changes its message"));
                    }
                    self.steps.push_back(message);
                    return Ok(());
                }
                if current_target != target {
                    return Err(inbox_event("splice target does not match the pending item"));
                }
                match action {
                    "edit" => {
                        self.queue_mut(target)[index] = message;
                        Ok(())
                    }
                    "remove" => {
                        let original = self
                            .queue_mut(target)
                            .remove(index)
                            .expect("validated inbox index");
                        if message != original {
                            return Err(inbox_event("remove changes its message"));
                        }
                        Ok(())
                    }
                    _ => Err(inbox_event("unknown splice action")),
                }
            }
            "user/message" => {
                if let Some(item_id) = event.data.get("id").and_then(Value::as_str) {
                    self.remove(item_id);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn find(&self, item_id: &MessageId) -> Option<(InboxReplayTarget, usize)> {
        self.followups
            .iter()
            .position(|message| message.id == *item_id)
            .map(|index| (InboxReplayTarget::Followup, index))
            .or_else(|| {
                self.steps
                    .iter()
                    .position(|message| message.id == *item_id)
                    .map(|index| (InboxReplayTarget::Step, index))
            })
    }

    fn queue_mut(&mut self, target: InboxReplayTarget) -> &mut VecDeque<Message> {
        match target {
            InboxReplayTarget::Followup => &mut self.followups,
            InboxReplayTarget::Step => &mut self.steps,
        }
    }

    fn remove(&mut self, item_id: &str) {
        if let Some(index) = self
            .followups
            .iter()
            .position(|message| message.id.as_str() == item_id)
        {
            self.followups.remove(index);
        }
        if let Some(index) = self
            .steps
            .iter()
            .position(|message| message.id.as_str() == item_id)
        {
            self.steps.remove(index);
        }
    }
}

fn replay_inbox(events: &[SessionEvent]) -> Result<InboxReplay, SessionError> {
    let mut inbox = InboxReplay {
        followups: VecDeque::new(),
        steps: VecDeque::new(),
    };
    for event in events {
        inbox.apply(event)?;
    }
    Ok(inbox)
}

fn is_inbox_event(event: &SessionEvent) -> bool {
    matches!(
        event.event_type.as_str(),
        INBOX_ENQUEUED_EVENT | INBOX_SPLICED_EVENT
    )
}

fn inbox_target(event: &SessionEvent) -> Result<InboxReplayTarget, SessionError> {
    match event.data.get("target").and_then(Value::as_str) {
        Some("next-turn") => Ok(InboxReplayTarget::Followup),
        Some("next-step") => Ok(InboxReplayTarget::Step),
        _ => Err(inbox_event("inbox target must be next-turn or next-step")),
    }
}

fn inbox_message(event: &SessionEvent) -> Result<Message, SessionError> {
    let message: Message = serde_json::from_value(
        event
            .data
            .get("message")
            .cloned()
            .ok_or_else(|| inbox_event("inbox message is missing"))?,
    )
    .map_err(|_| inbox_event("inbox message is invalid"))?;
    message
        .validate()
        .map_err(|_| inbox_event("inbox message is invalid"))?;
    Ok(message)
}

fn inbox_item_id(event: &SessionEvent) -> Result<MessageId, SessionError> {
    serde_json::from_value(
        event
            .data
            .get("itemId")
            .cloned()
            .ok_or_else(|| inbox_event("splice itemId is missing"))?,
    )
    .map_err(|_| inbox_event("splice itemId is invalid"))
}

fn inbox_event(reason: impl Into<String>) -> SessionError {
    SessionError::InvalidInboxEvent {
        reason: reason.into(),
    }
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

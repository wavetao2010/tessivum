//! SQLite-backed durable [`SessionPersistence`](crate::session::SessionPersistence).
//!
//! Each mutating operation uses an immediate SQLite transaction. The database is
//! the commit point: a failed transaction is never reflected by a later read.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use tessivum_core::CancellationToken;

use crate::{
    agent_mode::AgentModeId,
    error::TessivumError,
    protocol::{
        migrate_legacy_agent_preset, migrate_legacy_agent_preset_selection, SessionEvent,
        SessionHeader, SessionId, SessionOrigin, SurfaceOp,
    },
    session::{SessionError, SessionInspection, SessionPersistence},
};

const STORAGE_VERSION: i64 = 1;

/// Connection options for [`SqliteSessionPersistence`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SqliteSessionPersistenceOptions {
    /// Uses SQLite WAL journalling. This is the durable default for file databases.
    pub wal: bool,
}

impl Default for SqliteSessionPersistenceOptions {
    fn default() -> Self {
        Self { wal: true }
    }
}

/// Persistent state kept alongside a session log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqlitePersistenceState {
    pub version: u64,
    pub revision: u64,
    pub incarnation: u64,
    pub next_seq: u64,
    pub flush_count: u64,
}

/// SQLite implementation of the session persistence boundary.
///
/// The connection is intentionally serialized. SQLite supplies process-level
/// coordination for separately opened instances, while the mutex prevents one
/// instance from interleaving a transaction with a read on its connection.
#[derive(Clone, Debug)]
pub struct SqliteSessionPersistence {
    path: PathBuf,
    connection: Arc<Mutex<Connection>>,
}

impl SqliteSessionPersistence {
    /// Opens (and initializes) a database with WAL enabled by default.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        Self::open_with_options(path, SqliteSessionPersistenceOptions::default())
    }

    /// Alias for [`Self::open`].
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        Self::open(path)
    }

    /// Opens a database with explicitly selected journal behaviour.
    pub fn open_with_options(
        path: impl Into<PathBuf>,
        options: SqliteSessionPersistenceOptions,
    ) -> Result<Self, SessionError> {
        let path = path.into();
        create_parent(&path)?;
        let connection =
            Connection::open(&path).map_err(|error| sqlite_error("open database", error))?;
        #[cfg(unix)]
        if path.as_os_str() != ":memory:" {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(
                |error| {
                    persistence_error(
                        "secure database permissions",
                        format!("{}: {error}", path.display()),
                    )
                },
            )?;
        }
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| sqlite_error("configure busy timeout", error))?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")
            .map_err(|error| sqlite_error("configure database", error))?;
        if options.wal {
            // SQLite returns `memory` for an in-memory database; accepting that is
            // intentional because WAL is not available for it.
            connection
                .pragma_update(None, "journal_mode", "WAL")
                .map_err(|error| sqlite_error("enable WAL", error))?;
        }
        initialize(&connection)?;
        Ok(Self {
            path,
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Returns the caller-selected database path without normalization.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the durable revision information for one session.
    pub fn state(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SqlitePersistenceState>, SessionError> {
        let connection = lock(&self.connection);
        state_for(&connection, session_id)
    }

    /// Removes the committed suffix beginning at `from_seq` in one transaction.
    ///
    /// This is an explicit administrative recovery operation. Normal cold restore
    /// remains responsible for deciding whether any repair is appropriate.
    pub async fn rollback(
        &self,
        session_id: &SessionId,
        from_seq: u64,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        let mut connection = lock(&self.connection);
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin rollback", error))?;
        let session = read_session(&transaction, session_id)?
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))?;
        if from_seq > session.state.next_seq {
            return Err(SessionError::SequenceGap {
                expected: session.state.next_seq,
                actual: from_seq,
            });
        }
        check_cancellation(&cancellation)?;
        transaction
            .execute(
                "DELETE FROM events WHERE session_id = ?1 AND seq >= ?2",
                params![session_id.as_str(), to_i64(from_seq, "rollback sequence")?],
            )
            .map_err(|error| sqlite_error("delete event suffix", error))?;
        transaction
            .execute(
                "UPDATE persistence_state
                 SET next_seq = ?2, revision = revision + 1, incarnation = incarnation + 1
                 WHERE session_id = ?1",
                params![session_id.as_str(), to_i64(from_seq, "rollback sequence")?],
            )
            .map_err(|error| sqlite_error("update rollback state", error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit rollback", error))
    }
}

#[async_trait]
impl SessionPersistence for SqliteSessionPersistence {
    async fn create(
        &self,
        header: &SessionHeader,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        validate_header(header)?;
        let mut connection = lock(&self.connection);
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin create", error))?;
        let existing = transaction
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![header.id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| sqlite_error("check existing session", error))?;
        if existing.is_some() {
            return Err(SessionError::AlreadyExists(header.id.clone()));
        }
        check_cancellation(&cancellation)?;
        insert_header(&transaction, header)?;
        transaction
            .execute(
                "INSERT INTO persistence_state
                 (session_id, version, revision, incarnation, next_seq, flush_count)
                 VALUES (?1, ?2, 0, 1, 0, 0)",
                params![header.id.as_str(), STORAGE_VERSION],
            )
            .map_err(|error| sqlite_error("create persistence state", error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit create", error))
    }

    async fn append(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        event.validate()?;
        let mut connection = lock(&self.connection);
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin append", error))?;
        let session = read_session(&transaction, session_id)?
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))?;
        let expected = session.state.next_seq;
        if event.seq != expected {
            return Err(SessionError::SequenceGap {
                expected,
                actual: event.seq,
            });
        }
        let next_seq = expected
            .checked_add(1)
            .ok_or(SessionError::SequenceExhausted)?;
        check_cancellation(&cancellation)?;
        insert_event(&transaction, session_id, event)?;
        transaction
            .execute(
                "UPDATE persistence_state SET next_seq = ?2, revision = revision + 1 WHERE session_id = ?1",
                params![session_id.as_str(), to_i64(next_seq, "next sequence")?],
            )
            .map_err(|error| sqlite_error("advance persistence state", error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit append", error))
    }
    async fn delete(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        let mut connection = lock(&self.connection);
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin delete", error))?;
        check_cancellation(&cancellation)?;
        if transaction
            .execute(
                "DELETE FROM sessions WHERE id = ?1",
                params![session_id.as_str()],
            )
            .map_err(|error| sqlite_error("delete session", error))?
            == 0
        {
            return Err(SessionError::NotFound(session_id.clone()));
        }
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit delete", error))
    }

    async fn load(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionHeader>, SessionError> {
        check_cancellation(&cancellation)?;
        let connection = lock(&self.connection);
        Ok(read_session(&connection, session_id)?.map(|session| session.header))
    }

    async fn inspect(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionInspection>, SessionError> {
        check_cancellation(&cancellation)?;
        let connection = lock(&self.connection);
        read_session(&connection, session_id)?
            .map(|session| inspection(&session))
            .transpose()
    }

    async fn read_from(
        &self,
        session_id: &SessionId,
        from_seq: u64,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        check_cancellation(&cancellation)?;
        let connection = lock(&self.connection);
        let session = read_session(&connection, session_id)?
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))?;
        Ok(session
            .events
            .into_iter()
            .filter(|event| event.seq >= from_seq)
            .collect())
    }

    async fn flush(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        let mut connection = lock(&self.connection);
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| sqlite_error("begin flush", error))?;
        if read_session(&transaction, session_id)?.is_none() {
            return Err(SessionError::NotFound(session_id.clone()));
        }
        check_cancellation(&cancellation)?;
        transaction
            .execute(
                "UPDATE persistence_state SET flush_count = flush_count + 1, revision = revision + 1
                 WHERE session_id = ?1",
                params![session_id.as_str()],
            )
            .map_err(|error| sqlite_error("advance flush state", error))?;
        transaction
            .commit()
            .map_err(|error| sqlite_error("commit flush", error))
    }

    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SessionError> {
        check_cancellation(&cancellation)?;
        let connection = lock(&self.connection);
        let mut statement = connection
            .prepare("SELECT id FROM sessions ORDER BY id")
            .map_err(|error| sqlite_error("prepare session list", error))?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| sqlite_error("read session list", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("decode session list", error))?;
        let mut sessions = Vec::with_capacity(ids.len());
        for id in ids {
            check_cancellation(&cancellation)?;
            let id = SessionId::from(id);
            let session = read_session(&connection, &id)?.ok_or_else(|| {
                corrupt(
                    "session disappeared while listing",
                    json!({"id": id.as_str()}),
                )
            })?;
            sessions.push(inspection(&session)?);
        }
        Ok(sessions)
    }
}

struct StoredSession {
    header: SessionHeader,
    events: Vec<SessionEvent>,
    state: SqlitePersistenceState,
}

fn initialize(connection: &Connection) -> Result<(), SessionError> {
    connection
        .execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY NOT NULL,
                version INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                cwd TEXT NULL,
                parent_session_id TEXT NULL,
                seed_length INTEGER NULL,
                origin_json TEXT NULL,
                delegation_depth INTEGER NULL,
                agent_mode TEXT NULL
             );
             CREATE TABLE IF NOT EXISTS events (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                seq INTEGER NOT NULL CHECK (seq >= 0),
                event_type TEXT NOT NULL,
                time INTEGER NOT NULL CHECK (time >= 0),
                data_json TEXT NOT NULL,
                ignorable INTEGER NULL CHECK (ignorable IS NULL OR ignorable = 1),
                source_event_seqs_json TEXT NULL,
                surface_op_json TEXT NULL,
                PRIMARY KEY (session_id, seq)
             );
             CREATE TABLE IF NOT EXISTS persistence_state (
                session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                version INTEGER NOT NULL CHECK (version = 1),
                revision INTEGER NOT NULL CHECK (revision >= 0),
                incarnation INTEGER NOT NULL CHECK (incarnation >= 1),
                next_seq INTEGER NOT NULL CHECK (next_seq >= 0),
                flush_count INTEGER NOT NULL CHECK (flush_count >= 0)
             );
             CREATE INDEX IF NOT EXISTS events_session_seq ON events(session_id, seq);
             COMMIT;",
        )
        .map_err(|error| sqlite_error("initialize schema", error))?;
    migrate_legacy_session_headers(connection)
}

fn migrate_legacy_session_headers(connection: &Connection) -> Result<(), SessionError> {
    let columns = {
        let mut statement = connection
            .prepare("PRAGMA table_info(sessions)")
            .map_err(|error| sqlite_error("inspect session schema", error))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| sqlite_error("read session schema", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("decode session schema", error))?;
        columns
    };
    if !columns.iter().any(|column| column == "agent_preset") {
        return Ok(());
    }

    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|error| sqlite_error("begin mode migration", error))?;
    if !columns.iter().any(|column| column == "agent_mode") {
        transaction
            .execute_batch("ALTER TABLE sessions ADD COLUMN agent_mode TEXT NULL")
            .map_err(|error| sqlite_error("add agent mode column", error))?;
    }
    let legacy_rows = {
        let mut statement = transaction
            .prepare("SELECT id, agent_preset FROM sessions WHERE agent_preset IS NOT NULL")
            .map_err(|error| sqlite_error("read legacy agent presets", error))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| sqlite_error("query legacy agent presets", error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| sqlite_error("decode legacy agent presets", error))?;
        rows
    };
    for (id, legacy_preset) in legacy_rows {
        let agent_mode = migrate_legacy_agent_preset(&legacy_preset)?;
        transaction
            .execute(
                "UPDATE sessions SET agent_mode = ?2 WHERE id = ?1 AND agent_mode IS NULL",
                params![id, agent_mode.as_str()],
            )
            .map_err(|error| sqlite_error("migrate legacy agent preset", error))?;
    }
    transaction
        .commit()
        .map_err(|error| sqlite_error("commit mode migration", error))
}

fn insert_header(
    transaction: &Transaction<'_>,
    header: &SessionHeader,
) -> Result<(), SessionError> {
    let origin_json = header
        .origin
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| {
            corrupt(
                "serialize header origin",
                json!({"error": error.to_string()}),
            )
        })?;
    transaction
        .execute(
            "INSERT INTO sessions
             (id, version, created_at, cwd, parent_session_id, seed_length, origin_json, delegation_depth, agent_mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                header.id.as_str(),
                to_i64(header.version, "header version")?,
                to_i64(header.created_at, "header creation time")?,
                header.cwd,
                header.parent_session.as_ref().map(SessionId::as_str),
                header.seed_length.map(|value| to_i64(value, "header seed length")).transpose()?,
                origin_json,
                header.delegation_depth.map(|value| to_i64(value, "header delegation depth")).transpose()?,
                header.agent_mode.as_ref().map(AgentModeId::as_str),
            ],
        )
        .map_err(|error| sqlite_error("insert session header", error))?;
    Ok(())
}

fn insert_event(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    event: &SessionEvent,
) -> Result<(), SessionError> {
    let data_json = serde_json::to_string(&event.data)
        .map_err(|error| corrupt("serialize event data", json!({"error": error.to_string()})))?;
    let sources_json = event
        .source_event_seqs
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| {
            corrupt(
                "serialize event sources",
                json!({"error": error.to_string()}),
            )
        })?;
    let surface_json = event
        .surface_op
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| {
            corrupt(
                "serialize surface operation",
                json!({"error": error.to_string()}),
            )
        })?;
    transaction
        .execute(
            "INSERT INTO events
             (session_id, seq, event_type, time, data_json, ignorable, source_event_seqs_json, surface_op_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id.as_str(),
                to_i64(event.seq, "event sequence")?,
                event.event_type,
                to_i64(event.time, "event time")?,
                data_json,
                event.ignorable.map(|value| if value { 1_i64 } else { 0_i64 }),
                sources_json,
                surface_json,
            ],
        )
        .map_err(|error| sqlite_error("insert event", error))?;
    Ok(())
}

fn read_session(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Option<StoredSession>, SessionError> {
    let header = connection
        .query_row(
            "SELECT version, id, created_at, cwd, parent_session_id, seed_length, origin_json, delegation_depth, agent_mode
             FROM sessions WHERE id = ?1",
            params![session_id.as_str()],
            decode_header,
        )
        .optional()
        .map_err(|error| sqlite_error("read session header", error))?;
    let Some(header) = header else {
        return Ok(None);
    };
    if header.id != *session_id {
        return Err(corrupt(
            "session header identity does not match its row",
            json!({"id": session_id.as_str()}),
        ));
    }
    validate_header(&header)?;
    let state = state_for(connection, session_id)?.ok_or_else(|| {
        corrupt(
            "session has no persistence state",
            json!({"id": session_id.as_str()}),
        )
    })?;
    let mut statement = connection
        .prepare(
            "SELECT seq, event_type, time, data_json, ignorable, source_event_seqs_json, surface_op_json
             FROM events WHERE session_id = ?1 ORDER BY seq",
        )
        .map_err(|error| sqlite_error("prepare event read", error))?;
    let mut rows = statement
        .query(params![session_id.as_str()])
        .map_err(|error| sqlite_error("read events", error))?;
    let mut events = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|error| sqlite_error("read event row", error))?
    {
        events.push(decode_event(row)?);
    }
    validate_committed(&events, &state, session_id)?;
    Ok(Some(StoredSession {
        header,
        events,
        state,
    }))
}

fn state_for(
    connection: &Connection,
    session_id: &SessionId,
) -> Result<Option<SqlitePersistenceState>, SessionError> {
    connection
        .query_row(
            "SELECT version, revision, incarnation, next_seq, flush_count
             FROM persistence_state WHERE session_id = ?1",
            params![session_id.as_str()],
            |row| {
                Ok(SqlitePersistenceState {
                    version: from_i64(row.get(0)?, "state version").map_err(to_sql_error)?,
                    revision: from_i64(row.get(1)?, "state revision").map_err(to_sql_error)?,
                    incarnation: from_i64(row.get(2)?, "state incarnation")
                        .map_err(to_sql_error)?,
                    next_seq: from_i64(row.get(3)?, "state next sequence").map_err(to_sql_error)?,
                    flush_count: from_i64(row.get(4)?, "state flush count")
                        .map_err(to_sql_error)?,
                })
            },
        )
        .optional()
        .map_err(|error| sqlite_error("read persistence state", error))
}

fn decode_header(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionHeader> {
    let version = from_i64(row.get(0)?, "header version").map_err(to_sql_error)?;
    let id = SessionId::from(row.get::<_, String>(1)?);
    let created_at = from_i64(row.get(2)?, "header creation time").map_err(to_sql_error)?;
    let seed_length = row
        .get::<_, Option<i64>>(5)?
        .map(|value| from_i64(value, "header seed length").map_err(to_sql_error))
        .transpose()?;
    let origin = row
        .get::<_, Option<String>>(6)?
        .map(|json| serde_json::from_str::<SessionOrigin>(&json).map_err(to_sql_error))
        .transpose()?;
    let delegation_depth = row
        .get::<_, Option<i64>>(7)?
        .map(|value| from_i64(value, "header delegation depth").map_err(to_sql_error))
        .transpose()?;
    let agent_mode = row
        .get::<_, Option<String>>(8)?
        .map(|value| AgentModeId::new(value).map_err(to_sql_error))
        .transpose()?;
    Ok(SessionHeader {
        version,
        id,
        created_at,
        cwd: row.get(3)?,
        parent_session: row.get::<_, Option<String>>(4)?.map(SessionId::from),
        seed_length,
        origin,
        delegation_depth,
        agent_mode,
    })
}

fn decode_event(row: &rusqlite::Row<'_>) -> Result<SessionEvent, SessionError> {
    let data_json: String = row
        .get(3)
        .map_err(|error| sqlite_error("decode event data", error))?;
    let sources_json: Option<String> = row
        .get(5)
        .map_err(|error| sqlite_error("decode event sources", error))?;
    let surface_json: Option<String> = row
        .get(6)
        .map_err(|error| sqlite_error("decode event surface", error))?;
    let mut event = SessionEvent {
        event_type: row
            .get(1)
            .map_err(|error| sqlite_error("decode event type", error))?,
        seq: from_i64(
            row.get(0)
                .map_err(|error| sqlite_error("decode event sequence", error))?,
            "event sequence",
        )?,
        time: from_i64(
            row.get(2)
                .map_err(|error| sqlite_error("decode event time", error))?,
            "event time",
        )?,
        data: serde_json::from_str(&data_json)
            .map_err(|error| corrupt("decode event data", json!({"error": error.to_string()})))?,
        ignorable: row
            .get::<_, Option<i64>>(4)
            .map_err(|error| sqlite_error("decode event ignorable", error))?
            .map(|value| value != 0),
        source_event_seqs: sources_json
            .map(|json| {
                serde_json::from_str(&json).map_err(|error| {
                    corrupt("decode event sources", json!({"error": error.to_string()}))
                })
            })
            .transpose()?,
        surface_op: surface_json
            .map(|json| {
                serde_json::from_str::<SurfaceOp>(&json).map_err(|error| {
                    corrupt("decode event surface", json!({"error": error.to_string()}))
                })
            })
            .transpose()?,
    };
    migrate_legacy_agent_preset_selection(&mut event.event_type, &mut event.data)?;
    Ok(event)
}

fn validate_committed(
    events: &[SessionEvent],
    state: &SqlitePersistenceState,
    session_id: &SessionId,
) -> Result<(), SessionError> {
    if state.version != STORAGE_VERSION as u64 || state.incarnation == 0 {
        return Err(corrupt(
            "unsupported persistence state",
            json!({"id": session_id.as_str()}),
        ));
    }
    let mut expected = 0_u64;
    for event in events {
        event.validate().map_err(SessionError::from)?;
        if event.seq != expected {
            return Err(corrupt(
                "committed event sequence is not contiguous",
                json!({"id": session_id.as_str(), "expected": expected, "actual": event.seq}),
            ));
        }
        expected = expected
            .checked_add(1)
            .ok_or(SessionError::SequenceExhausted)?;
    }
    if state.next_seq != expected {
        return Err(corrupt(
            "persistence state does not match committed event sequence",
            json!({"id": session_id.as_str(), "nextSeq": state.next_seq, "expected": expected}),
        ));
    }
    Ok(())
}

fn inspection(session: &StoredSession) -> Result<SessionInspection, SessionError> {
    Ok(SessionInspection {
        header: session.header.clone(),
        event_count: u64::try_from(session.events.len())
            .map_err(|_| SessionError::SequenceExhausted)?,
        next_seq: session.state.next_seq,
        flush_count: session.state.flush_count,
    })
}

fn validate_header(header: &SessionHeader) -> Result<(), SessionError> {
    header.validate()?;
    if header.id.as_str().is_empty() {
        return Err(SessionError::EmptySessionId);
    }
    Ok(())
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), SessionError> {
    if cancellation.is_cancelled() {
        Err(SessionError::Cancelled)
    } else {
        Ok(())
    }
}

fn create_parent(path: &Path) -> Result<(), SessionError> {
    if path.as_os_str() == ":memory:" {
        return Ok(());
    }
    if path.file_name().is_none() {
        return Err(persistence_error(
            "open database",
            "database path must name a file",
        ));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            persistence_error(
                "create database parent",
                format!("{}: {error}", parent.display()),
            )
        })?;
    }
    Ok(())
}

fn to_i64(value: u64, field: &str) -> Result<i64, SessionError> {
    i64::try_from(value).map_err(|_| {
        corrupt(
            "integer exceeds SQLite range",
            json!({"field": field, "value": value}),
        )
    })
}

fn from_i64(value: i64, field: &str) -> Result<u64, SessionError> {
    u64::try_from(value).map_err(|_| {
        corrupt(
            "SQLite integer is negative",
            json!({"field": field, "value": value}),
        )
    })
}

fn to_sql_error(error: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    )
}

fn corrupt(message: impl Into<String>, details: Value) -> SessionError {
    SessionError::Protocol(TessivumError::new(
        "SESSION_LOG_CORRUPT",
        message,
        "persistence",
        details,
    ))
}

fn sqlite_error(operation: &str, error: impl std::fmt::Display) -> SessionError {
    persistence_error(operation, error.to_string())
}

fn persistence_error(operation: &str, message: impl Into<String>) -> SessionError {
    SessionError::Protocol(TessivumError::new(
        "SESSION_PERSISTENCE_IO",
        format!("{operation}: {}", message.into()),
        "persistence",
        Value::Null,
    ))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

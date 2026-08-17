//! Read-only session queries over live memory and durable persistence.
//!
//! Text matching never constructs SQL, regular expressions, or shell input: it
//! is a literal match over semantic message text after Unicode case and whitespace
//! normalization.

use std::{collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tessivum_core::CancellationToken;
use thiserror::Error;

use crate::{
    protocol::{Message, SessionEvent, SessionHeader, SessionId, SurfaceOp},
    session::{SessionError, SessionPersistence, SessionStore},
};

const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 1_000;

/// Inclusive numeric range used by session and event filters.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InclusiveRange {
    pub start: u64,
    pub end: u64,
}

impl InclusiveRange {
    fn contains(self, value: u64) -> bool {
        self.start <= value && value <= self.end
    }

    fn validate(self) -> Result<(), SessionQueryError> {
        if self.start > self.end {
            return Err(SessionQueryError::InvalidRequest(
                "range start exceeds range end".into(),
            ));
        }
        Ok(())
    }
}

/// Clauses applied with AND; multiple event types are one OR clause.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionQueryFilter {
    /// Literal semantic text: case-insensitive and whitespace-flexible.
    pub text: Option<String>,
    /// Accepted event types. Any one matches when nonempty.
    #[serde(default)]
    pub event_types: Vec<String>,
    pub created_at: Option<InclusiveRange>,
    pub event_time: Option<InclusiveRange>,
    pub event_seq: Option<InclusiveRange>,
}

/// Request for a durable/live session list.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListRequest {
    #[serde(default)]
    pub filter: SessionQueryFilter,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// Request for one session's event log.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLogRequest {
    #[serde(default)]
    pub filter: SessionQueryFilter,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

/// One session read. `live` identifies the source chosen for this response.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionRecord {
    pub header: SessionHeader,
    pub events: Vec<SessionEvent>,
    pub live: bool,
}

/// A cursor-paginated response.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionQueryPage<T> {
    pub items: Vec<T>,
    pub cursor: Option<String>,
}

/// One model-visible surface entry rebuilt directly from events.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSurfaceEntry {
    pub event_seq: u64,
    pub message: Message,
    pub source_event_seqs: Option<Vec<u64>>,
}

/// Provenance around a selected log event.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionEventTrace {
    pub event: SessionEvent,
    /// Earlier events explicitly cited by the selected event.
    pub sources: Vec<SessionEvent>,
    /// Later events which explicitly cite the selected event as a replacement source.
    pub replacements: Vec<SessionEvent>,
}

/// Stable query failures.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum SessionQueryError {
    #[error("session query was cancelled")]
    Cancelled,
    #[error("invalid session query: {0}")]
    InvalidRequest(String),
    #[error("invalid session query cursor")]
    InvalidCursor,
    #[error("cursor does not match the normalized request")]
    CursorRequestMismatch,
    #[error("cursor is stale")]
    CursorStale,
    #[error("session does not exist: {0}")]
    NotFound(SessionId),
    #[error("session surface cannot be derived from committed events")]
    InvalidSurface,
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl SessionQueryError {
    /// Stable machine-readable code for API boundaries.
    pub fn code(&self) -> String {
        match self {
            Self::Cancelled => "CANCELLED".into(),
            Self::InvalidRequest(_) => "INVALID_SESSION_QUERY".into(),
            Self::InvalidCursor => "INVALID_SESSION_QUERY_CURSOR".into(),
            Self::CursorRequestMismatch => "SESSION_QUERY_CURSOR_MISMATCH".into(),
            Self::CursorStale => "STALE_SESSION_QUERY_CURSOR".into(),
            Self::NotFound(_) => "SESSION_NOT_FOUND".into(),
            Self::InvalidSurface => "INVALID_SESSION_SURFACE".into(),
            Self::Session(error) => error.code().into(),
        }
    }
}

/// Query facade preferring a live [`SessionStore`] entry over persistence.
#[derive(Clone)]
pub struct SessionQuery {
    store: SessionStore,
    persistence: Arc<dyn SessionPersistence>,
}

impl SessionQuery {
    pub fn new(store: SessionStore, persistence: Arc<dyn SessionPersistence>) -> Self {
        Self { store, persistence }
    }

    /// Reads one exact session, selecting an already-live object before disk.
    pub async fn read(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<SessionRecord, SessionQueryError> {
        check_cancellation(&cancellation)?;
        if let Some(session) = self.store.get(session_id) {
            return Ok(SessionRecord {
                header: session.header(),
                events: session.events(),
                live: true,
            });
        }
        let header = self
            .persistence
            .load(session_id, cancellation.clone())
            .await?
            .ok_or_else(|| SessionQueryError::NotFound(session_id.clone()))?;
        check_cancellation(&cancellation)?;
        Ok(SessionRecord {
            events: self
                .persistence
                .read_from(session_id, 0, cancellation)
                .await?,
            header,
            live: false,
        })
    }

    /// Lists sessions in stable id order, with live state replacing stale durable reads.
    pub async fn list(
        &self,
        request: SessionListRequest,
        cancellation: CancellationToken,
    ) -> Result<SessionQueryPage<SessionRecord>, SessionQueryError> {
        let normalized = NormalizedRequest::list(&request)?;
        let records = self.all_records(cancellation.clone()).await?;
        let mut matches = Vec::new();
        for record in records {
            check_cancellation(&cancellation)?;
            if matches_session(&record, &request.filter) {
                matches.push(record);
            }
        }
        let revision = revision_for_records(&matches)?;
        let cursor = decode_cursor(request.cursor.as_deref(), &normalized, &revision)?;
        let after = cursor.map(|cursor| cursor.position);
        let limit = page_size(request.limit)?;
        let mut items = matches
            .into_iter()
            .filter(|record| {
                after
                    .as_ref()
                    .is_none_or(|position| record.header.id.as_str() > position.as_str())
            })
            .collect::<Vec<_>>();
        let next = if items.len() > limit {
            let position = items[limit - 1].header.id.as_str().to_owned();
            items.truncate(limit);
            Some(encode_cursor(&normalized, &revision, position)?)
        } else {
            None
        };
        Ok(SessionQueryPage {
            items,
            cursor: next,
        })
    }

    /// Reads matching events in sequence order from an exact session.
    pub async fn log(
        &self,
        session_id: &SessionId,
        request: SessionLogRequest,
        cancellation: CancellationToken,
    ) -> Result<SessionQueryPage<SessionEvent>, SessionQueryError> {
        let normalized = NormalizedRequest::log(session_id, &request)?;
        let record = self.read(session_id, cancellation.clone()).await?;
        let mut matches = Vec::new();
        for event in record.events {
            check_cancellation(&cancellation)?;
            if matches_event(&event, &request.filter) {
                matches.push(event);
            }
        }
        let revision = revision_for_events(&matches)?;
        let cursor = decode_cursor(request.cursor.as_deref(), &normalized, &revision)?;
        let after = cursor
            .map(|cursor| {
                cursor
                    .position
                    .parse::<u64>()
                    .map_err(|_| SessionQueryError::InvalidCursor)
            })
            .transpose()?;
        let limit = page_size(request.limit)?;
        let mut items = matches
            .into_iter()
            .filter(|event| after.is_none_or(|seq| event.seq > seq))
            .collect::<Vec<_>>();
        let next = if items.len() > limit {
            let position = items[limit - 1].seq.to_string();
            items.truncate(limit);
            Some(encode_cursor(&normalized, &revision, position)?)
        } else {
            None
        };
        Ok(SessionQueryPage {
            items,
            cursor: next,
        })
    }

    /// Rebuilds the visible surface without mutating or repairing the session log.
    pub async fn surface(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionSurfaceEntry>, SessionQueryError> {
        let record = self.read(session_id, cancellation.clone()).await?;
        let mut surface = Vec::new();
        for event in &record.events {
            check_cancellation(&cancellation)?;
            let Some(entry) = surface_entry(event)? else {
                continue;
            };
            match event.surface_op.as_ref() {
                Some(SurfaceOp::Append) => surface.push(entry),
                Some(SurfaceOp::Replace { start, end })
                    if start <= end && *end <= surface.len() as u64 =>
                {
                    surface.splice(*start as usize..*end as usize, std::iter::once(entry));
                }
                _ => return Err(SessionQueryError::InvalidSurface),
            }
        }
        Ok(surface)
    }

    /// Returns direct source and reverse replacement links around one event.
    pub async fn trace(
        &self,
        session_id: &SessionId,
        event_seq: u64,
        cancellation: CancellationToken,
    ) -> Result<SessionEventTrace, SessionQueryError> {
        let record = self.read(session_id, cancellation.clone()).await?;
        let event = record
            .events
            .iter()
            .find(|event| event.seq == event_seq)
            .cloned()
            .ok_or_else(|| {
                SessionQueryError::InvalidRequest("event sequence does not exist".into())
            })?;
        let by_seq = record
            .events
            .iter()
            .map(|candidate| (candidate.seq, candidate))
            .collect::<BTreeMap<_, _>>();
        let sources = event
            .source_event_seqs
            .iter()
            .flat_map(|sources| sources.iter())
            .filter_map(|seq| by_seq.get(seq).cloned().cloned())
            .collect();
        let replacements = record
            .events
            .iter()
            .filter(|candidate| {
                candidate
                    .source_event_seqs
                    .as_ref()
                    .is_some_and(|sources| sources.contains(&event_seq))
            })
            .cloned()
            .collect();
        Ok(SessionEventTrace {
            event,
            sources,
            replacements,
        })
    }

    async fn all_records(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionRecord>, SessionQueryError> {
        check_cancellation(&cancellation)?;
        let mut records = BTreeMap::new();
        for inspection in self.persistence.list(cancellation.clone()).await? {
            check_cancellation(&cancellation)?;
            let id = inspection.header.id.clone();
            let events = self
                .persistence
                .read_from(&id, 0, cancellation.clone())
                .await?;
            records.insert(
                id,
                SessionRecord {
                    header: inspection.header,
                    events,
                    live: false,
                },
            );
        }
        for session in self.store.list() {
            check_cancellation(&cancellation)?;
            records.insert(
                session.id(),
                SessionRecord {
                    header: session.header(),
                    events: session.events(),
                    live: true,
                },
            );
        }
        Ok(records.into_values().collect())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NormalizedRequest {
    operation: &'static str,
    session_id: Option<String>,
    text: Option<String>,
    event_types: Vec<String>,
    created_at: Option<InclusiveRange>,
    event_time: Option<InclusiveRange>,
    event_seq: Option<InclusiveRange>,
}

impl NormalizedRequest {
    fn list(request: &SessionListRequest) -> Result<String, SessionQueryError> {
        normalized("list", None, &request.filter)
    }

    fn log(
        session_id: &SessionId,
        request: &SessionLogRequest,
    ) -> Result<String, SessionQueryError> {
        normalized("log", Some(session_id.as_str().to_owned()), &request.filter)
    }
}

fn normalized(
    operation: &'static str,
    session_id: Option<String>,
    filter: &SessionQueryFilter,
) -> Result<String, SessionQueryError> {
    filter
        .created_at
        .map(InclusiveRange::validate)
        .transpose()?;
    filter
        .event_time
        .map(InclusiveRange::validate)
        .transpose()?;
    filter.event_seq.map(InclusiveRange::validate).transpose()?;
    let text = filter.text.as_deref().map(normalize_text).transpose()?;
    let mut event_types = filter.event_types.clone();
    event_types.sort();
    event_types.dedup();
    if event_types.iter().any(|kind| kind.is_empty()) {
        return Err(SessionQueryError::InvalidRequest(
            "event type must not be empty".into(),
        ));
    }
    serde_json::to_string(&NormalizedRequest {
        operation,
        session_id,
        text,
        event_types,
        created_at: filter.created_at,
        event_time: filter.event_time,
        event_seq: filter.event_seq,
    })
    .map_err(|_| SessionQueryError::InvalidRequest("request cannot be normalized".into()))
}

#[derive(Deserialize, Serialize)]
struct CursorPayload {
    request: String,
    revision: String,
    position: String,
}

fn encode_cursor(
    request: &str,
    revision: &str,
    position: String,
) -> Result<String, SessionQueryError> {
    let bytes = serde_json::to_vec(&CursorPayload {
        request: request.to_owned(),
        revision: revision.to_owned(),
        position,
    })
    .map_err(|_| SessionQueryError::InvalidCursor)?;
    Ok(format!("{}.{}", hex(&bytes), hash(&bytes)))
}

fn decode_cursor(
    value: Option<&str>,
    request: &str,
    revision: &str,
) -> Result<Option<CursorPayload>, SessionQueryError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let (encoded, signature) = value
        .split_once('.')
        .ok_or(SessionQueryError::InvalidCursor)?;
    if value.matches('.').count() != 1 {
        return Err(SessionQueryError::InvalidCursor);
    }
    let bytes = unhex(encoded).ok_or(SessionQueryError::InvalidCursor)?;
    if hash(&bytes) != signature {
        return Err(SessionQueryError::InvalidCursor);
    }
    let cursor = serde_json::from_slice::<CursorPayload>(&bytes)
        .map_err(|_| SessionQueryError::InvalidCursor)?;
    if cursor.request != request {
        return Err(SessionQueryError::CursorRequestMismatch);
    }
    if cursor.revision != revision {
        return Err(SessionQueryError::CursorStale);
    }
    Ok(Some(cursor))
}

fn matches_session(record: &SessionRecord, filter: &SessionQueryFilter) -> bool {
    if filter
        .created_at
        .is_some_and(|range| !range.contains(record.header.created_at))
    {
        return false;
    }
    if !has_event_clause(filter) {
        return true;
    }
    record
        .events
        .iter()
        .any(|event| matches_event(event, filter))
}

fn matches_event(event: &SessionEvent, filter: &SessionQueryFilter) -> bool {
    if !filter.event_types.is_empty()
        && !filter
            .event_types
            .iter()
            .any(|kind| kind == &event.event_type)
    {
        return false;
    }
    if filter
        .event_time
        .is_some_and(|range| !range.contains(event.time))
    {
        return false;
    }
    if filter
        .event_seq
        .is_some_and(|range| !range.contains(event.seq))
    {
        return false;
    }
    let Some(text) = filter.text.as_deref() else {
        return true;
    };
    let normalized = match normalize_text(text) {
        Ok(text) => text,
        Err(_) => return false,
    };
    semantic_text(&event.data)
        .into_iter()
        .map(|text| normalize_value(&text))
        .any(|candidate| candidate.contains(&normalized))
}

fn has_event_clause(filter: &SessionQueryFilter) -> bool {
    filter.text.is_some()
        || !filter.event_types.is_empty()
        || filter.event_time.is_some()
        || filter.event_seq.is_some()
}

fn semantic_text(value: &Value) -> Vec<String> {
    let mut result = Vec::new();
    collect_semantic_text(value, &mut result);
    result
}

fn collect_semantic_text(value: &Value, result: &mut Vec<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_semantic_text(value, result);
            }
        }
        Value::Object(object) => {
            let text_block = matches!(
                object.get("type").and_then(Value::as_str),
                Some("text" | "reasoning")
            );
            if text_block {
                if let Some(text) = object.get("text").and_then(Value::as_str) {
                    result.push(text.to_owned());
                }
                return;
            }
            for value in object.values() {
                collect_semantic_text(value, result);
            }
        }
        _ => {}
    }
}

fn normalize_text(value: &str) -> Result<String, SessionQueryError> {
    let value = normalize_value(value);
    if value.is_empty() {
        Err(SessionQueryError::InvalidRequest(
            "text filter must contain a non-whitespace character".into(),
        ))
    } else {
        Ok(value)
    }
}

fn normalize_value(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut previous_whitespace = true;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_whitespace() {
            if !previous_whitespace {
                output.push(' ');
            }
            previous_whitespace = true;
        } else {
            output.push(character);
            previous_whitespace = false;
        }
    }
    output.trim_end().to_owned()
}

fn surface_entry(event: &SessionEvent) -> Result<Option<SessionSurfaceEntry>, SessionQueryError> {
    let data = match event.event_type.as_str() {
        "user/message" => Some(event.data.clone()),
        "assistant/message" | "tool/result" => event.data.get("message").cloned(),
        _ => None,
    };
    let Some(data) = data else {
        return Ok(None);
    };
    let message = serde_json::from_value(data).map_err(|_| SessionQueryError::InvalidSurface)?;
    Ok(Some(SessionSurfaceEntry {
        event_seq: event.seq,
        message,
        source_event_seqs: event.source_event_seqs.clone(),
    }))
}

fn page_size(limit: Option<usize>) -> Result<usize, SessionQueryError> {
    match limit.unwrap_or(DEFAULT_PAGE_SIZE) {
        1..=MAX_PAGE_SIZE => Ok(limit.unwrap_or(DEFAULT_PAGE_SIZE)),
        _ => Err(SessionQueryError::InvalidRequest(format!(
            "limit must be within 1..={MAX_PAGE_SIZE}"
        ))),
    }
}

fn revision_for_records(records: &[SessionRecord]) -> Result<String, SessionQueryError> {
    let values = records
        .iter()
        .map(|record| json!({"header": record.header, "events": record.events}))
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&values)
        .map_err(|_| SessionQueryError::InvalidRequest("records cannot be fingerprinted".into()))?;
    Ok(hash(&bytes))
}

fn revision_for_events(events: &[SessionEvent]) -> Result<String, SessionQueryError> {
    let bytes = serde_json::to_vec(events)
        .map_err(|_| SessionQueryError::InvalidRequest("events cannot be fingerprinted".into()))?;
    Ok(hash(&bytes))
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), SessionQueryError> {
    if cancellation.is_cancelled() {
        Err(SessionQueryError::Cancelled)
    } else {
        Ok(())
    }
}

fn hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn unhex(value: &str) -> Option<Vec<u8>> {
    if value.is_empty() || !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some(hex_digit(pair[0])? << 4 | hex_digit(pair[1])?))
        .collect()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

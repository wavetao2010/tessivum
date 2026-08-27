//! Durable JSONL-backed [`SessionPersistence`] implementation.

use std::{
    collections::HashMap,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use crc32fast::hash as crc32;
use serde_json::{json, Value};
use tessivum_core::CancellationToken;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::Mutex as AsyncMutex,
};
use uuid::Uuid;

use crate::{
    error::TessivumError,
    protocol::{migrate_legacy_agent_preset_selection, SessionEvent, SessionHeader, SessionId},
    session::{SessionError, SessionInspection, SessionPersistence, SessionRawArtifact},
};

const RAW_SUFFIX: &str = ".jsonl";
const ZSTD_SUFFIX: &str = ".jsonl.zst";
const FILE_PREFIX: &str = "session-";
const FRAME_HEADER_LEN: usize = 8;

/// The on-disk encoding used for newly created session logs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JsonlStorageFormat {
    /// One UTF-8 JSON record per line, compatible with upstream session logs.
    #[default]
    Raw,
    /// Independently compressed and checksummed records in a `.jsonl.zst` file.
    Zstd,
}

/// Filesystem persistence for append-only session event logs.
///
/// Session IDs are hex-encoded before becoming path components, so opaque wire
/// identities can never select a path outside `root`.
#[derive(Clone, Debug)]
pub struct JsonlSessionPersistence {
    root: PathBuf,
    format: JsonlStorageFormat,
    gates: Arc<Mutex<HashMap<SessionId, Arc<AsyncMutex<()>>>>>,
    flush_counts: Arc<Mutex<HashMap<SessionId, u64>>>,
}

impl JsonlSessionPersistence {
    /// Creates raw JSONL persistence rooted at the caller-supplied directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_format(root, JsonlStorageFormat::Raw)
    }

    /// Creates zstd-framed JSONL persistence rooted at the caller-supplied directory.
    pub fn zstd(root: impl Into<PathBuf>) -> Self {
        Self::with_format(root, JsonlStorageFormat::Zstd)
    }

    /// Alias for [`Self::zstd`].
    pub fn compressed(root: impl Into<PathBuf>) -> Self {
        Self::zstd(root)
    }

    /// Creates persistence with the selected encoding for new logs.
    pub fn with_format(root: impl Into<PathBuf>, format: JsonlStorageFormat) -> Self {
        Self {
            root: root.into(),
            format,
            gates: Arc::new(Mutex::new(HashMap::new())),
            flush_counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns the root directory without normalizing caller-controlled paths.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the confined raw-JSONL path for an opaque session ID.
    pub fn raw_path(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(format!(
            "{FILE_PREFIX}{}{}",
            encode_id(session_id),
            RAW_SUFFIX
        ))
    }

    /// Returns the confined compressed-JSONL path for an opaque session ID.
    pub fn compressed_path(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(format!(
            "{FILE_PREFIX}{}{}",
            encode_id(session_id),
            ZSTD_SUFFIX
        ))
    }

    fn new_path(&self, session_id: &SessionId) -> PathBuf {
        match self.format {
            JsonlStorageFormat::Raw => self.raw_path(session_id),
            JsonlStorageFormat::Zstd => self.compressed_path(session_id),
        }
    }

    fn gate(&self, session_id: &SessionId) -> Arc<AsyncMutex<()>> {
        let mut gates = lock(&self.gates);
        gates
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    async fn existing_path(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(PathBuf, JsonlStorageFormat)>, SessionError> {
        let raw = self.raw_path(session_id);
        if path_exists(&raw).await? {
            return Ok(Some((raw, JsonlStorageFormat::Raw)));
        }
        let compressed = self.compressed_path(session_id);
        if path_exists(&compressed).await? {
            return Ok(Some((compressed, JsonlStorageFormat::Zstd)));
        }
        Ok(None)
    }

    async fn read_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<StoredSession>, SessionError> {
        let Some((path, format)) = self.existing_path(session_id).await? else {
            return Ok(None);
        };
        let bytes = fs::read(&path)
            .await
            .map_err(|error| io_error("read", &path, error))?;
        parse_session(&bytes, format, Some(session_id))
            .map(Some)
            .map_err(|error| session_parse_error(&path, error))
    }

    async fn write_new(&self, path: &Path, record: &[u8]) -> Result<(), SessionError> {
        fs::create_dir_all(&self.root)
            .await
            .map_err(|error| io_error("create root", &self.root, error))?;
        let temp = path.with_file_name(format!(
            ".{}-{}.tmp",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("session"),
            Uuid::new_v4()
        ));
        let result = async {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(&temp)
                .await
                .map_err(|error| io_error("create temporary log", &temp, error))?;
            file.write_all(record)
                .await
                .map_err(|error| io_error("write temporary log", &temp, error))?;
            file.sync_data()
                .await
                .map_err(|error| io_error("sync temporary log", &temp, error))?;
            fs::rename(&temp, path)
                .await
                .map_err(|error| io_error("rename temporary log", path, error))?;
            #[cfg(unix)]
            {
                let directory = fs::File::open(&self.root)
                    .await
                    .map_err(|error| io_error("open log directory", &self.root, error))?;
                directory
                    .sync_all()
                    .await
                    .map_err(|error| io_error("sync log directory", &self.root, error))?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temp).await;
        }
        result
    }
}

#[async_trait]
impl SessionPersistence for JsonlSessionPersistence {
    async fn create(
        &self,
        header: &SessionHeader,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        validate_header(header)?;
        let gate = self.gate(&header.id);
        let _guard = gate.lock().await;
        check_cancellation(&cancellation)?;
        if self.existing_path(&header.id).await?.is_some() {
            return Err(SessionError::AlreadyExists(header.id.clone()));
        }

        let record = match self.format {
            JsonlStorageFormat::Raw => header_record(header).into_bytes(),
            JsonlStorageFormat::Zstd => encode_frame(&header_record(header).into_bytes())?,
        };
        self.write_new(&self.new_path(&header.id), &record).await
    }

    async fn append(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancellation(&cancellation)?;
        event.validate()?;
        let gate = self.gate(session_id);
        let _guard = gate.lock().await;
        check_cancellation(&cancellation)?;
        let session = self
            .read_session(session_id)
            .await?
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))?;
        let expected = next_seq(&session.events)?;
        if event.seq != expected {
            return Err(SessionError::SequenceGap {
                expected,
                actual: event.seq,
            });
        }

        let (path, format) = self
            .existing_path(session_id)
            .await?
            .expect("read_session found an existing path");
        let record = event_record(event);
        let record = match format {
            JsonlStorageFormat::Raw => record.into_bytes(),
            JsonlStorageFormat::Zstd => encode_frame(&record.into_bytes())?,
        };
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .map_err(|error| io_error("open log for append", &path, error))?;
        file.write_all(&record)
            .await
            .map_err(|error| io_error("append log", &path, error))?;
        file.sync_data()
            .await
            .map_err(|error| io_error("sync appended log", &path, error))
    }

    fn supports_raw_artifacts(&self) -> bool {
        true
    }

    async fn read_raw(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionRawArtifact>, SessionError> {
        check_cancellation(&cancellation)?;
        let gate = self.gate(session_id);
        let _guard = gate.lock().await;
        check_cancellation(&cancellation)?;
        let Some((path, format)) = self.existing_path(session_id).await? else {
            return Ok(None);
        };
        let stored = fs::read(&path)
            .await
            .map_err(|error| io_error("read", &path, error))?;
        check_cancellation(&cancellation)?;
        parse_session(&stored, format, Some(session_id))
            .map_err(|error| session_parse_error(&path, error))?;
        let bytes = match format {
            JsonlStorageFormat::Raw => committed_raw_bytes(stored),
            JsonlStorageFormat::Zstd => {
                decode_zstd_raw(&stored).map_err(|error| log_error(&path, error))?
            }
        };
        check_cancellation(&cancellation)?;
        Ok(Some(SessionRawArtifact {
            filename: "session.jsonl".into(),
            bytes,
        }))
    }

    async fn load(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionHeader>, SessionError> {
        check_cancellation(&cancellation)?;
        Ok(self
            .read_session(session_id)
            .await?
            .map(|session| session.header))
    }

    async fn inspect(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionInspection>, SessionError> {
        check_cancellation(&cancellation)?;
        let Some(session) = self.read_session(session_id).await? else {
            return Ok(None);
        };
        let event_count =
            u64::try_from(session.events.len()).map_err(|_| SessionError::SequenceExhausted)?;
        Ok(Some(SessionInspection {
            header: session.header,
            event_count,
            next_seq: next_seq(&session.events)?,
            flush_count: *lock(&self.flush_counts).get(session_id).unwrap_or(&0),
        }))
    }

    async fn read_from(
        &self,
        session_id: &SessionId,
        from_seq: u64,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        check_cancellation(&cancellation)?;
        let session = self
            .read_session(session_id)
            .await?
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
        let gate = self.gate(session_id);
        let _guard = gate.lock().await;
        check_cancellation(&cancellation)?;
        let (path, _) = self
            .existing_path(session_id)
            .await?
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))?;
        let file = OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .map_err(|error| io_error("open log for flush", &path, error))?;
        file.sync_data()
            .await
            .map_err(|error| io_error("flush log", &path, error))?;
        let mut flush_counts = lock(&self.flush_counts);
        let count = flush_counts.entry(session_id.clone()).or_default();
        *count = count
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
        let gate = self.gate(session_id);
        let _guard = gate.lock().await;
        check_cancellation(&cancellation)?;
        let mut deleted = false;
        for path in [self.raw_path(session_id), self.compressed_path(session_id)] {
            match fs::remove_file(&path).await {
                Ok(()) => deleted = true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error("delete session log", &path, error)),
            }
        }
        if !deleted {
            return Err(SessionError::NotFound(session_id.clone()));
        }
        #[cfg(unix)]
        {
            let directory = fs::File::open(&self.root)
                .await
                .map_err(|error| io_error("open log directory", &self.root, error))?;
            directory
                .sync_all()
                .await
                .map_err(|error| io_error("sync log directory", &self.root, error))?;
        }
        lock(&self.flush_counts).remove(session_id);
        Ok(())
    }

    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SessionError> {
        check_cancellation(&cancellation)?;
        let mut directory = match fs::read_dir(&self.root).await {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error("list session logs", &self.root, error)),
        };
        let mut sessions = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|error| io_error("read session directory", &self.root, error))?
        {
            check_cancellation(&cancellation)?;
            let path = entry.path();
            let Some((id, format)) = id_from_path(&path) else {
                continue;
            };
            let bytes = fs::read(&path)
                .await
                .map_err(|error| io_error("read", &path, error))?;
            let session = parse_session(&bytes, format, Some(&id))
                .map_err(|error| session_parse_error(&path, error))?;
            let event_count =
                u64::try_from(session.events.len()).map_err(|_| SessionError::SequenceExhausted)?;
            sessions.push(SessionInspection {
                next_seq: next_seq(&session.events)?,
                header: session.header,
                event_count,
                flush_count: *lock(&self.flush_counts).get(&id).unwrap_or(&0),
            });
        }
        sessions.sort_by(|left, right| left.header.id.cmp(&right.header.id));
        sessions.dedup_by(|left, right| left.header.id == right.header.id);
        Ok(sessions)
    }
}

struct StoredSession {
    header: SessionHeader,
    events: Vec<SessionEvent>,
}

enum SessionParseError {
    Corrupt(String),
    Protocol(TessivumError),
}

impl From<String> for SessionParseError {
    fn from(message: String) -> Self {
        Self::Corrupt(message)
    }
}

fn parse_session(
    bytes: &[u8],
    format: JsonlStorageFormat,
    expected_id: Option<&SessionId>,
) -> Result<StoredSession, SessionParseError> {
    let records = match format {
        JsonlStorageFormat::Raw => parse_raw_records(bytes)?,
        JsonlStorageFormat::Zstd => parse_zstd_records(bytes)?,
    };
    let Some((header_record, event_records)) = records.split_first() else {
        return Err("session log has no header record".to_owned().into());
    };
    let header = parse_header(header_record)?;
    if header.id.as_str().is_empty() {
        return Err("session header ID is empty".to_owned().into());
    }
    header.validate().map_err(|error| error.to_string())?;
    if expected_id.is_some_and(|id| *id != header.id) {
        return Err("session header ID does not match its log path"
            .to_owned()
            .into());
    }

    let mut events = Vec::with_capacity(event_records.len());
    for record in event_records {
        let mut event = serde_json::from_slice::<SessionEvent>(record)
            .map_err(|error| format!("invalid session event: {error}"))?;
        migrate_legacy_agent_preset_selection(&mut event.event_type, &mut event.data)
            .map_err(SessionParseError::Protocol)?;
        event.validate().map_err(|error| error.to_string())?;
        let expected = next_seq(&events).map_err(|error| error.to_string())?;
        if event.seq != expected {
            return Err(format!(
                "session event sequence is not contiguous: expected {expected}, got {}",
                event.seq
            )
            .into());
        }
        events.push(event);
    }
    Ok(StoredSession { header, events })
}

fn parse_raw_records(bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut records = Vec::new();
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let record = &bytes[start..index];
        if record.is_empty() {
            return Err("empty JSONL record in committed prefix".into());
        }
        if serde_json::from_slice::<Value>(record).is_err() {
            return Err("malformed JSONL record in committed prefix".into());
        }
        records.push(record.to_vec());
        start = index + 1;
    }
    if start < bytes.len() {
        let final_record = &bytes[start..];
        if serde_json::from_slice::<Value>(final_record).is_ok() {
            records.push(final_record.to_vec());
        }
        // A non-newline-terminated malformed record is the only recoverable raw tail.
    }
    Ok(records)
}

fn parse_zstd_records(bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut records = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes.len() - offset < FRAME_HEADER_LEN {
            break;
        }
        let checksum = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let compressed_len =
            u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        offset += FRAME_HEADER_LEN;
        if bytes.len() - offset < compressed_len {
            break;
        }
        let compressed = &bytes[offset..offset + compressed_len];
        offset += compressed_len;
        if crc32(compressed) != checksum {
            return Err("compressed frame checksum mismatch".into());
        }
        let record = zstd::stream::decode_all(Cursor::new(compressed))
            .map_err(|error| format!("invalid compressed frame: {error}"))?;
        if record.last() != Some(&b'\n') || record[..record.len() - 1].contains(&b'\n') {
            return Err("compressed frame does not contain exactly one JSONL record".into());
        }
        let record = &record[..record.len() - 1];
        if record.is_empty() || serde_json::from_slice::<Value>(record).is_err() {
            return Err("malformed JSONL record in compressed frame".into());
        }
        records.push(record.to_vec());
    }
    Ok(records)
}

fn committed_raw_bytes(mut bytes: Vec<u8>) -> Vec<u8> {
    let start = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    if start < bytes.len() && serde_json::from_slice::<Value>(&bytes[start..]).is_err() {
        bytes.truncate(start);
    }
    bytes
}

fn decode_zstd_raw(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut raw = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes.len() - offset < FRAME_HEADER_LEN {
            break;
        }
        let checksum = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let compressed_len =
            u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        offset += FRAME_HEADER_LEN;
        if bytes.len() - offset < compressed_len {
            break;
        }
        let compressed = &bytes[offset..offset + compressed_len];
        offset += compressed_len;
        if crc32(compressed) != checksum {
            return Err("compressed frame checksum mismatch".into());
        }
        let record = zstd::stream::decode_all(Cursor::new(compressed))
            .map_err(|error| format!("invalid compressed frame: {error}"))?;
        if record.last() != Some(&b'\n') || record[..record.len() - 1].contains(&b'\n') {
            return Err("compressed frame does not contain exactly one JSONL record".into());
        }
        raw.extend_from_slice(&record);
    }
    Ok(raw)
}

fn parse_header(record: &[u8]) -> Result<SessionHeader, SessionParseError> {
    let mut value: Value = serde_json::from_slice(record)
        .map_err(|error| format!("invalid session header: {error}"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "session header must be a JSON object".to_owned())?;
    match object.remove("type") {
        Some(Value::String(kind)) if kind == "session" => {}
        _ => {
            return Err("first JSONL record must have type=session"
                .to_owned()
                .into())
        }
    }
    SessionHeader::from_json_value(value).map_err(|error| {
        if error.code == "MODE_MIGRATION_REQUIRED" {
            SessionParseError::Protocol(error)
        } else {
            SessionParseError::Corrupt(format!("invalid session header: {error}"))
        }
    })
}

fn header_record(header: &SessionHeader) -> String {
    let serialized = serde_json::to_string(header).expect("SessionHeader serializes");
    format!("{{\"type\":\"session\",{}\n", &serialized[1..])
}

fn event_record(event: &SessionEvent) -> String {
    let mut serialized = serde_json::to_string(event).expect("SessionEvent serializes");
    serialized.push('\n');
    serialized
}

fn encode_frame(record: &[u8]) -> Result<Vec<u8>, SessionError> {
    let compressed = zstd::stream::encode_all(Cursor::new(record), 0)
        .map_err(|error| persistence_error("encode compressed frame", error.to_string()))?;
    let length = u32::try_from(compressed.len())
        .map_err(|_| persistence_error("encode compressed frame", "frame exceeds u32 length"))?;
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + compressed.len());
    frame.extend_from_slice(&crc32(&compressed).to_be_bytes());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&compressed);
    Ok(frame)
}

fn validate_header(header: &SessionHeader) -> Result<(), SessionError> {
    header.validate()?;
    if header.id.as_str().is_empty() {
        return Err(SessionError::EmptySessionId);
    }
    Ok(())
}

fn next_seq(events: &[SessionEvent]) -> Result<u64, SessionError> {
    events
        .last()
        .map(|event| {
            event
                .seq
                .checked_add(1)
                .ok_or(SessionError::SequenceExhausted)
        })
        .unwrap_or(Ok(0))
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), SessionError> {
    if cancellation.is_cancelled() {
        Err(SessionError::Cancelled)
    } else {
        Ok(())
    }
}

async fn path_exists(path: &Path) -> Result<bool, SessionError> {
    fs::try_exists(path)
        .await
        .map_err(|error| io_error("inspect log path", path, error))
}

fn encode_id(session_id: &SessionId) -> String {
    let mut encoded = String::with_capacity(session_id.as_str().len() * 2);
    for byte in session_id.as_str().bytes() {
        use std::fmt::Write;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn id_from_path(path: &Path) -> Option<(SessionId, JsonlStorageFormat)> {
    let name = path.file_name()?.to_str()?.strip_prefix(FILE_PREFIX)?;
    let (encoded, format) = if let Some(encoded) = name.strip_suffix(ZSTD_SUFFIX) {
        (encoded, JsonlStorageFormat::Zstd)
    } else {
        (name.strip_suffix(RAW_SUFFIX)?, JsonlStorageFormat::Raw)
    };
    decode_id(encoded).map(|id| (SessionId::from(id), format))
}

fn decode_id(encoded: &str) -> Option<String> {
    if encoded.is_empty() || !encoded.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        bytes.push(high << 4 | low);
    }
    String::from_utf8(bytes).ok()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn session_parse_error(path: &Path, error: SessionParseError) -> SessionError {
    match error {
        SessionParseError::Corrupt(message) => log_error(path, message),
        SessionParseError::Protocol(error) => SessionError::Protocol(error),
    }
}
fn io_error(operation: &str, path: &Path, error: std::io::Error) -> SessionError {
    persistence_error(operation, format!("{}: {error}", path.display()))
}

fn log_error(path: &Path, message: String) -> SessionError {
    SessionError::Protocol(TessivumError::new(
        "SESSION_LOG_CORRUPT",
        message,
        "persistence",
        json!({"path": path.display().to_string()}),
    ))
}

fn persistence_error(operation: &str, message: impl Into<String>) -> SessionError {
    SessionError::Protocol(TessivumError::new(
        "SESSION_PERSISTENCE_IO",
        format!("{operation}: {}", message.into()),
        "persistence",
        Value::Null,
    ))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

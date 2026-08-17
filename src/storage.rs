//! Durable, registry-backed key/value storage domains.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tessivum_core::ServiceKey;
use thiserror::Error;
use tokio::sync::{broadcast, Mutex as AsyncMutex};

pub fn storage_service_key() -> ServiceKey {
    ServiceKey::new("harness.storage", "1")
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum StorageError {
    #[error("storage name is unsafe: {0}")]
    UnsafeName(String),
    #[error("storage backend is already registered: {0}")]
    DuplicateBackend(String),
    #[error("storage backend is not registered: {0}")]
    BackendNotFound(String),
    #[error("storage domain version conflict: expected {expected}, found {actual}")]
    VersionConflict { expected: u64, actual: u64 },
    #[error("storage domain is closed")]
    Closed,
    #[error("storage persistence failed: {0}")]
    Persistence(String),
    #[error("storage value is not JSON compatible")]
    InvalidValue,
}
impl StorageError {
    pub fn code(&self) -> &str {
        match self {
            Self::UnsafeName(_) => "INVALID_STORAGE_NAME",
            Self::DuplicateBackend(_) => "DUPLICATE_STORAGE_BACKEND",
            Self::BackendNotFound(_) => "STORAGE_BACKEND_NOT_FOUND",
            Self::VersionConflict { .. } => "STORAGE_VERSION_CONFLICT",
            Self::Closed => "STORAGE_CLOSED",
            Self::Persistence(_) => "STORAGE_PERSISTENCE_FAILED",
            Self::InvalidValue => "INVALID_STORAGE_VALUE",
        }
    }
}

/// Durable representation consumed by a backend. Values are intentionally not
/// `Debug` so generic logging cannot reveal application data.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageDocument {
    pub version: u64,
    pub tables: BTreeMap<String, BTreeMap<String, Value>>,
}
impl fmt::Debug for StorageDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageDocument")
            .field("version", &self.version)
            .field("table_count", &self.tables.len())
            .finish()
    }
}

#[async_trait]
pub trait StorageBackend: Send + Sync {
    async fn load(&self, domain: &str) -> Result<Option<StorageDocument>, StorageError>;
    /// Must atomically replace the domain's complete durable representation.
    async fn persist(&self, domain: &str, document: &StorageDocument) -> Result<(), StorageError>;
    async fn shutdown(&self) -> Result<(), StorageError> {
        Ok(())
    }
}

/// Simple backend for tests and explicitly volatile deployments.
pub struct MemoryStorageBackend {
    documents: Mutex<BTreeMap<String, StorageDocument>>,
    writable: AtomicBool,
}
impl Default for MemoryStorageBackend {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Debug for MemoryStorageBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryStorageBackend")
            .field("domain_count", &lock(&self.documents).len())
            .field("writable", &self.writable.load(Ordering::Acquire))
            .finish()
    }
}
impl MemoryStorageBackend {
    pub fn new() -> Self {
        Self {
            documents: Mutex::new(BTreeMap::new()),
            writable: AtomicBool::new(true),
        }
    }
    pub fn set_writable(&self, writable: bool) {
        self.writable.store(writable, Ordering::Release);
    }
}
#[async_trait]
impl StorageBackend for MemoryStorageBackend {
    async fn load(&self, domain: &str) -> Result<Option<StorageDocument>, StorageError> {
        Ok(lock(&self.documents).get(domain).cloned())
    }
    async fn persist(&self, domain: &str, document: &StorageDocument) -> Result<(), StorageError> {
        if !self.writable.load(Ordering::Acquire) {
            return Err(StorageError::Persistence(
                "memory backend is read-only".into(),
            ));
        }
        lock(&self.documents).insert(domain.to_owned(), document.clone());
        Ok(())
    }
}

/// Registry ensures a caller gets one in-memory authoritative view for each
/// backend/domain pair rather than independently mutating stale snapshots.
pub struct StorageRegistry {
    backends: Mutex<BTreeMap<String, Arc<dyn StorageBackend>>>,
    domains: Mutex<BTreeMap<(String, String), Arc<KvDomain>>>,
    open_gate: AsyncMutex<()>,
    closed: AtomicBool,
}
impl fmt::Debug for StorageRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageRegistry")
            .field("backend_count", &lock(&self.backends).len())
            .field("domain_count", &lock(&self.domains).len())
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish()
    }
}
impl Default for StorageRegistry {
    fn default() -> Self {
        Self::new()
    }
}
impl StorageRegistry {
    pub fn new() -> Self {
        Self {
            backends: Mutex::new(BTreeMap::new()),
            domains: Mutex::new(BTreeMap::new()),
            open_gate: AsyncMutex::new(()),
            closed: AtomicBool::new(false),
        }
    }
    pub fn register_backend(
        &self,
        name: impl Into<String>,
        backend: Arc<dyn StorageBackend>,
    ) -> Result<(), StorageError> {
        let name = name.into();
        ensure_name(&name)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(StorageError::Closed);
        }
        let mut backends = lock(&self.backends);
        if backends.contains_key(&name) {
            return Err(StorageError::DuplicateBackend(name));
        }
        backends.insert(name, backend);
        Ok(())
    }
    pub async fn open(
        &self,
        backend_name: &str,
        domain: &str,
        version: u64,
    ) -> Result<Arc<KvDomain>, StorageError> {
        ensure_name(backend_name)?;
        ensure_name(domain)?;
        if version == 0 {
            return Err(StorageError::VersionConflict {
                expected: 1,
                actual: 0,
            });
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(StorageError::Closed);
        }
        let _gate = self.open_gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(StorageError::Closed);
        }
        let key = (backend_name.to_owned(), domain.to_owned());
        if let Some(existing) = lock(&self.domains).get(&key).cloned() {
            if existing.version() != version {
                return Err(StorageError::VersionConflict {
                    expected: version,
                    actual: existing.version(),
                });
            }
            return Ok(existing);
        }
        let backend = lock(&self.backends)
            .get(backend_name)
            .cloned()
            .ok_or_else(|| StorageError::BackendNotFound(backend_name.to_owned()))?;
        let document = backend.load(domain).await?.unwrap_or(StorageDocument {
            version,
            tables: BTreeMap::new(),
        });
        validate_document(&document)?;
        if document.version != version {
            return Err(StorageError::VersionConflict {
                expected: version,
                actual: document.version,
            });
        }
        let opened = Arc::new(KvDomain::new(domain.to_owned(), document, backend));
        lock(&self.domains).insert(key, Arc::clone(&opened));
        Ok(opened)
    }
    pub async fn shutdown(&self) -> Result<(), StorageError> {
        self.closed.store(true, Ordering::Release);
        let domains: Vec<_> = lock(&self.domains).values().cloned().collect();
        for domain in domains {
            domain.close().await?;
        }
        let mut unique = BTreeMap::<usize, Arc<dyn StorageBackend>>::new();
        for backend in lock(&self.backends).values() {
            unique
                .entry(Arc::as_ptr(backend) as *const () as usize)
                .or_insert_with(|| Arc::clone(backend));
        }
        for backend in unique.into_values() {
            backend.shutdown().await?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageOperation {
    Put,
    Remove,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageEvent {
    pub domain: String,
    pub table: String,
    pub unit: String,
    pub revision: u64,
    pub operation: StorageOperation,
}

/// Snapshot values are detached. Its custom Debug avoids revealing values.
pub struct StorageSnapshot {
    pub version: u64,
    pub revision: u64,
    pub tables: BTreeMap<String, BTreeMap<String, Value>>,
}
impl fmt::Debug for StorageSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageSnapshot")
            .field("version", &self.version)
            .field("revision", &self.revision)
            .field("table_count", &self.tables.len())
            .finish()
    }
}

struct DomainState {
    document: StorageDocument,
    revision: u64,
}
pub struct KvDomain {
    name: String,
    backend: Arc<dyn StorageBackend>,
    state: Mutex<DomainState>,
    write_gate: AsyncMutex<()>,
    updates: broadcast::Sender<StorageEvent>,
    accepting: AtomicBool,
}
impl fmt::Debug for KvDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("KvDomain")
            .field("name", &self.name)
            .field("version", &state.document.version)
            .field("revision", &state.revision)
            .field("closed", &!self.accepting.load(Ordering::Acquire))
            .finish()
    }
}
impl KvDomain {
    fn new(name: String, document: StorageDocument, backend: Arc<dyn StorageBackend>) -> Self {
        let (updates, _) = broadcast::channel(128);
        Self {
            name,
            backend,
            state: Mutex::new(DomainState {
                document,
                revision: 0,
            }),
            write_gate: AsyncMutex::new(()),
            updates,
            accepting: AtomicBool::new(true),
        }
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn version(&self) -> u64 {
        lock(&self.state).document.version
    }
    pub fn subscribe(&self) -> broadcast::Receiver<StorageEvent> {
        self.updates.subscribe()
    }
    pub fn get(&self, table: &str, unit: &str) -> Result<Option<Value>, StorageError> {
        ensure_name(table)?;
        ensure_name(unit)?;
        Ok(lock(&self.state)
            .document
            .tables
            .get(table)
            .and_then(|table| table.get(unit))
            .cloned())
    }
    pub fn snapshot(&self) -> StorageSnapshot {
        let state = lock(&self.state);
        StorageSnapshot {
            version: state.document.version,
            revision: state.revision,
            tables: state.document.tables.clone(),
        }
    }
    pub async fn put(&self, table: &str, unit: &str, value: Value) -> Result<(), StorageError> {
        ensure_name(table)?;
        ensure_name(unit)?;
        validate_value(&value)?;
        self.commit(table, unit, StorageOperation::Put, move |document| {
            document
                .tables
                .entry(table.to_owned())
                .or_default()
                .insert(unit.to_owned(), value);
        })
        .await
    }
    pub async fn remove(&self, table: &str, unit: &str) -> Result<(), StorageError> {
        ensure_name(table)?;
        ensure_name(unit)?;
        self.commit(table, unit, StorageOperation::Remove, move |document| {
            let remove_table = document.tables.get_mut(table).is_some_and(|units| {
                units.remove(unit);
                units.is_empty()
            });
            if remove_table {
                document.tables.remove(table);
            }
        })
        .await
    }
    pub async fn close(&self) -> Result<(), StorageError> {
        self.accepting.store(false, Ordering::Release);
        let _gate = self.write_gate.lock().await;
        Ok(())
    }
    async fn commit<F>(
        &self,
        table: &str,
        unit: &str,
        operation: StorageOperation,
        change: F,
    ) -> Result<(), StorageError>
    where
        F: FnOnce(&mut StorageDocument),
    {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(StorageError::Closed);
        }
        let _gate = self.write_gate.lock().await;
        if !self.accepting.load(Ordering::Acquire) {
            return Err(StorageError::Closed);
        }
        let (mut document, revision) = {
            let state = lock(&self.state);
            (state.document.clone(), state.revision)
        };
        change(&mut document);
        validate_document(&document)?;
        if document.tables == lock(&self.state).document.tables {
            return Ok(());
        }
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| StorageError::Persistence("revision exhausted".into()))?;
        self.backend.persist(&self.name, &document).await?;
        {
            let mut state = lock(&self.state);
            state.document = document;
            state.revision = next_revision;
        }
        let _ = self.updates.send(StorageEvent {
            domain: self.name.clone(),
            table: table.to_owned(),
            unit: unit.to_owned(),
            revision: next_revision,
            operation,
        });
        Ok(())
    }
}

fn ensure_name(name: &str) -> Result<(), StorageError> {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err(StorageError::UnsafeName(name.into()));
    };
    if !first.is_ascii_lowercase() {
        return Err(StorageError::UnsafeName(name.into()));
    }
    let mut separator = false;
    for byte in bytes {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            separator = false;
        } else if byte == b'-' && !separator {
            separator = true;
        } else {
            return Err(StorageError::UnsafeName(name.into()));
        }
    }
    if separator {
        Err(StorageError::UnsafeName(name.into()))
    } else {
        Ok(())
    }
}
fn validate_document(document: &StorageDocument) -> Result<(), StorageError> {
    if document.version == 0 {
        return Err(StorageError::InvalidValue);
    }
    for (table, units) in &document.tables {
        ensure_name(table)?;
        for (unit, value) in units {
            ensure_name(unit)?;
            validate_value(value)?;
        }
    }
    Ok(())
}
fn validate_value(value: &Value) -> Result<(), StorageError> {
    match value {
        Value::Array(values) => {
            for value in values {
                validate_value(value)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_value(value)?;
            }
        }
        Value::Number(number) if number.as_f64().is_some_and(|number| !number.is_finite()) => {
            return Err(StorageError::InvalidValue)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

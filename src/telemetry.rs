//! Fail-closed session telemetry capture with a bounded, explicitly drained queue.

use crate::{SessionEvent, SessionId};
use async_trait::async_trait;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, VecDeque},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
    },
};
use tessivum_core::{ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

pub fn telemetry_service_key() -> ServiceKey {
    ServiceKey::new("harness.telemetry", "1")
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TelemetryChannel {
    Ledger,
    Ops,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetrySeverity {
    Info,
    Warn,
    Error,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TelemetrySharing {
    Full,
    FeedbackOnly,
    Disabled,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryRecord {
    pub channel: TelemetryChannel,
    pub time: u64,
    pub severity: TelemetrySeverity,
    pub attributes: std::collections::BTreeMap<String, Value>,
    pub body: Value,
}
#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("telemetry queue capacity must be positive")]
    InvalidCapacity,
    #[error("telemetry redactor failed: {0}")]
    Redactor(String),
}

/// Redactor failures and panics withhold the individual record from the backend.
pub trait TelemetryRedactor: Send + Sync {
    fn redact(&self, record: TelemetryRecord) -> Result<TelemetryRecord, TelemetryError>;
}
#[async_trait]
pub trait TelemetryBackend: Send + Sync {
    fn sharing(&self) -> TelemetrySharing;
    async fn emit(&self, record: TelemetryRecord) -> Result<(), String>;
    fn flush_hint(&self) -> Result<(), String> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<(), String>;
}

#[derive(Clone)]
pub struct TelemetryCoordinator {
    inner: Arc<Inner>,
}
struct Inner {
    backend: Arc<dyn TelemetryBackend>,
    redactors: RwLock<Vec<Arc<dyn TelemetryRedactor>>>,
    state: Mutex<State>,
    drain_gate: AsyncMutex<()>,
    shutdown_gate: AsyncMutex<()>,
    failures: AtomicU64,
}
struct State {
    capacity: usize,
    queue: VecDeque<TelemetryRecord>,
    ledger_seen: BTreeSet<(String, u64)>,
    chunk_seen: BTreeSet<(String, u64, u64)>,
    closed: bool,
}
impl TelemetryCoordinator {
    pub fn new(
        backend: Arc<dyn TelemetryBackend>,
        capacity: usize,
    ) -> Result<Self, TelemetryError> {
        if capacity == 0 {
            return Err(TelemetryError::InvalidCapacity);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                backend,
                redactors: RwLock::new(Vec::new()),
                state: Mutex::new(State {
                    capacity,
                    queue: VecDeque::new(),
                    ledger_seen: BTreeSet::new(),
                    chunk_seen: BTreeSet::new(),
                    closed: false,
                }),
                drain_gate: AsyncMutex::new(()),
                shutdown_gate: AsyncMutex::new(()),
                failures: AtomicU64::new(0),
            }),
        })
    }
    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(telemetry_service_key(), self)
    }
    pub fn sharing(&self) -> TelemetrySharing {
        self.inner.backend.sharing()
    }
    pub fn failures(&self) -> u64 {
        self.inner.failures.load(Ordering::Acquire)
    }
    pub fn add_redactor(&self, redactor: Arc<dyn TelemetryRedactor>) {
        write(&self.inner.redactors).push(redactor);
    }
    /// Capture the exact event data through a redaction copy; source event remains unchanged.
    pub fn capture_event(&self, session: &SessionId, event: &SessionEvent) -> bool {
        let session_id = session.as_str().to_owned();
        let mut state = lock(&self.inner.state);
        if state.closed || !state.ledger_seen.insert((session_id.clone(), event.seq)) {
            return false;
        }
        if event.event_type == "assistant/chunk" {
            if let (Some(turn), Some(step)) = (
                event.data.get("turn").and_then(Value::as_u64),
                event.data.get("step").and_then(Value::as_u64),
            ) {
                if !state.chunk_seen.insert((session_id.clone(), turn, step)) {
                    return false;
                }
            }
        }
        drop(state);
        self.enqueue(
            TelemetryRecord {
                channel: TelemetryChannel::Ledger,
                time: event.time,
                severity: severity(event),
                attributes: attrs([
                    ("session.id", Value::String(session_id)),
                    ("event.type", Value::String(event.event_type.clone())),
                    ("event.seq", json!(event.seq)),
                ]),
                body: clone_json(&event.data).expect("session data is JSON"),
            },
            false,
        )
    }
    /// Operational events are intentionally never deduplicated.
    pub fn capture_ops(&self, record: TelemetryRecord) -> bool {
        if record.channel != TelemetryChannel::Ops {
            self.inner.failures.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.enqueue(record, false)
    }
    pub fn shutdown_marker(&self, session: SessionId) -> bool {
        self.enqueue(
            TelemetryRecord {
                channel: TelemetryChannel::Ops,
                time: now(),
                severity: TelemetrySeverity::Info,
                attributes: attrs([
                    ("telemetry.op", Value::String("shutdown".into())),
                    ("session.id", Value::String(session.into_inner())),
                ]),
                body: Value::Null,
            },
            true,
        )
    }
    /// Calls the backend synchronously only as a hint; errors cannot affect the caller.
    pub fn flush_hint(&self) {
        if catch_unwind(AssertUnwindSafe(|| self.inner.backend.flush_hint()))
            .ok()
            .and_then(Result::err)
            .is_some()
        {
            self.inner.failures.fetch_add(1, Ordering::Relaxed);
        }
    }
    /// Backend handoff happens outside every coordinator lock.
    pub async fn drain(&self) {
        let _gate = self.inner.drain_gate.lock().await;
        loop {
            let record = lock(&self.inner.state).queue.pop_front();
            let Some(record) = record else {
                return;
            };
            if !matches!(
                AssertUnwindSafe(self.inner.backend.emit(record))
                    .catch_unwind()
                    .await,
                Ok(Ok(()))
            ) {
                self.inner.failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    /// Enqueue markers first; records accepted during a prior flush or drain are included before close.
    pub async fn shutdown<I>(&self, sessions: I)
    where
        I: IntoIterator<Item = SessionId>,
    {
        let _shutdown = self.inner.shutdown_gate.lock().await;
        for session in sessions {
            self.shutdown_marker(session);
        }
        loop {
            self.drain().await;
            let mut state = lock(&self.inner.state);
            if state.queue.is_empty() {
                state.closed = true;
                break;
            }
        }
        if !matches!(
            AssertUnwindSafe(self.inner.backend.shutdown())
                .catch_unwind()
                .await,
            Ok(Ok(()))
        ) {
            self.inner.failures.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn enqueue(&self, record: TelemetryRecord, critical: bool) -> bool {
        let Some(record) = self.redact(record) else {
            return false;
        };
        let mut state = lock(&self.inner.state);
        if state.closed {
            return false;
        }
        if state.queue.len() == state.capacity {
            if !critical {
                return false;
            }
            state.queue.pop_front();
        }
        state.queue.push_back(record);
        true
    }
    fn redact(&self, record: TelemetryRecord) -> Option<TelemetryRecord> {
        let mut record = clone_json(&record).ok()?;
        for redactor in read(&self.inner.redactors).clone() {
            match catch_unwind(AssertUnwindSafe(|| {
                redactor.redact(clone_json(&record).expect("record is JSON"))
            })) {
                Ok(Ok(next)) => record = next,
                _ => {
                    self.inner.failures.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            }
        }
        Some(record)
    }
}
fn severity(event: &SessionEvent) -> TelemetrySeverity {
    if event.event_type == "agent/error" || event.data.get("isError") == Some(&Value::Bool(true)) {
        TelemetrySeverity::Error
    } else {
        TelemetrySeverity::Info
    }
}
fn attrs<const N: usize>(pairs: [(&str, Value); N]) -> std::collections::BTreeMap<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.into(), v)).collect()
}
fn clone_json<T: Serialize + for<'a> Deserialize<'a>>(value: &T) -> Result<T, TelemetryError> {
    serde_json::from_slice(
        &serde_json::to_vec(value).map_err(|e| TelemetryError::Redactor(e.to_string()))?,
    )
    .map_err(|e| TelemetryError::Redactor(e.to_string()))
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().try_into().unwrap_or(u64::MAX))
}
fn lock<T>(v: &Mutex<T>) -> MutexGuard<'_, T> {
    v.lock().unwrap_or_else(|e| e.into_inner())
}
fn read<T>(v: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    v.read().unwrap_or_else(|e| e.into_inner())
}
fn write<T>(v: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    v.write().unwrap_or_else(|e| e.into_inner())
}

//! Versioned, durable settings with redacted descriptors.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tessivum_core::ServiceKey;
use thiserror::Error;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::{broadcast, Mutex as AsyncMutex},
};
use uuid::Uuid;

use crate::TessivumError;

pub fn settings_service_key() -> ServiceKey {
    ServiceKey::new("harness.settings", "1")
}

pub type SettingPath = Vec<String>;
pub type SettingsValidator = Arc<dyn Fn(&Value) -> Result<(), TessivumError> + Send + Sync>;

#[async_trait]
pub trait SettingsProvider: Send + Sync {
    async fn load(&self, namespace: &str) -> Result<Option<Value>, SettingsError>;
    async fn persist(&self, namespace: &str, user: &Value) -> Result<(), SettingsError>;
    fn writable(&self) -> bool {
        true
    }
}

pub struct SettingsRegistration {
    pub namespace: String,
    pub schema: Value,
    pub defaults: Value,
    pub base: Value,
    pub secret_paths: Vec<SettingPath>,
    pub validator: Option<SettingsValidator>,
}

impl fmt::Debug for SettingsRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsRegistration")
            .field("namespace", &self.namespace)
            .field("secret_path_count", &self.secret_paths.len())
            .field("has_validator", &self.validator.is_some())
            .finish()
    }
}

impl SettingsRegistration {
    pub fn new(namespace: impl Into<String>, schema: Value, defaults: Value, base: Value) -> Self {
        Self {
            namespace: namespace.into(),
            schema,
            defaults,
            base,
            secret_paths: Vec::new(),
            validator: None,
        }
    }
    pub fn with_validator(mut self, validator: SettingsValidator) -> Self {
        self.validator = Some(validator);
        self
    }
    pub fn with_secret_paths(mut self, secret_paths: Vec<SettingPath>) -> Self {
        self.secret_paths = secret_paths;
        self
    }
}
#[derive(Clone, PartialEq)]
pub struct SettingsSnapshot {
    pub namespace: String,
    pub revision: u64,
    pub value: Value,
}
impl fmt::Debug for SettingsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsSnapshot")
            .field("namespace", &self.namespace)
            .field("revision", &self.revision)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDescriptor {
    pub namespace: String,
    pub revision: u64,
    pub schema: Value,
    pub defaults: Value,
    pub base: Value,
    pub user: Value,
    pub resolved: Value,
    pub secret_paths: Vec<SettingPath>,
    #[serde(skip)]
    pub secret_set: Vec<bool>,
    #[serde(skip)]
    pub user_present: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsEvent {
    pub namespace: String,
    pub revision: u64,
    pub kind: SettingsEventKind,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SettingsEventKind {
    Registered,
    Updated,
    Reloaded,
    Unregistered,
}

/// One path-addressed update applied atomically to a user settings section.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsPathOp {
    Set { path: SettingPath, value: Value },
    Unset { path: SettingPath },
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum SettingsError {
    #[error("settings namespace is invalid: {0}")]
    InvalidNamespace(String),
    #[error("settings namespace is already registered: {0}")]
    DuplicateNamespace(String),
    #[error("settings namespace is not registered: {0}")]
    NotRegistered(String),
    #[error("settings provider is read-only")]
    ReadOnly,
    #[error("settings service is closed")]
    Closed,
    #[error("settings revision conflict: expected {expected}, found {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("settings value must be a JSON object")]
    InvalidDocument,
    #[error("settings path is invalid")]
    InvalidPath,
    #[error("settings revision space is exhausted")]
    RevisionExhausted,
    #[error("settings persistence failed: {0}")]
    Persistence(String),
    #[error(transparent)]
    Validation(#[from] TessivumError),
}

impl SettingsError {
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidNamespace(_) => "INVALID_SETTINGS_NAMESPACE",
            Self::DuplicateNamespace(_) => "DUPLICATE_SETTINGS_NAMESPACE",
            Self::NotRegistered(_) => "SETTINGS_NOT_REGISTERED",
            Self::ReadOnly => "SETTINGS_READ_ONLY",
            Self::Closed => "SETTINGS_CLOSED",
            Self::Conflict { .. } => "SETTINGS_CONFLICT",
            Self::InvalidDocument => "INVALID_SETTINGS_DOCUMENT",
            Self::InvalidPath => "INVALID_SETTINGS_PATH",
            Self::RevisionExhausted => "SETTINGS_REVISION_EXHAUSTED",
            Self::Persistence(_) => "SETTINGS_PERSISTENCE_FAILED",
            Self::Validation(error) => &error.code,
        }
    }
    pub fn as_tessivum_error(&self) -> TessivumError {
        TessivumError::new(
            self.code(),
            self.to_string(),
            "settings",
            match self {
                Self::Conflict { expected, actual } => {
                    json!({"expectedRevision": expected, "actualRevision": actual})
                }
                _ => Value::Null,
            },
        )
    }
}

struct NamespaceState {
    registration: SettingsRegistration,
    data: Mutex<NamespaceData>,
    gate: AsyncMutex<()>,
}
struct NamespaceData {
    user: Value,
    user_present: bool,
    resolved: Value,
    revision: u64,
}

pub struct Settings {
    provider: Arc<dyn SettingsProvider>,
    namespaces: Mutex<BTreeMap<String, Arc<NamespaceState>>>,
    updates: broadcast::Sender<SettingsEvent>,
    closed: AtomicBool,
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("namespace_count", &lock(&self.namespaces).len())
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish()
    }
}

impl Settings {
    pub fn new(provider: Arc<dyn SettingsProvider>) -> Self {
        let (updates, _) = broadcast::channel(128);
        Self {
            provider,
            namespaces: Mutex::new(BTreeMap::new()),
            updates,
            closed: AtomicBool::new(false),
        }
    }

    pub async fn register(&self, registration: SettingsRegistration) -> Result<(), SettingsError> {
        ensure_namespace(&registration.namespace)?;
        validate_document(&registration.defaults)?;
        validate_document(&registration.base)?;
        validate_secret_paths(&registration.secret_paths)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(SettingsError::Closed);
        }
        if lock(&self.namespaces).contains_key(&registration.namespace) {
            return Err(SettingsError::DuplicateNamespace(registration.namespace));
        }
        let loaded_user = self.provider.load(&registration.namespace).await?;
        let user_present = loaded_user.is_some();
        let user = loaded_user.unwrap_or_else(empty_document);
        validate_document(&user)?;
        let resolved = resolve(&registration.defaults, &registration.base, &user);
        validate_resolved(&registration, &resolved)?;
        let state = Arc::new(NamespaceState {
            registration,
            data: Mutex::new(NamespaceData {
                user,
                user_present,
                resolved,
                revision: 0,
            }),
            gate: AsyncMutex::new(()),
        });
        let mut namespaces = lock(&self.namespaces);
        if self.closed.load(Ordering::Acquire) {
            return Err(SettingsError::Closed);
        }
        if namespaces.contains_key(&state.registration.namespace) {
            return Err(SettingsError::DuplicateNamespace(
                state.registration.namespace.clone(),
            ));
        }
        let namespace = state.registration.namespace.clone();
        namespaces.insert(namespace.clone(), state);
        let _ = self.updates.send(SettingsEvent {
            namespace,
            revision: 0,
            kind: SettingsEventKind::Registered,
        });
        Ok(())
    }

    pub fn get(&self, namespace: &str) -> Result<SettingsSnapshot, SettingsError> {
        let state = self.state(namespace)?;
        let data = lock(&state.data);
        Ok(SettingsSnapshot {
            namespace: namespace.to_owned(),
            revision: data.revision,
            value: data.resolved.clone(),
        })
    }
    pub fn writable(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.provider.writable()
    }
    pub fn describe_all(&self) -> Result<Vec<SettingsDescriptor>, SettingsError> {
        let namespaces = lock(&self.namespaces).keys().cloned().collect::<Vec<_>>();
        namespaces
            .into_iter()
            .map(|namespace| self.describe(&namespace))
            .collect()
    }
    pub fn user(&self, namespace: &str) -> Result<Value, SettingsError> {
        Ok(lock(&self.state(namespace)?.data).user.clone())
    }
    pub fn describe(&self, namespace: &str) -> Result<SettingsDescriptor, SettingsError> {
        let state = self.state(namespace)?;
        let data = lock(&state.data);
        let secret_paths = state.registration.secret_paths.clone();
        Ok(SettingsDescriptor {
            namespace: namespace.to_owned(),
            revision: data.revision,
            schema: state.registration.schema.clone(),
            defaults: redact(&state.registration.defaults, &secret_paths),
            base: redact(&state.registration.base, &secret_paths),
            user: redact(&data.user, &secret_paths),
            resolved: redact(&data.resolved, &secret_paths),
            secret_set: secret_paths
                .iter()
                .map(|path| has_path(&data.resolved, path))
                .collect(),
            secret_paths,
            user_present: data.user_present,
        })
    }
    pub fn subscribe(&self) -> broadcast::Receiver<SettingsEvent> {
        self.updates.subscribe()
    }

    pub async fn update(
        &self,
        namespace: &str,
        patch: Value,
        expected_revision: Option<u64>,
    ) -> Result<SettingsSnapshot, SettingsError> {
        validate_document(&patch)?;
        self.commit(
            namespace,
            expected_revision,
            SettingsEventKind::Updated,
            move |user| merge(user, &patch),
        )
        .await
    }
    pub async fn replace(
        &self,
        namespace: &str,
        user: Value,
        expected_revision: Option<u64>,
    ) -> Result<SettingsSnapshot, SettingsError> {
        validate_document(&user)?;
        self.commit(
            namespace,
            expected_revision,
            SettingsEventKind::Updated,
            move |_| user,
        )
        .await
    }
    pub async fn set_path(
        &self,
        namespace: &str,
        path: SettingPath,
        value: Value,
        expected_revision: Option<u64>,
    ) -> Result<SettingsSnapshot, SettingsError> {
        validate_path(&path)?;
        validate_json(&value)?;
        self.commit(
            namespace,
            expected_revision,
            SettingsEventKind::Updated,
            move |mut user| {
                set_at_path(&mut user, &path, value);
                user
            },
        )
        .await
    }
    pub async fn remove_path(
        &self,
        namespace: &str,
        path: SettingPath,
        expected_revision: Option<u64>,
    ) -> Result<SettingsSnapshot, SettingsError> {
        validate_path(&path)?;
        self.commit(
            namespace,
            expected_revision,
            SettingsEventKind::Updated,
            move |mut user| {
                remove_at_path(&mut user, &path);
                user
            },
        )
        .await
    }
    pub async fn mutate(
        &self,
        namespace: &str,
        ops: Vec<SettingsPathOp>,
        expected_revision: Option<u64>,
    ) -> Result<SettingsSnapshot, SettingsError> {
        let mut root_is_document = true;
        for op in &ops {
            let path = match op {
                SettingsPathOp::Set { path, value } => {
                    validate_path_or_root(path)?;
                    validate_json(value)?;
                    path
                }
                SettingsPathOp::Unset { path } => {
                    validate_path_or_root(path)?;
                    path
                }
            };
            if path.is_empty() {
                root_is_document = match op {
                    SettingsPathOp::Set { value, .. } => value.is_object(),
                    SettingsPathOp::Unset { .. } => true,
                };
            } else if !root_is_document {
                return Err(SettingsError::InvalidDocument);
            }
        }
        self.commit(
            namespace,
            expected_revision,
            SettingsEventKind::Updated,
            move |mut user| {
                for op in ops {
                    match op {
                        SettingsPathOp::Set { path, value } if path.is_empty() => user = value,
                        SettingsPathOp::Set { path, value } => set_at_path(&mut user, &path, value),
                        SettingsPathOp::Unset { path } if path.is_empty() => user = empty_document(),
                        SettingsPathOp::Unset { path } => remove_at_path(&mut user, &path),
                    }
                }
                user
            },
        )
        .await
    }

    /// Detaches one owner while preserving its durable user section for a later owner.
    pub async fn unregister(&self, namespace: &str) -> Result<(), SettingsError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SettingsError::Closed);
        }
        let state = self.state(namespace)?;
        let _gate = state.gate.lock().await;
        let revision = lock(&state.data).revision;
        if lock(&self.namespaces).remove(namespace).is_none() {
            return Err(SettingsError::NotRegistered(namespace.to_owned()));
        }
        let _ = self.updates.send(SettingsEvent {
            namespace: namespace.to_owned(),
            revision,
            kind: SettingsEventKind::Unregistered,
        });
        Ok(())
    }

    pub async fn reload(&self, namespace: &str) -> Result<SettingsSnapshot, SettingsError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SettingsError::Closed);
        }
        let state = self.state(namespace)?;
        let _gate = state.gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(SettingsError::Closed);
        }
        let loaded_user = self.provider.load(namespace).await?;
        let user_present = loaded_user.is_some();
        let user = loaded_user.unwrap_or_else(empty_document);
        validate_document(&user)?;
        let resolved = resolve(
            &state.registration.defaults,
            &state.registration.base,
            &user,
        );
        validate_resolved(&state.registration, &resolved)?;
        let (snapshot, event) = {
            let mut data = lock(&state.data);
            if data.user == user && data.user_present == user_present {
                return Ok(SettingsSnapshot {
                    namespace: namespace.to_owned(),
                    revision: data.revision,
                    value: data.resolved.clone(),
                });
            }
            data.revision = data
                .revision
                .checked_add(1)
                .ok_or(SettingsError::RevisionExhausted)?;
            data.user = user;
            data.user_present = user_present;
            data.resolved = resolved;
            (
                SettingsSnapshot {
                    namespace: namespace.to_owned(),
                    revision: data.revision,
                    value: data.resolved.clone(),
                },
                SettingsEvent {
                    namespace: namespace.to_owned(),
                    revision: data.revision,
                    kind: SettingsEventKind::Reloaded,
                },
            )
        };
        let _ = self.updates.send(event);
        Ok(snapshot)
    }

    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        let states: Vec<_> = lock(&self.namespaces).values().cloned().collect();
        for state in states {
            let _gate = state.gate.lock().await;
        }
    }

    async fn commit<F>(
        &self,
        namespace: &str,
        expected_revision: Option<u64>,
        kind: SettingsEventKind,
        change: F,
    ) -> Result<SettingsSnapshot, SettingsError>
    where
        F: FnOnce(Value) -> Value,
    {
        if self.closed.load(Ordering::Acquire) {
            return Err(SettingsError::Closed);
        }
        if !self.provider.writable() {
            return Err(SettingsError::ReadOnly);
        }
        let state = self.state(namespace)?;
        let _gate = state.gate.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(SettingsError::Closed);
        }
        let (current_user, revision) = {
            let data = lock(&state.data);
            (data.user.clone(), data.revision)
        };
        if let Some(expected) = expected_revision {
            if expected != revision {
                return Err(SettingsError::Conflict {
                    expected,
                    actual: revision,
                });
            }
        }
        let candidate = change(current_user.clone());
        validate_document(&candidate)?;
        let resolved = resolve(
            &state.registration.defaults,
            &state.registration.base,
            &candidate,
        );
        validate_resolved(&state.registration, &resolved)?;
        if candidate == current_user {
            return Ok(SettingsSnapshot {
                namespace: namespace.to_owned(),
                revision,
                value: lock(&state.data).resolved.clone(),
            });
        }
        let next_revision = revision
            .checked_add(1)
            .ok_or(SettingsError::RevisionExhausted)?;
        self.provider.persist(namespace, &candidate).await?;
        let snapshot = {
            let mut data = lock(&state.data);
            data.user = candidate;
            data.resolved = resolved;
            data.revision = next_revision;
            data.user_present = true;
            SettingsSnapshot {
                namespace: namespace.to_owned(),
                revision: data.revision,
                value: data.resolved.clone(),
            }
        };
        let _ = self.updates.send(SettingsEvent {
            namespace: namespace.to_owned(),
            revision: next_revision,
            kind,
        });
        Ok(snapshot)
    }
    fn state(&self, namespace: &str) -> Result<Arc<NamespaceState>, SettingsError> {
        lock(&self.namespaces)
            .get(namespace)
            .cloned()
            .ok_or_else(|| SettingsError::NotRegistered(namespace.to_owned()))
    }
}
pub struct MemorySettingsProvider {
    sections: Mutex<BTreeMap<String, Value>>,
    writable: AtomicBool,
}
impl Default for MemorySettingsProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Debug for MemorySettingsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySettingsProvider")
            .field("section_count", &lock(&self.sections).len())
            .field("writable", &self.writable.load(Ordering::Acquire))
            .finish()
    }
}
impl MemorySettingsProvider {
    pub fn new() -> Self {
        Self {
            sections: Mutex::new(BTreeMap::new()),
            writable: AtomicBool::new(true),
        }
    }
    pub fn set_writable(&self, writable: bool) {
        self.writable.store(writable, Ordering::Release);
    }
    pub fn insert(&self, namespace: impl Into<String>, value: Value) -> Result<(), SettingsError> {
        validate_document(&value)?;
        lock(&self.sections).insert(namespace.into(), value);
        Ok(())
    }
}
#[async_trait]
impl SettingsProvider for MemorySettingsProvider {
    async fn load(&self, namespace: &str) -> Result<Option<Value>, SettingsError> {
        Ok(lock(&self.sections).get(namespace).cloned())
    }
    async fn persist(&self, namespace: &str, user: &Value) -> Result<(), SettingsError> {
        if !self.writable() {
            return Err(SettingsError::ReadOnly);
        }
        lock(&self.sections).insert(namespace.to_owned(), user.clone());
        Ok(())
    }
    fn writable(&self) -> bool {
        self.writable.load(Ordering::Acquire)
    }
}

pub struct YamlSettingsProvider {
    path: PathBuf,
    last_good: Mutex<BTreeMap<String, Value>>,
    writable: bool,
}
impl fmt::Debug for YamlSettingsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("YamlSettingsProvider")
            .field("path", &self.path)
            .field("writable", &self.writable)
            .finish()
    }
}
impl YamlSettingsProvider {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            last_good: Mutex::new(BTreeMap::new()),
            writable: true,
        }
    }
    pub fn read_only(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            last_good: Mutex::new(BTreeMap::new()),
            writable: false,
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    async fn read_document(&self) -> Result<BTreeMap<String, Value>, SettingsError> {
        let text = match fs::read_to_string(&self.path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let document = BTreeMap::new();
                *lock(&self.last_good) = document.clone();
                return Ok(document);
            }
            Err(error) => {
                return Err(SettingsError::Persistence(format!(
                    "read settings YAML: {error}"
                )))
            }
        };
        let document: BTreeMap<String, Value> = serde_yaml::from_str(&text)
            .map_err(|error| SettingsError::Persistence(format!("parse settings YAML: {error}")))?;
        for (namespace, value) in &document {
            ensure_namespace(namespace)?;
            validate_document(value)?;
        }
        *lock(&self.last_good) = document.clone();
        Ok(document)
    }
    async fn write_document(
        &self,
        document: &BTreeMap<String, Value>,
    ) -> Result<(), SettingsError> {
        let bytes = serde_yaml::to_string(document)
            .map_err(|error| SettingsError::Persistence(format!("encode settings YAML: {error}")))?
            .into_bytes();
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).await.map_err(|error| {
            SettingsError::Persistence(format!("create settings directory: {error}"))
        })?;
        let filename = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("settings.yaml");
        let temporary = parent.join(format!(".{filename}-{}.tmp", Uuid::new_v4()));
        let result = async {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).await.map_err(|error| {
                SettingsError::Persistence(format!("create settings temporary: {error}"))
            })?;
            file.write_all(&bytes).await.map_err(|error| {
                SettingsError::Persistence(format!("write settings temporary: {error}"))
            })?;
            file.sync_all().await.map_err(|error| {
                SettingsError::Persistence(format!("sync settings temporary: {error}"))
            })?;
            fs::rename(&temporary, &self.path).await.map_err(|error| {
                SettingsError::Persistence(format!("rename settings temporary: {error}"))
            })
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temporary).await;
        }
        result
    }
}
#[async_trait]
impl SettingsProvider for YamlSettingsProvider {
    async fn load(&self, namespace: &str) -> Result<Option<Value>, SettingsError> {
        match self.read_document().await {
            Ok(document) => Ok(document.get(namespace).cloned()),
            Err(error @ SettingsError::Persistence(_)) => lock(&self.last_good)
                .get(namespace)
                .cloned()
                .map(Some)
                .ok_or(error),
            Err(error) => Err(error),
        }
    }
    async fn persist(&self, namespace: &str, user: &Value) -> Result<(), SettingsError> {
        if !self.writable {
            return Err(SettingsError::ReadOnly);
        }
        ensure_namespace(namespace)?;
        validate_document(user)?;
        let mut document = self.read_document().await?;
        document.insert(namespace.to_owned(), user.clone());
        self.write_document(&document).await?;
        *lock(&self.last_good) = document;
        Ok(())
    }
    fn writable(&self) -> bool {
        self.writable
    }
}

fn ensure_namespace(namespace: &str) -> Result<(), SettingsError> {
    let mut bytes = namespace.bytes();
    let Some(first) = bytes.next() else {
        return Err(SettingsError::InvalidNamespace(namespace.to_owned()));
    };
    if !first.is_ascii_lowercase() {
        return Err(SettingsError::InvalidNamespace(namespace.to_owned()));
    }
    let mut separator = false;
    for byte in bytes {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            separator = false;
        } else if byte == b'-' && !separator {
            separator = true;
        } else {
            return Err(SettingsError::InvalidNamespace(namespace.to_owned()));
        }
    }
    if separator {
        return Err(SettingsError::InvalidNamespace(namespace.to_owned()));
    }
    Ok(())
}
fn validate_secret_paths(paths: &[SettingPath]) -> Result<(), SettingsError> {
    let mut seen = BTreeSet::new();
    for path in paths {
        validate_path(path)?;
        if !seen.insert(path.clone()) {
            return Err(SettingsError::InvalidPath);
        }
    }
    Ok(())
}
fn validate_path(path: &[String]) -> Result<(), SettingsError> {
    if path.is_empty()
        || path
            .iter()
            .any(|member| member.is_empty() || member.contains('\0'))
    {
        Err(SettingsError::InvalidPath)
    } else {
        Ok(())
    }
}
fn validate_path_or_root(path: &[String]) -> Result<(), SettingsError> {
    if path.is_empty() {
        Ok(())
    } else {
        validate_path(path)
    }
}

fn has_path(value: &Value, path: &[String]) -> bool {
    let mut current = value;
    for member in path {
        let Some(next) = current.as_object().and_then(|object| object.get(member)) else {
            return false;
        };
        current = next;
    }
    true
}
fn validate_document(value: &Value) -> Result<(), SettingsError> {
    if !value.is_object() {
        return Err(SettingsError::InvalidDocument);
    }
    validate_json(value)
}
fn validate_json(value: &Value) -> Result<(), SettingsError> {
    match value {
        Value::Array(values) => {
            for value in values {
                validate_json(value)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_json(value)?;
            }
        }
        Value::Number(number) if number.as_f64().is_some_and(|number| !number.is_finite()) => {
            return Err(SettingsError::InvalidDocument)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}
fn validate_resolved(
    registration: &SettingsRegistration,
    resolved: &Value,
) -> Result<(), SettingsError> {
    validate_document(resolved)?;
    if let Some(validator) = &registration.validator {
        validator(resolved)?;
    }
    Ok(())
}
fn empty_document() -> Value {
    Value::Object(Map::new())
}
fn resolve(defaults: &Value, base: &Value, user: &Value) -> Value {
    merge(merge(defaults.clone(), base), user)
}
fn merge(mut left: Value, right: &Value) -> Value {
    if let (Value::Object(left_object), Value::Object(right_object)) = (&mut left, right) {
        for (key, right_value) in right_object {
            let value = left_object
                .remove(key)
                .map(|left_value| merge(left_value, right_value))
                .unwrap_or_else(|| right_value.clone());
            left_object.insert(key.clone(), value);
        }
        left
    } else {
        right.clone()
    }
}
fn set_at_path(document: &mut Value, path: &[String], value: Value) {
    let mut current = document
        .as_object_mut()
        .expect("validated settings document");
    for member in &path[..path.len() - 1] {
        let next = current.entry(member.clone()).or_insert_with(empty_document);
        if !next.is_object() {
            *next = empty_document();
        }
        current = next.as_object_mut().expect("object inserted above");
    }
    current.insert(path.last().expect("validated nonempty path").clone(), value);
}
fn remove_at_path(document: &mut Value, path: &[String]) {
    let mut current = match document.as_object_mut() {
        Some(current) => current,
        None => return,
    };
    for member in &path[..path.len() - 1] {
        let Some(next) = current.get_mut(member).and_then(Value::as_object_mut) else {
            return;
        };
        current = next;
    }
    current.remove(path.last().expect("validated nonempty path"));
}
fn redact(value: &Value, paths: &[SettingPath]) -> Value {
    let mut redacted = value.clone();
    for path in paths {
        remove_at_path(&mut redacted, path);
    }
    redacted
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

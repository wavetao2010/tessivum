//! Layered credentials with live environment precedence and value-free metadata.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use tessivum_core::ServiceKey;
use thiserror::Error;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::{broadcast, Mutex as AsyncMutex},
};
use uuid::Uuid;

pub fn credentials_service_key() -> ServiceKey {
    ServiceKey::new("harness.credentials", "1")
}

/// A validated credential name. It is deliberately not a value container.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CredentialRef(String);

impl CredentialRef {
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialError> {
        let value = value.into();
        let expression = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("literal credential regex");
        if !expression.is_match(&value) {
            return Err(CredentialError::InvalidRef(value));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CredentialRef").field(&self.0).finish()
    }
}
impl<'de> Deserialize<'de> for CredentialRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        CredentialRef::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl TryFrom<String> for CredentialRef {
    type Error = CredentialError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl TryFrom<&str> for CredentialRef {
    type Error = CredentialError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Live environment lookup is a dependency rather than a cache, so each
/// operation observes ambient changes without retaining credentials in memory.
pub trait CredentialEnvironment: Send + Sync {
    fn get(&self, reference: &CredentialRef) -> Option<String>;
}

#[derive(Debug, Default)]
pub struct ProcessEnvironment;
impl CredentialEnvironment for ProcessEnvironment {
    fn get(&self, reference: &CredentialRef) -> Option<String> {
        std::env::var(reference.as_str()).ok()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialSource {
    Environment,
    File,
}

/// Serializable credential state that cannot reveal a secret value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialDescriptor {
    pub reference: CredentialRef,
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<CredentialSource>,
    pub writable: bool,
}

/// A committed writable-layer change. No secret is carried.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEvent {
    pub reference: CredentialRef,
    pub configured: bool,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum CredentialError {
    #[error("credential ref {0:?} must match /^[A-Za-z_][A-Za-z0-9_]*$/")]
    InvalidRef(String),
    #[error("credential values must not be blank")]
    BlankValue,
    #[error("credential {0} is shadowed by a read-only environment value")]
    Shadowed(CredentialRef),
    #[error("credential file is read-only")]
    ReadOnly,
    #[error("credentials service is closed")]
    Closed,
    #[error("credential persistence failed: {0}")]
    Persistence(String),
}

impl CredentialError {
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidRef(_) => "INVALID_CREDENTIAL_REF",
            Self::BlankValue => "INVALID_CREDENTIAL_VALUE",
            Self::Shadowed(_) => "CREDENTIAL_SHADOWED",
            Self::ReadOnly => "CREDENTIALS_READ_ONLY",
            Self::Closed => "CREDENTIALS_CLOSED",
            Self::Persistence(_) => "CREDENTIALS_PERSISTENCE_FAILED",
        }
    }
}

/// Writable YAML layer. Reads are intentionally uncached.
pub struct YamlCredentialFile {
    path: PathBuf,
    writable: bool,
}
impl fmt::Debug for YamlCredentialFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("YamlCredentialFile")
            .field("path", &self.path)
            .field("writable", &self.writable)
            .finish()
    }
}
impl YamlCredentialFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            writable: true,
        }
    }
    pub fn read_only(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            writable: false,
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn writable(&self) -> bool {
        self.writable
    }

    async fn read_all(&self) -> Result<BTreeMap<CredentialRef, String>, CredentialError> {
        let text = match fs::read_to_string(&self.path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new())
            }
            Err(error) => {
                return Err(CredentialError::Persistence(format!(
                    "read credential YAML: {error}"
                )))
            }
        };
        let raw: BTreeMap<String, String> = serde_yaml::from_str(&text).map_err(|error| {
            CredentialError::Persistence(format!("parse credential YAML: {error}"))
        })?;
        let mut values = BTreeMap::new();
        for (name, value) in raw {
            let reference = CredentialRef::new(name)?;
            if !value.trim().is_empty() {
                values.insert(reference, value);
            }
        }
        Ok(values)
    }

    async fn write_all(
        &self,
        values: &BTreeMap<CredentialRef, String>,
    ) -> Result<(), CredentialError> {
        if !self.writable {
            return Err(CredentialError::ReadOnly);
        }
        let encoded: BTreeMap<_, _> = values
            .iter()
            .map(|(reference, value)| (reference.as_str(), value))
            .collect();
        let bytes = serde_yaml::to_string(&encoded)
            .map_err(|error| {
                CredentialError::Persistence(format!("encode credential YAML: {error}"))
            })?
            .into_bytes();
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).await.map_err(|error| {
            CredentialError::Persistence(format!("create credential directory: {error}"))
        })?;
        let name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("credentials.yaml");
        let temporary = parent.join(format!(".{name}-{}.tmp", Uuid::new_v4()));
        let result = async {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).await.map_err(|error| {
                CredentialError::Persistence(format!("create credential temporary: {error}"))
            })?;
            file.write_all(&bytes).await.map_err(|error| {
                CredentialError::Persistence(format!("write credential temporary: {error}"))
            })?;
            file.sync_all().await.map_err(|error| {
                CredentialError::Persistence(format!("sync credential temporary: {error}"))
            })?;
            fs::rename(&temporary, &self.path).await.map_err(|error| {
                CredentialError::Persistence(format!("rename credential temporary: {error}"))
            })?;
            #[cfg(unix)]
            {
                let directory = fs::File::open(parent).await.map_err(|error| {
                    CredentialError::Persistence(format!("open credential directory: {error}"))
                })?;
                directory.sync_all().await.map_err(|error| {
                    CredentialError::Persistence(format!("sync credential directory: {error}"))
                })?;
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&temporary).await;
        }
        result
    }

    async fn get(&self, reference: &CredentialRef) -> Result<Option<String>, CredentialError> {
        Ok(self.read_all().await?.remove(reference))
    }
}

/// Credentials layered as live environment then writable YAML. The only mutex
/// serializes read-modify-write file operations; it never contains a secret.
pub struct Credentials {
    environment: Arc<dyn CredentialEnvironment>,
    file: Arc<YamlCredentialFile>,
    write_gate: AsyncMutex<()>,
    updates: broadcast::Sender<CredentialEvent>,
    closed: Mutex<bool>,
}
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("file", &self.file)
            .field("closed", &*lock(&self.closed))
            .finish()
    }
}

impl Credentials {
    pub fn new(file: Arc<YamlCredentialFile>) -> Self {
        Self::with_environment(Arc::new(ProcessEnvironment), file)
    }
    pub fn with_environment(
        environment: Arc<dyn CredentialEnvironment>,
        file: Arc<YamlCredentialFile>,
    ) -> Self {
        let (updates, _) = broadcast::channel(128);
        Self {
            environment,
            file,
            write_gate: AsyncMutex::new(()),
            updates,
            closed: Mutex::new(false),
        }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<CredentialEvent> {
        self.updates.subscribe()
    }

    /// Resolves fresh for each call and does not retain the returned string.
    pub async fn resolve(
        &self,
        reference: &CredentialRef,
    ) -> Result<Option<String>, CredentialError> {
        if let Some(value) = self
            .environment
            .get(reference)
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(Some(value));
        }
        self.file.get(reference).await
    }
    pub async fn describe(
        &self,
        reference: &CredentialRef,
    ) -> Result<CredentialDescriptor, CredentialError> {
        if self
            .environment
            .get(reference)
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Ok(CredentialDescriptor {
                reference: reference.clone(),
                configured: true,
                source: Some(CredentialSource::Environment),
                writable: false,
            });
        }
        let configured = self.file.get(reference).await?.is_some();
        Ok(CredentialDescriptor {
            reference: reference.clone(),
            configured,
            source: configured.then_some(CredentialSource::File),
            writable: self.file.writable(),
        })
    }
    pub async fn set(
        &self,
        reference: CredentialRef,
        value: String,
    ) -> Result<(), CredentialError> {
        if value.trim().is_empty() {
            return Err(CredentialError::BlankValue);
        }
        let _gate = self.write_gate.lock().await;
        self.ensure_open()?;
        self.ensure_not_shadowed(&reference)?;
        let mut values = self.file.read_all().await?;
        values.insert(reference.clone(), value);
        self.file.write_all(&values).await?;
        let _ = self.updates.send(CredentialEvent {
            reference,
            configured: true,
        });
        Ok(())
    }
    pub async fn unset(&self, reference: &CredentialRef) -> Result<(), CredentialError> {
        let _gate = self.write_gate.lock().await;
        self.ensure_open()?;
        self.ensure_not_shadowed(reference)?;
        let mut values = self.file.read_all().await?;
        if values.remove(reference).is_none() {
            return Ok(());
        }
        self.file.write_all(&values).await?;
        let _ = self.updates.send(CredentialEvent {
            reference: reference.clone(),
            configured: false,
        });
        Ok(())
    }
    pub async fn shutdown(&self) {
        *lock(&self.closed) = true;
        let _gate = self.write_gate.lock().await;
    }
    fn ensure_open(&self) -> Result<(), CredentialError> {
        if *lock(&self.closed) {
            Err(CredentialError::Closed)
        } else {
            Ok(())
        }
    }
    fn ensure_not_shadowed(&self, reference: &CredentialRef) -> Result<(), CredentialError> {
        if self
            .environment
            .get(reference)
            .is_some_and(|value| !value.trim().is_empty())
        {
            Err(CredentialError::Shadowed(reference.clone()))
        } else {
            Ok(())
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

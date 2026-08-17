//! Package-owned invariant installers with scoped, joined lifetimes.

use async_trait::async_trait;
use regex::Regex;
use std::{
    collections::BTreeSet,
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
};
use tessivum_core::{BoxDisposer, ContextHandle, CoreError, Fiber, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

pub fn invariants_service_key() -> ServiceKey {
    ServiceKey::new("harness.invariants", "1")
}

#[derive(Clone, Debug, Default)]
pub struct InvariantConfig {
    pub enabled: Option<bool>,
    pub package_allowlist: Vec<String>,
    pub package_blocklist: Vec<String>,
}
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("invariant violated by \"{package_name}\": {message}")]
pub struct InvariantError {
    pub code: &'static str,
    pub package_name: String,
    pub message: String,
}
impl InvariantError {
    fn new(package_name: String, message: impl Into<String>) -> Self {
        Self {
            code: "INVARIANT",
            package_name,
            message: message.into(),
        }
    }
}

/// A package-bound reporter. Installers return this error to stop their own setup or callback.
#[derive(Clone)]
pub struct InvariantFailure {
    package_name: String,
}
impl InvariantFailure {
    pub fn fail(&self, message: impl Into<String>) -> InvariantError {
        InvariantError::new(self.package_name.clone(), message)
    }
}

#[derive(Debug, Error)]
pub enum InvariantInstallerError {
    #[error(transparent)]
    Invariant(#[from] InvariantError),
    #[error("invariant installer failed: {0}")]
    Failed(String),
}
#[derive(Debug, Error)]
pub enum InvariantRegistryError {
    #[error("invalid invariant configuration: {0}")]
    Configuration(String),
    #[error("invalid invariant package name: {0}")]
    Package(String),
    #[error("invariant package is already registered: {0}")]
    Duplicate(String),
    #[error(transparent)]
    Installer(#[from] InvariantInstallerError),
    #[error(transparent)]
    Core(#[from] CoreError),
}

#[async_trait]
pub trait InvariantInstaller: Send + Sync {
    async fn install(
        &self,
        context: ContextHandle,
        fail: InvariantFailure,
    ) -> Result<(), InvariantInstallerError>;
}
#[async_trait]
impl<F, Fut> InvariantInstaller for F
where
    F: Send + Sync + Fn(ContextHandle, InvariantFailure) -> Fut,
    Fut: Future<Output = Result<(), InvariantInstallerError>> + Send,
{
    async fn install(
        &self,
        context: ContextHandle,
        fail: InvariantFailure,
    ) -> Result<(), InvariantInstallerError> {
        (self)(context, fail).await
    }
}

#[derive(Clone)]
pub struct InvariantRegistry {
    inner: Arc<Inner>,
}
struct Inner {
    owner: ContextHandle,
    enabled: bool,
    allow: Vec<Regex>,
    block: Vec<Regex>,
    reservations: Mutex<BTreeSet<String>>,
}
impl InvariantRegistry {
    pub fn new(
        owner: ContextHandle,
        config: InvariantConfig,
    ) -> Result<Self, InvariantRegistryError> {
        Ok(Self {
            inner: Arc::new(Inner {
                owner,
                enabled: config.enabled.unwrap_or(true),
                allow: patterns("package_allowlist", &config.package_allowlist)?,
                block: patterns("package_blocklist", &config.package_blocklist)?,
                reservations: Mutex::new(BTreeSet::new()),
            }),
        })
    }
    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(invariants_service_key(), self)
    }
    pub fn selected(&self, package_name: &str) -> bool {
        self.inner.enabled
            && (self.inner.allow.is_empty()
                || self.inner.allow.iter().any(|r| r.is_match(package_name)))
            && !self.inner.block.iter().any(|r| r.is_match(package_name))
    }
    /// Reserves the exact package before filtering. Enabled installers run in one dedicated child fiber and are joined here.
    pub async fn register(
        &self,
        package_name: impl Into<String>,
        installer: Arc<dyn InvariantInstaller>,
    ) -> Result<InvariantRegistration, InvariantRegistryError> {
        let package_name = package_name.into();
        package(&package_name)?;
        {
            let mut reservations = lock(&self.inner.reservations);
            if !reservations.insert(package_name.clone()) {
                return Err(InvariantRegistryError::Duplicate(package_name));
            }
        }
        let registration = InvariantRegistration {
            inner: Arc::new(Registration {
                registry: Arc::downgrade(&self.inner),
                package_name: package_name.clone(),
                fiber: Mutex::new(None),
                gate: AsyncMutex::new(()),
                released: AtomicBool::new(false),
            }),
        };
        if !self.selected(&package_name) {
            return Ok(registration);
        }
        let fiber = match Fiber::new(
            &self.inner.owner.scope(),
            format!("invariants.register({package_name:?})"),
        ) {
            Ok(fiber) => fiber,
            Err(error) => {
                registration.release();
                return Err(error.into());
            }
        };
        let owner = self.inner.owner.clone();
        let failure = InvariantFailure {
            package_name: package_name.clone(),
        };
        if let Err(error) = fiber
            .start(move |scope| async move {
                installer
                    .install(owner.with_scope(scope), failure)
                    .await
                    .map_err(|error| CoreError::from(error.to_string()))
            })
            .await
        {
            let _ = fiber.dispose().await;
            registration.release();
            return Err(InvariantRegistryError::Core(error));
        }
        *lock(&registration.inner.fiber) = Some(fiber);
        let cleanup = registration.clone();
        let label = format!("invariants.register({package_name:?})");
        let effect: BoxDisposer = Box::new(move || {
            Box::pin(async move {
                let _ = cleanup.dispose().await;
                Ok(())
            })
        });
        if let Err(error) = self.inner.owner.scope().add_effect(label, effect) {
            let _ = registration.dispose().await;
            return Err(error.into());
        }
        Ok(registration)
    }
}

#[derive(Clone)]
pub struct InvariantRegistration {
    inner: Arc<Registration>,
}
struct Registration {
    registry: Weak<Inner>,
    package_name: String,
    fiber: Mutex<Option<Fiber>>,
    gate: AsyncMutex<()>,
    released: AtomicBool,
}
impl InvariantRegistration {
    pub fn package_name(&self) -> &str {
        &self.inner.package_name
    }
    /// Stops the child installer, awaits all scoped cleanup, and releases the exact reservation.
    pub async fn dispose(&self) -> Result<(), InvariantRegistryError> {
        let _gate = self.inner.gate.lock().await;
        let fiber = { lock(&self.inner.fiber).take() };
        let result = match fiber {
            Some(fiber) => fiber.dispose().await.map_err(InvariantRegistryError::Core),
            None => Ok(()),
        };
        self.release();
        result
    }
    fn release(&self) {
        if self.inner.released.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(registry) = self.inner.registry.upgrade() {
            lock(&registry.reservations).remove(&self.inner.package_name);
        }
    }
}

fn patterns(field: &str, values: &[String]) -> Result<Vec<Regex>, InvariantRegistryError> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::with_capacity(values.len());
    for value in values {
        if value.is_empty() || value.trim() != value {
            return Err(InvariantRegistryError::Configuration(format!(
                "{field} entries must be non-blank and have no surrounding whitespace"
            )));
        }
        if !seen.insert(value.clone()) {
            return Err(InvariantRegistryError::Configuration(format!(
                "{field} contains duplicate regex {value:?}"
            )));
        }
        result.push(Regex::new(value).map_err(|error| {
            InvariantRegistryError::Configuration(format!(
                "{field} contains invalid regex {value:?}: {error}"
            ))
        })?);
    }
    Ok(result)
}
fn package(package_name: &str) -> Result<(), InvariantRegistryError> {
    if package_name.is_empty()
        || package_name.trim() != package_name
        || package_name.chars().any(char::is_whitespace)
    {
        Err(InvariantRegistryError::Package(
            "must be non-blank and contain no whitespace".into(),
        ))
    } else {
        Ok(())
    }
}
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

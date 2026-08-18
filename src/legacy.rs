//! Managed Legacy Node profile integration.
//!
//! One profile owns one compatibility-host process and one typed domain bridge.
//! Every Node-owned registration is tied to its connection generation, so a
//! crash cannot leave stale services or dependencies looking active.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::Duration,
};

use sha2::{Digest, Sha256};
use tessivum_core::{
    ContextHandle, Entry, Loader, LoaderError, LoaderFuture, LoaderRuntime, PackageResolver,
    ResolvedPackage, RuntimeHandle, RuntimeKind,
};
use tessivum_extism::{
    CapabilityRegistry, PluginError as WasmPluginError, PluginManifest, ResourceLimits,
    WasmLifecycleGuard, WasmLifecycleHook, WasmPackage, WasmPluginRuntime, WasmResult,
};
use tessivum_node_bridge::{
    BridgeClient, BridgeError, ClientConfig, ConnectionStatus, HostCommand, LegacyNodeRuntime,
    NodeSupervisor,
};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    bridge::{
        BridgeServices, DomainBridge, WasmEffectivePolicy, WasmPolicyRegistration,
        WasmPolicyRegistry,
    },
    plugins::{
        CompatibilityReport, PluginError, PluginPackage, PluginRouter, PluginRuntime,
        ServiceMethodPermission,
    },
};

/// Failure while configuring or operating a [`LegacyProfile`].
#[derive(Debug, Error)]
pub enum LegacyProfileError {
    #[error("invalid legacy Node profile: {0}")]
    InvalidConfiguration(String),
    #[error("legacy Node host is already running")]
    Running,
    #[error("legacy Node host cleanup is still in progress")]
    CleanupPending,
    #[error("legacy Node host has not been started")]
    NotStarted,
    #[error(transparent)]
    Bridge(#[from] BridgeError),
    #[error("legacy Node lifecycle task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Loader(#[from] LoaderError),
}

/// The current connection state of a Legacy Node profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyProfileHealth {
    Stopped,
    Starting,
    Ready,
    Disconnected,
    Stopping,
}

impl From<ConnectionStatus> for LegacyProfileHealth {
    fn from(status: ConnectionStatus) -> Self {
        match status {
            ConnectionStatus::Handshaking => Self::Starting,
            ConnectionStatus::Ready => Self::Ready,
            ConnectionStatus::Disconnected => Self::Disconnected,
        }
    }
}

/// A cheap observation of the managed compatibility host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyProfileSnapshot {
    pub generation: Option<u64>,
    pub health: LegacyProfileHealth,
}

enum Lifecycle {
    Stopped,
    Starting(Option<u64>),
    Running(BridgeClient),
    Stopping,
}

struct ProfileInner {
    supervisor: Arc<NodeSupervisor>,
    bridge: Arc<DomainBridge>,
    request_timeout: Duration,
    lifecycle: Mutex<Lifecycle>,
    shutdown_gate: AsyncMutex<()>,
}

/// A managed Legacy Node compatibility host for one product profile.
///
/// Node code receives only the explicit typed domain bridge. It never receives
/// a raw [`tessivum_core::ContextHandle`] or a generic context getter.
#[derive(Clone)]
pub struct LegacyProfile {
    inner: Arc<ProfileInner>,
}

impl std::fmt::Debug for LegacyProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LegacyProfile")
            .field("snapshot", &self.snapshot())
            .finish_non_exhaustive()
    }
}

impl LegacyProfile {
    /// Validates and owns one restartable Node host command and bridge service set.
    pub fn new(
        command: HostCommand,
        config: ClientConfig,
        services: BridgeServices,
    ) -> Result<Self, LegacyProfileError> {
        let command = validate_command(command)?;
        validate_config(&config)?;
        let request_timeout = config.request_timeout;
        let supervisor = Arc::new(NodeSupervisor::new(command, config)?);
        let bridge = Arc::new(DomainBridge::new(services)?);
        Ok(Self {
            inner: Arc::new(ProfileInner {
                supervisor,
                bridge,
                request_timeout,
                lifecycle: Mutex::new(Lifecycle::Stopped),
                shutdown_gate: AsyncMutex::new(()),
            }),
        })
    }

    /// Starts the host and completes the hello/ready handshake.
    ///
    /// This does not expose a runtime until the typed bridge handler is set, the
    /// generation is attached, and crash cleanup is registered.
    pub fn start(&self) -> Result<(), LegacyProfileError> {
        {
            let mut lifecycle = lock(&self.inner.lifecycle);
            match &*lifecycle {
                Lifecycle::Stopped => *lifecycle = Lifecycle::Starting(None),
                Lifecycle::Starting(_) | Lifecycle::Stopping => {
                    return Err(LegacyProfileError::CleanupPending)
                }
                Lifecycle::Running(_) => return Err(LegacyProfileError::Running),
            }
        }

        let client = match self.inner.supervisor.start() {
            Ok(client) => client,
            Err(error) => {
                self.reset_stopped();
                return Err(error.into());
            }
        };
        let generation = client.generation();

        if let Err(error) = self.inner.bridge.attach_client(client.clone(), generation) {
            self.stop_failed_start(generation);
            return Err(error.into());
        }
        *lock(&self.inner.lifecycle) = Lifecycle::Starting(Some(generation));

        let cleanup = Arc::downgrade(&self.inner);
        if let Err(error) = self.inner.supervisor.register_cleanup(generation, {
            let cleanup = cleanup.clone();
            move || cleanup_profile_generation(&cleanup, generation)
        }) {
            self.inner.bridge.cleanup_generation(generation);
            self.stop_failed_start(generation);
            return Err(error.into());
        }
        // BridgeClient currently has one disconnect callback. Replace the
        // supervisor's callback only after registering its cleanup, then run
        // supervisor shutdown ourselves so its generation teardown is never
        // lost if another bridge participant replaced that callback.
        let disconnect = cleanup;
        client.set_disconnect_handler(move |_| {
            if let Some(inner) = disconnect.upgrade() {
                let supervisor = Arc::clone(&inner.supervisor);
                let _ = supervisor.shutdown();
            }
            cleanup_profile_generation(&disconnect, generation);
        });

        let mut lifecycle = lock(&self.inner.lifecycle);
        let attached = matches!(
            &*lifecycle,
            Lifecycle::Starting(Some(current)) if *current == generation
        ) && client.status() != ConnectionStatus::Disconnected;
        if attached {
            *lifecycle = Lifecycle::Running(client.clone());
            drop(lifecycle);
            spawn_heartbeat_monitor(Arc::downgrade(&self.inner), client, generation);
            Ok(())
        } else {
            drop(lifecycle);
            self.stop_failed_start(generation);
            Err(LegacyProfileError::CleanupPending)
        }
    }

    /// Returns the Loader runtime bound to the current, ready host generation.
    pub fn runtime(&self) -> Result<LegacyNodeRuntime, LegacyProfileError> {
        match &*lock(&self.inner.lifecycle) {
            Lifecycle::Running(client) if client.status() == ConnectionStatus::Ready => {
                Ok(LegacyNodeRuntime::new(client.clone()).with_timeout(self.inner.request_timeout))
            }
            Lifecycle::Running(client) if client.status() == ConnectionStatus::Disconnected => {
                Err(client
                    .disconnect_error()
                    .unwrap_or_else(|| BridgeError::Disconnected("host stopped".into()))
                    .into())
            }
            Lifecycle::Stopped => Err(LegacyProfileError::NotStarted),
            Lifecycle::Starting(_) | Lifecycle::Stopping | Lifecycle::Running(_) => {
                Err(LegacyProfileError::CleanupPending)
            }
        }
    }

    /// Starts a new generation only after the prior generation has cleaned up.
    pub async fn restart(&self) -> Result<(), LegacyProfileError> {
        self.start()
    }

    /// Gracefully drains and, if necessary, kills the owned process tree.
    ///
    /// The brief lifecycle transition happens before the blocking operation;
    /// neither profile locks nor bridge locks are held while the supervisor
    /// waits for async Node disposers or kills the process group.
    pub async fn shutdown(&self) -> Result<(), LegacyProfileError> {
        let _shutdown = self.inner.shutdown_gate.lock().await;
        {
            let mut lifecycle = lock(&self.inner.lifecycle);
            match &*lifecycle {
                Lifecycle::Stopped => return Ok(()),
                Lifecycle::Starting(_) => return Err(LegacyProfileError::CleanupPending),
                Lifecycle::Running(_) => *lifecycle = Lifecycle::Stopping,
                Lifecycle::Stopping => return Ok(()),
            }
        }

        let supervisor = Arc::clone(&self.inner.supervisor);
        let result = tokio::task::spawn_blocking(move || supervisor.shutdown()).await?;
        self.reset_stopped();
        result?;
        Ok(())
    }

    pub fn health(&self) -> LegacyProfileHealth {
        match &*lock(&self.inner.lifecycle) {
            Lifecycle::Stopped => LegacyProfileHealth::Stopped,
            Lifecycle::Starting(_) => LegacyProfileHealth::Starting,
            Lifecycle::Running(client) => client.status().into(),
            Lifecycle::Stopping => LegacyProfileHealth::Stopping,
        }
    }

    pub fn snapshot(&self) -> LegacyProfileSnapshot {
        LegacyProfileSnapshot {
            generation: self.inner.supervisor.generation(),
            health: self.health(),
        }
    }

    fn reset_stopped(&self) {
        *lock(&self.inner.lifecycle) = Lifecycle::Stopped;
    }

    fn stop_failed_start(&self, generation: u64) {
        self.inner.bridge.cleanup_generation(generation);
        let _ = self.inner.supervisor.shutdown();
        self.reset_stopped();
    }
}

fn cleanup_profile_generation(inner: &Weak<ProfileInner>, generation: u64) {
    let Some(inner) = inner.upgrade() else {
        return;
    };
    inner.bridge.cleanup_generation(generation);
    let mut lifecycle = lock(&inner.lifecycle);
    let owns_generation = match &*lifecycle {
        Lifecycle::Starting(Some(current)) => *current == generation,
        Lifecycle::Running(client) => client.generation() == generation,
        Lifecycle::Stopped | Lifecycle::Starting(None) | Lifecycle::Stopping => false,
    };
    if owns_generation {
        *lifecycle = Lifecycle::Stopped;
    }
}
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(100);

fn spawn_heartbeat_monitor(inner: Weak<ProfileInner>, client: BridgeClient, generation: u64) {
    std::thread::spawn(move || loop {
        if generation_client(&inner, generation).is_none() {
            return;
        }
        let before = client.last_heartbeat();
        if client.heartbeat().is_err() {
            client.close();
            cleanup_profile_generation(&inner, generation);
            return;
        }
        std::thread::sleep(HEARTBEAT_INTERVAL);
        if generation_client(&inner, generation).is_none() {
            return;
        }
        if client.last_heartbeat() <= before {
            client.close();
            cleanup_profile_generation(&inner, generation);
            return;
        }
    });
}

fn generation_client(inner: &Weak<ProfileInner>, generation: u64) -> Option<BridgeClient> {
    let inner = inner.upgrade()?;
    let client = match &*lock(&inner.lifecycle) {
        Lifecycle::Running(client) if client.generation() == generation => client.clone(),
        Lifecycle::Stopped
        | Lifecycle::Starting(_)
        | Lifecycle::Running(_)
        | Lifecycle::Stopping => {
            return None;
        }
    };
    (client.status() == ConnectionStatus::Ready).then_some(client)
}

fn validate_config(config: &ClientConfig) -> Result<(), LegacyProfileError> {
    if config.max_frame_size == 0 || config.max_frame_size > u32::MAX as usize {
        return Err(LegacyProfileError::InvalidConfiguration(
            "max_frame_size must be between 1 and u32::MAX".into(),
        ));
    }
    if config.queue_capacity == 0 {
        return Err(LegacyProfileError::InvalidConfiguration(
            "queue_capacity must be greater than zero".into(),
        ));
    }
    if [
        config.handshake_timeout,
        config.request_timeout,
        config.shutdown_timeout,
    ]
    .into_iter()
    .any(|timeout| timeout.is_zero())
    {
        return Err(LegacyProfileError::InvalidConfiguration(
            "handshake, request, and shutdown timeouts must be positive".into(),
        ));
    }
    Ok(())
}

fn validate_command(mut command: HostCommand) -> Result<HostCommand, LegacyProfileError> {
    if command.program.as_os_str().is_empty() {
        return Err(LegacyProfileError::InvalidConfiguration(
            "host program must not be empty".into(),
        ));
    }
    command.program = resolve_program(&command.program)?;
    if let Some(cwd) = command.cwd.take() {
        let cwd = std::fs::canonicalize(&cwd).map_err(|error| {
            LegacyProfileError::InvalidConfiguration(format!(
                "host working directory {cwd:?} is unavailable: {error}"
            ))
        })?;
        if !cwd.is_dir() {
            return Err(LegacyProfileError::InvalidConfiguration(format!(
                "host working directory {cwd:?} is not a directory"
            )));
        }
        command.cwd = Some(cwd);
    }
    let mut names = BTreeSet::<OsString>::new();
    for (name, _) in &command.env {
        if name.is_empty() || !names.insert(name.clone()) {
            return Err(LegacyProfileError::InvalidConfiguration(
                "host environment names must be non-empty and unique".into(),
            ));
        }
    }
    Ok(command)
}

fn resolve_program(program: &Path) -> Result<PathBuf, LegacyProfileError> {
    if program.is_absolute() || program.components().count() != 1 {
        return canonical_executable(program);
    }
    let Some(path) = std::env::var_os("PATH") else {
        return Err(LegacyProfileError::InvalidConfiguration(
            "PATH is required to resolve the host program".into(),
        ));
    };
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| executable(candidate))
        .map(|candidate| canonical_executable(&candidate))
        .transpose()?
        .ok_or_else(|| {
            LegacyProfileError::InvalidConfiguration(format!(
                "host program {program:?} is not executable on PATH"
            ))
        })
}

fn canonical_executable(path: &Path) -> Result<PathBuf, LegacyProfileError> {
    if !executable(path) {
        return Err(LegacyProfileError::InvalidConfiguration(format!(
            "host program {path:?} is not executable"
        )));
    }
    std::fs::canonicalize(path).map_err(|error| {
        LegacyProfileError::InvalidConfiguration(format!(
            "could not canonicalize host program {path:?}: {error}"
        ))
    })
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

const MAX_WASM_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
type WasmPolicyDefinitions = Arc<Mutex<BTreeMap<String, BTreeSet<ServiceMethodPermission>>>>;

/// Adapts validated product manifests to the generic Extism runtime.
pub struct WasmProductRuntime {
    inner: Arc<WasmPluginRuntime>,
    definitions: WasmPolicyDefinitions,
    registered: Mutex<BTreeSet<String>>,
    // ponytail: cold-path serialization binds a staged policy to the manifest being instantiated;
    // replace with generation-scoped hook inputs only if parallel plugin startup becomes material.
    instantiate_gate: AsyncMutex<()>,
}

impl WasmProductRuntime {
    pub fn new(
        capabilities: Arc<CapabilityRegistry>,
        policies: WasmPolicyRegistry,
        limits: ResourceLimits,
    ) -> Self {
        let definitions = Arc::new(Mutex::new(BTreeMap::new()));
        let hook = Arc::new(ProductWasmLifecycle {
            definitions: Arc::clone(&definitions),
            policies,
        });
        Self {
            inner: Arc::new(WasmPluginRuntime::new(capabilities, limits).with_lifecycle_hook(hook)),
            definitions,
            registered: Mutex::new(BTreeSet::new()),
            instantiate_gate: AsyncMutex::new(()),
        }
    }

    fn prepare_package(&self, package: &ResolvedPackage) -> Result<ResolvedPackage, LoaderError> {
        let inspected = PluginPackage::inspect(&package.specifier).map_err(router_error)?;
        let product = inspected.wasm_product_declaration().map_err(router_error)?;
        let resolved = fs::canonicalize(&package.location).map_err(|error| {
            LoaderError::Validation(format!(
                "WASM package {:?} is unavailable: {error}",
                package.location
            ))
        })?;
        if resolved != product.entry {
            return Err(LoaderError::Validation(format!(
                "WASM package {:?} resolved to an unexpected artifact",
                package.specifier
            )));
        }
        let metadata = fs::metadata(&resolved).map_err(|error| {
            LoaderError::Validation(format!("cannot inspect {}: {error}", resolved.display()))
        })?;
        if metadata.len() > MAX_WASM_PACKAGE_BYTES {
            return Err(LoaderError::Validation(format!(
                "WASM package exceeds the {MAX_WASM_PACKAGE_BYTES}-byte limit"
            )));
        }
        let wasm = fs::read(&resolved).map_err(|error| {
            LoaderError::Validation(format!("cannot read {}: {error}", resolved.display()))
        })?;
        let digest = format!("{:x}", Sha256::digest(&wasm));
        let runtime_specifier = format!("{}#sha256:{digest}", package.specifier);
        let manifest = product.manifest;
        let plugin_id = manifest.id.clone();
        let wasm_package = WasmPackage::from_bytes(manifest, wasm).map_err(wasm_loader_error)?;
        let should_register = lock(&self.registered).insert(runtime_specifier.clone());
        if should_register {
            if let Err(error) = self.inner.register(runtime_specifier.clone(), wasm_package) {
                lock(&self.registered).remove(&runtime_specifier);
                return Err(wasm_loader_error(error));
            }
        }
        lock(&self.definitions).insert(plugin_id, product.service_permissions);
        Ok(ResolvedPackage {
            specifier: runtime_specifier,
            location: package.location.clone(),
        })
    }
}

impl LoaderRuntime for WasmProductRuntime {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Wasm
    }

    fn instantiate<'a>(
        &'a self,
        package: ResolvedPackage,
        entry: Entry,
        context: ContextHandle,
    ) -> LoaderFuture<'a, Box<dyn RuntimeHandle>> {
        Box::pin(async move {
            let _gate = self.instantiate_gate.lock().await;
            let package = self.prepare_package(&package)?;
            self.inner.instantiate(package, entry, context).await
        })
    }
}

struct ProductWasmLifecycle {
    definitions: WasmPolicyDefinitions,
    policies: WasmPolicyRegistry,
}

impl WasmLifecycleHook for ProductWasmLifecycle {
    fn install(&self, manifest: &PluginManifest) -> WasmResult<Box<dyn WasmLifecycleGuard>> {
        let methods = lock(&self.definitions)
            .get(&manifest.id)
            .cloned()
            .ok_or_else(|| {
                WasmPluginError::new(
                    "PLUGIN_POLICY_NOT_FOUND",
                    "validated product policy is not staged",
                    "policy",
                )
            })?;
        let registration = self
            .policies
            .install(WasmEffectivePolicy::new(manifest.id.clone(), methods))?;
        Ok(Box::new(ProductWasmGuard { registration }))
    }
}

struct ProductWasmGuard {
    registration: WasmPolicyRegistration,
}

impl WasmLifecycleGuard for ProductWasmGuard {
    fn drain(&mut self, timeout: Duration) -> WasmResult<()> {
        self.registration.drain(timeout)
    }

    fn revoke(&mut self) {
        self.registration.revoke();
    }
}

/// Resolves product packages through the deterministic compatibility router.
///
/// Locations given to a runtime are canonical absolute paths. An optional root
/// closes the usual package-path escape hatch: symlinks are resolved before the
/// containment check, so `../` and an in-root symlink cannot select a package
/// outside the configured product tree.
#[derive(Clone)]
pub struct ProductPackageResolver {
    router: Arc<PluginRouter>,
    root: Option<PathBuf>,
}

impl Default for ProductPackageResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ProductPackageResolver {
    pub fn new() -> Self {
        Self {
            router: Arc::new(PluginRouter::new()),
            root: std::fs::canonicalize(".").ok(),
        }
    }

    pub fn with_router(router: PluginRouter) -> Self {
        Self {
            router: Arc::new(router),
            root: std::fs::canonicalize(".").ok(),
        }
    }

    pub fn confine_to(mut self, root: impl AsRef<Path>) -> Result<Self, LegacyProfileError> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(|error| {
            LegacyProfileError::InvalidConfiguration(format!(
                "package root {:?} is unavailable: {error}",
                root.as_ref()
            ))
        })?;
        if !root.is_dir() {
            return Err(LegacyProfileError::InvalidConfiguration(format!(
                "package root {root:?} is not a directory"
            )));
        }
        self.root = Some(root);
        Ok(self)
    }

    pub fn inspect(
        &self,
        path: impl AsRef<Path>,
        explicit: Option<PluginRuntime>,
    ) -> Result<CompatibilityReport, PluginError> {
        self.router.inspect(path.as_ref(), explicit)
    }

    fn resolve_package(
        &self,
        specifier: &str,
        runtime: RuntimeKind,
    ) -> Result<ResolvedPackage, LoaderError> {
        let package = self
            .router
            .package(Path::new(specifier))
            .map_err(router_error)?;
        let route = self
            .router
            .route(&package, Some(plugin_runtime(runtime)))
            .map_err(router_error)?;
        if RuntimeKind::try_from(route.runtime).ok() != Some(runtime) {
            return Err(LoaderError::Validation(format!(
                "package {specifier:?} routes to {:?}, not requested runtime {runtime:?}",
                route.runtime
            )));
        }
        let location = route.artifact.ok_or_else(|| {
            LoaderError::Validation(format!(
                "package {specifier:?} selected runtime {runtime:?} has no server artifact"
            ))
        })?;
        if !location.is_absolute() {
            return Err(LoaderError::Validation(format!(
                "package {specifier:?} resolved to a non-absolute location {location:?}"
            )));
        }
        let root = self.root.as_ref().ok_or_else(|| {
            LoaderError::Validation("could not establish a canonical package root".into())
        })?;
        if !location.starts_with(root) {
            return Err(LoaderError::Validation(format!(
                "package {specifier:?} resolves outside the configured package root"
            )));
        }
        Ok(ResolvedPackage {
            specifier: specifier.into(),
            location: location.to_string_lossy().into_owned(),
        })
    }
}

impl PackageResolver for ProductPackageResolver {
    fn resolve<'a>(
        &'a self,
        specifier: &'a str,
        runtime: RuntimeKind,
    ) -> LoaderFuture<'a, ResolvedPackage> {
        Box::pin(async move { self.resolve_package(specifier, runtime) })
    }
}

/// Creates a Core Loader with the product WASM runtime and an optional Legacy Node runtime.
pub fn product_loader(
    profile: Option<&LegacyProfile>,
    resolver: Arc<dyn PackageResolver>,
    wasm: Arc<WasmProductRuntime>,
) -> Result<Loader, LegacyProfileError> {
    let mut runtimes: Vec<Arc<dyn LoaderRuntime>> = vec![wasm];
    if let Some(profile) = profile {
        runtimes.push(Arc::new(profile.runtime()?));
    }
    Ok(Loader::try_new(resolver, runtimes)?)
}

/// Creates a Core Loader with the currently active Legacy Node runtime.
pub fn legacy_loader(
    profile: &LegacyProfile,
    resolver: Arc<dyn PackageResolver>,
) -> Result<Loader, LegacyProfileError> {
    let runtime: Arc<dyn LoaderRuntime> = Arc::new(profile.runtime()?);
    Ok(Loader::try_new(resolver, [runtime])?)
}

fn plugin_runtime(runtime: RuntimeKind) -> PluginRuntime {
    match runtime {
        RuntimeKind::Native => PluginRuntime::Native,
        RuntimeKind::Wasm => PluginRuntime::Wasm,
        RuntimeKind::LegacyNode => PluginRuntime::LegacyNode,
    }
}

fn router_error(error: PluginError) -> LoaderError {
    LoaderError::Validation(error.to_string())
}

fn wasm_loader_error(error: WasmPluginError) -> LoaderError {
    LoaderError::Validation(error.to_string())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

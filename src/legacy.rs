//! Managed Legacy Node profile integration.
//!
//! One profile owns one compatibility-host process and one typed domain bridge.
//! Every Node-owned registration is tied to its connection generation, so a
//! crash cannot leave stale services or dependencies looking active.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Read,
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
const MARKET_BRIDGE_MAX_FRAME_SIZE: usize = 12 * 1024 * 1024;

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
        mut config: ClientConfig,
        services: BridgeServices,
    ) -> Result<Self, LegacyProfileError> {
        config.max_frame_size = MARKET_BRIDGE_MAX_FRAME_SIZE;
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

    /// Returns the generation-scoped route registry owned by this profile's bridge.
    pub fn web_route_registry(&self) -> DomainBridge {
        self.inner.bridge.as_ref().clone()
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
        client.set_log_handler(|message| eprintln!("Legacy Node: {message}"));
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

const MAX_WASM_PACKAGE_BYTES: usize = 8 * 1024 * 1024;
type StagedWasmPolicy = Arc<Mutex<Option<WasmPolicyDefinition>>>;

struct WasmPolicyDefinition {
    plugin_id: String,
    methods: BTreeSet<ServiceMethodPermission>,
}

/// Adapts validated product manifests to the generic Extism runtime.
pub struct WasmProductRuntime {
    inner: Arc<WasmPluginRuntime>,
    staged_policy: StagedWasmPolicy,
    // ponytail: one cached module per distinct validated contract for this Host lifetime;
    // add eviction only if measured hot-reload churn makes this material.
    registered: Mutex<BTreeSet<String>>,
    // ponytail: cold-path serialization binds one staged policy to one runtime candidate;
    // replace with a batch-aware Loader hook only if parallel plugin startup becomes material.
    instantiate_gate: AsyncMutex<()>,
}

impl WasmProductRuntime {
    pub fn new(
        capabilities: Arc<CapabilityRegistry>,
        policies: WasmPolicyRegistry,
        limits: ResourceLimits,
    ) -> Self {
        let staged_policy = Arc::new(Mutex::new(None));
        let hook = Arc::new(ProductWasmLifecycle {
            staged_policy: Arc::clone(&staged_policy),
            policies,
        });
        Self {
            inner: Arc::new(WasmPluginRuntime::new(capabilities, limits).with_lifecycle_hook(hook)),
            staged_policy,
            registered: Mutex::new(BTreeSet::new()),
            instantiate_gate: AsyncMutex::new(()),
        }
    }

    fn prepare_package(
        &self,
        package: &ResolvedPackage,
    ) -> Result<(ResolvedPackage, WasmPolicyDefinition), LoaderError> {
        let inspected = PluginPackage::inspect(&package.location).map_err(router_error)?;
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
        let wasm = read_wasm_artifact(&resolved)?;
        let contract = serde_json::to_vec(&(&product.manifest, &product.service_permissions))
            .map_err(|error| {
                LoaderError::Validation(format!("cannot encode WASM package contract: {error}"))
            })?;
        let mut hasher = Sha256::new();
        hasher.update(&contract);
        hasher.update(&wasm);
        let runtime_specifier = format!("{}#sha256:{:x}", package.specifier, hasher.finalize());
        let plugin_id = product.manifest.id.clone();
        let policy = WasmPolicyDefinition {
            plugin_id,
            methods: product.service_permissions,
        };
        let wasm_package =
            WasmPackage::from_bytes(product.manifest, wasm).map_err(wasm_loader_error)?;
        let should_register = lock(&self.registered).insert(runtime_specifier.clone());
        if should_register {
            if let Err(error) = self.inner.register(runtime_specifier.clone(), wasm_package) {
                lock(&self.registered).remove(&runtime_specifier);
                return Err(wasm_loader_error(error));
            }
        }
        Ok((
            ResolvedPackage {
                specifier: runtime_specifier,
                location: package.location.clone(),
            },
            policy,
        ))
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
            let (package, policy) = self.prepare_package(&package)?;
            *lock(&self.staged_policy) = Some(policy);
            let result = self.inner.instantiate(package, entry, context).await;
            lock(&self.staged_policy).take();
            result
        })
    }
}

struct ProductWasmLifecycle {
    staged_policy: StagedWasmPolicy,
    policies: WasmPolicyRegistry,
}

impl WasmLifecycleHook for ProductWasmLifecycle {
    fn install(
        &self,
        manifest: &PluginManifest,
        entry: &Entry,
        instance_id: &str,
    ) -> WasmResult<Box<dyn WasmLifecycleGuard>> {
        let policy = lock(&self.staged_policy).take().ok_or_else(|| {
            WasmPluginError::new(
                "PLUGIN_POLICY_NOT_FOUND",
                "validated product policy is not staged",
                "policy",
            )
        })?;
        if policy.plugin_id != manifest.id {
            return Err(WasmPluginError::new(
                "PLUGIN_POLICY_NOT_FOUND",
                "staged product policy does not match the manifest",
                "policy",
            ));
        }
        let registration = self.policies.install(WasmEffectivePolicy::new(
            manifest.id.clone(),
            instance_id,
            entry.options.id.to_string(),
            policy.methods,
        ))?;
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

fn read_wasm_artifact(path: &Path) -> Result<Vec<u8>, LoaderError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|error| {
        LoaderError::Validation(format!("cannot open {}: {error}", path.display()))
    })?;
    let metadata = file.metadata().map_err(|error| {
        LoaderError::Validation(format!("cannot inspect {}: {error}", path.display()))
    })?;
    if !metadata.is_file() || metadata.len() > MAX_WASM_PACKAGE_BYTES as u64 {
        return Err(LoaderError::Validation(format!(
            "WASM package must be a regular file no larger than {MAX_WASM_PACKAGE_BYTES} bytes"
        )));
    }
    let mut wasm = Vec::with_capacity(metadata.len() as usize + 1);
    file.take(MAX_WASM_PACKAGE_BYTES as u64 + 1)
        .read_to_end(&mut wasm)
        .map_err(|error| {
            LoaderError::Validation(format!("cannot read {}: {error}", path.display()))
        })?;
    if wasm.len() > MAX_WASM_PACKAGE_BYTES {
        return Err(LoaderError::Validation(format!(
            "WASM package exceeds the {MAX_WASM_PACKAGE_BYTES}-byte limit"
        )));
    }
    Ok(wasm)
}

/// Resolves product packages through the deterministic compatibility router.
///
/// Locations given to a runtime are canonical absolute paths. An optional root
/// closes the usual package-path escape hatch: symlinks are resolved before the
/// containment check, so `../` and an in-root symlink cannot select a package
/// outside the configured product tree.
type PackageLocationAllowlist = Arc<dyn Fn(&str, &Path, RuntimeKind) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct ProductPackageResolver {
    router: Arc<PluginRouter>,
    root: Option<PathBuf>,
    native_packages: Arc<BTreeSet<String>>,
    legacy_node_modules: Option<PathBuf>,
    allow_location: Option<PackageLocationAllowlist>,
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
            native_packages: Arc::new(BTreeSet::new()),
            legacy_node_modules: None,
            allow_location: None,
        }
    }

    pub fn with_router(router: PluginRouter) -> Self {
        Self {
            router: Arc::new(router),
            root: std::fs::canonicalize(".").ok(),
            native_packages: Arc::new(BTreeSet::new()),
            legacy_node_modules: None,
            allow_location: None,
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
    pub fn with_native_packages(mut self, packages: impl IntoIterator<Item = String>) -> Self {
        self.native_packages = Arc::new(packages.into_iter().collect());
        self
    }

    pub fn with_legacy_node_modules(mut self, root: impl Into<PathBuf>) -> Self {
        self.legacy_node_modules = Some(root.into());
        self
    }

    pub fn allow_location_with(
        mut self,
        allow: impl Fn(&str, &Path, RuntimeKind) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.allow_location = Some(Arc::new(allow));
        self
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
        if runtime == RuntimeKind::Native {
            if !self.native_packages.contains(specifier) {
                return Err(LoaderError::Validation(format!(
                    "no native plugin factory is registered for {specifier}"
                )));
            }
            return Ok(ResolvedPackage {
                specifier: specifier.into(),
                location: specifier.into(),
            });
        }

        let legacy_package = (runtime == RuntimeKind::LegacyNode)
            .then(|| {
                self.legacy_node_modules
                    .as_ref()
                    .zip(legacy_package_relative(specifier))
                    .map(|(root, relative)| root.join(relative))
            })
            .flatten();
        let package_path = legacy_package
            .as_deref()
            .unwrap_or_else(|| Path::new(specifier));
        let package = self.router.package(package_path).map_err(router_error)?;
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
        let inside_root = self
            .root
            .as_ref()
            .is_some_and(|root| location.starts_with(root));
        let explicitly_allowed = legacy_package.is_some()
            || self
                .allow_location
                .as_ref()
                .is_some_and(|allow| allow(specifier, &location, runtime));
        if !inside_root && !explicitly_allowed {
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

fn legacy_package_relative(specifier: &str) -> Option<PathBuf> {
    let segment = |value: &str| {
        !value.is_empty()
            && value != "."
            && value != ".."
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'~')
            })
    };
    if let Some(scoped) = specifier.strip_prefix('@') {
        let mut parts = scoped.split('/');
        let scope = parts.next()?;
        let package = parts.next()?;
        (segment(scope) && segment(package) && parts.next().is_none())
            .then(|| PathBuf::from(format!("@{scope}")).join(package))
    } else {
        segment(specifier).then(|| PathBuf::from(specifier))
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

#[cfg(test)]
mod package_resolver_tests {
    use super::*;
    use uuid::Uuid;

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("tessivum-resolver-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn resolves_installed_legacy_package_ids_outside_workspace() {
        let root = TempRoot::new();
        let confined = root.0.join("workspace");
        let modules = root.0.join("profile/node_modules");
        let package = modules.join("@community/example");
        fs::create_dir_all(&confined).unwrap();
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"@community/example","version":"1.0.0","main":"index.js"}"#,
        )
        .unwrap();
        fs::write(package.join("index.js"), "export const apply = () => {};").unwrap();
        let resolver = ProductPackageResolver::new()
            .with_legacy_node_modules(modules)
            .confine_to(confined)
            .unwrap();

        let resolved = resolver
            .resolve_package("@community/example", RuntimeKind::LegacyNode)
            .unwrap();

        assert!(Path::new(&resolved.location).ends_with("@community/example/index.js"));
    }

    #[test]
    fn admits_only_explicit_mode_wasm_packages_outside_workspace() {
        let root = TempRoot::new();
        let confined = root.0.join("workspace");
        fs::create_dir_all(&confined).unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/wasm/rust-minimal/plugin.json")
            .canonicalize()
            .unwrap();
        let allowed = source.clone();
        let resolver = ProductPackageResolver::new()
            .allow_location_with(move |specifier, location, runtime| {
                runtime == RuntimeKind::Wasm
                    && Path::new(specifier) == allowed
                    && location.starts_with(allowed.parent().unwrap())
            })
            .confine_to(confined)
            .unwrap();

        let resolved = resolver
            .resolve_package(source.to_str().unwrap(), RuntimeKind::Wasm)
            .unwrap();

        assert!(Path::new(&resolved.location).ends_with("plugin.wasm"));
    }
}

//! Session-owned native composition registry.
//!
//! A composition admits only a typed Core entry reference and bounded JSON
//! configuration. Runtime construction remains injected so the Host retains
//! authority over registered native factories and product WASM/Legacy loaders.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tessivum_core::{
    ActivationState, ContextHandle, Entry, EntryId, EntryOptions, EntryTree, FiberState,
    LoaderRuntime, PackageResolver, ResolvedPackage, RuntimeHandle, RuntimeKind, Scope,
};
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    ContentBlock, SessionId, TessivumError,
};

pub const MAX_COMPOSITIONS_PER_SESSION: usize = 64;
pub const MAX_COMPOSITION_CONFIG_BYTES: usize = 64 * 1024;
pub const MAX_COMPOSITION_ID_BYTES: usize = 128;
pub const MAX_COMPOSITION_PACKAGE_BYTES: usize = 4 * 1024;
const MAX_COMPOSITION_CONFIG_DEPTH: usize = 32;
const MAX_COMPOSITION_CONFIG_NODES: usize = 2_048;
const MAX_FAILURE_TEXT_BYTES: usize = 4 * 1024;

/// The only execution targets admitted by a composition descriptor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompositionRuntime {
    Native,
    Wasm,
    Legacy,
}

impl CompositionRuntime {
    fn core(self) -> RuntimeKind {
        match self {
            Self::Native => RuntimeKind::Native,
            Self::Wasm => RuntimeKind::Wasm,
            Self::Legacy => RuntimeKind::LegacyNode,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Wasm => "wasm",
            Self::Legacy => "legacy",
        }
    }
}

/// A package entry resolved only by the injected Core resolver.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompositionEntryReference {
    pub runtime: CompositionRuntime,
    pub package: String,
}

/// One immutable descriptor submitted by a composition session.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompositionDescriptor {
    pub id: String,
    pub entry: CompositionEntryReference,
    pub config: Value,
}

impl CompositionDescriptor {
    fn validate(&self) -> Result<Entry, TessivumError> {
        if self.id.is_empty() || self.id.len() > MAX_COMPOSITION_ID_BYTES {
            return Err(descriptor_error(
                "id must be a non-empty bounded Core entry identifier",
                json!({"id": bounded(&self.id, MAX_COMPOSITION_ID_BYTES)}),
            ));
        }
        let id = EntryId::new(self.id.clone()).map_err(|error| {
            descriptor_error(
                "id must be a valid Core entry identifier",
                json!({"id": self.id, "error": bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES)}),
            )
        })?;
        if self.entry.package.trim().is_empty()
            || self.entry.package.len() > MAX_COMPOSITION_PACKAGE_BYTES
        {
            return Err(descriptor_error(
                "entry package must be a non-empty bounded reference",
                json!({"runtime": self.entry.runtime.as_str()}),
            ));
        }
        validate_config(&self.config)?;
        let entry = Entry::new(
            self.entry.package.clone(),
            EntryOptions {
                id,
                name: Some(self.id.clone()),
                runtime: self.entry.runtime.core(),
                config: self.config.clone(),
                inject: Vec::new(),
                isolate: Vec::new(),
                intercept: json!({}),
                disabled: false,
                group: None,
            },
        );
        EntryTree {
            entries: vec![entry.clone()],
            groups: Vec::new(),
        }
        .validate()
        .map_err(|error| {
            descriptor_error(
                "descriptor cannot produce a valid Core entry tree",
                json!({"id": self.id, "error": bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES)}),
            )
        })?;
        Ok(entry)
    }
}
fn validate_composition_id(id: &str) -> Result<(), TessivumError> {
    if id.is_empty() || id.len() > MAX_COMPOSITION_ID_BYTES {
        return Err(composition_error(
            "COMPOSITION_ID_INVALID",
            "composition id must be a non-empty bounded Core entry identifier",
            json!({"id": bounded(id, MAX_COMPOSITION_ID_BYTES)}),
        ));
    }
    EntryId::new(id.to_owned()).map_err(|error| {
        composition_error(
            "COMPOSITION_ID_INVALID",
            "composition id must be a valid Core entry identifier",
            json!({"id": id, "error": bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES)}),
        )
    })?;
    Ok(())
}

/// The lifecycle state owned by one descriptor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CompositionLifecycle {
    Draft,
    Validated,
    Active,
}

/// Structured last-failure data retained without unbounded loader diagnostics.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompositionFailure {
    pub code: String,
    pub message: String,
    pub phase: String,
    pub details: Value,
}

/// Observable Core state for one descriptor.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompositionCoreSnapshot {
    pub entry: Entry,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fiber_state: Option<FiberState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_state: Option<FiberState>,
}

/// One session-owned composition observation.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompositionSnapshot {
    pub owner: SessionId,
    pub descriptor: CompositionDescriptor,
    pub lifecycle: CompositionLifecycle,
    pub core: CompositionCoreSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<CompositionFailure>,
}

/// A bounded session inspection result.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompositionInspection {
    pub owner: SessionId,
    pub descriptors: Vec<CompositionSnapshot>,
    pub total: usize,
    pub truncated: bool,
}

/// Lifetime owner for the registry's five composition tool registrations.
pub struct CompositionTools {
    _registrations: Vec<ToolRegistration>,
}

#[derive(Clone)]
pub struct CompositionRegistry {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    catalog: RuntimeCatalog,
    sessions: Mutex<BTreeMap<SessionId, Arc<SessionState>>>,
}

struct RuntimeCatalog {
    resolver: Arc<dyn PackageResolver>,
    runtimes: BTreeMap<RuntimeKind, Arc<dyn LoaderRuntime>>,
}

struct SessionState {
    context: ContextHandle,
    descriptors: AsyncMutex<BTreeMap<String, DescriptorRecord>>,
}

struct DescriptorRecord {
    descriptor: CompositionDescriptor,
    entry: Entry,
    lifecycle: CompositionLifecycle,
    fiber_state: Option<FiberState>,
    last_scope_state: Option<FiberState>,
    last_failure: Option<CompositionFailure>,
    active: Option<ActiveComposition>,
}

struct ActiveComposition {
    handle: Box<dyn RuntimeHandle>,
    scope: Scope,
}

impl CompositionRegistry {
    /// Creates a registry from Host-injected Core package and runtime authority.
    ///
    /// The catalog can contain registered native factories plus the product WASM and Legacy runtimes.
    pub fn new(
        resolver: Arc<dyn PackageResolver>,
        runtimes: impl IntoIterator<Item = Arc<dyn LoaderRuntime>>,
    ) -> Result<Self, TessivumError> {
        let mut catalog = BTreeMap::new();
        for runtime in runtimes {
            let kind = runtime.kind();
            if catalog.insert(kind, runtime).is_some() {
                return Err(composition_error(
                    "COMPOSITION_CATALOG_INVALID",
                    "injected composition runtime catalog contains duplicate Core runtimes",
                    json!({"runtime": runtime_name(kind)}),
                ));
            }
        }
        Ok(Self {
            inner: Arc::new(RegistryInner {
                catalog: RuntimeCatalog {
                    resolver,
                    runtimes: catalog,
                },
                sessions: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    /// Attaches the Core context already owned by one session.
    pub fn attach_session(
        &self,
        owner: SessionId,
        context: ContextHandle,
    ) -> Result<(), TessivumError> {
        let mut sessions = lock(&self.inner.sessions);
        if sessions.contains_key(&owner) {
            return Err(composition_error(
                "COMPOSITION_SESSION_ALREADY_ATTACHED",
                "a composition registry is already attached to this session",
                json!({"owner": owner}),
            ));
        }
        sessions.insert(
            owner,
            Arc::new(SessionState {
                context,
                descriptors: AsyncMutex::new(BTreeMap::new()),
            }),
        );
        Ok(())
    }

    /// Disposes every live entry before removing the session's registry view.
    pub async fn dispose_session(&self, owner: &SessionId) -> Result<(), TessivumError> {
        let session = lock(&self.inner.sessions).remove(owner).ok_or_else(|| {
            composition_error(
                "COMPOSITION_SESSION_UNAVAILABLE",
                "the composition registry is not attached to this session",
                json!({"owner": owner}),
            )
        })?;
        let mut descriptors = session.descriptors.lock().await;
        let mut failures = Vec::new();
        for record in descriptors.values_mut() {
            if let Some(mut active) = record.active.take() {
                if let Err(error) = active.handle.dispose().await {
                    failures.push(bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES));
                }
                if let Err(error) = active.scope.dispose().await {
                    failures.push(bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES));
                }
                record.lifecycle = CompositionLifecycle::Validated;
                record.fiber_state = Some(FiberState::Disposed);
                record.last_scope_state = Some(FiberState::Disposed);
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(composition_error(
                "COMPOSITION_SESSION_DISPOSE_FAILED",
                "one or more composition resources could not be disposed cleanly",
                json!({"owner": owner, "failures": failures}),
            ))
        }
    }

    pub fn register_tools(&self, tools: &ToolRuntime) -> Result<CompositionTools, TessivumError> {
        let registrations = [
            (
                "composition_inspect",
                "Inspect native composition descriptors owned by this session.",
                inspect_schema(),
                ToolKind::Inspect,
            ),
            (
                "composition_define",
                "Define one typed native, WASM, or Legacy Core composition descriptor.",
                define_schema(),
                ToolKind::Define,
            ),
            (
                "composition_validate",
                "Resolve and validate a composition without starting it.",
                id_schema(),
                ToolKind::Validate,
            ),
            (
                "composition_run",
                "Transactionally mount one validated composition under this session.",
                id_schema(),
                ToolKind::Run,
            ),
            (
                "composition_stop",
                "Idempotently dispose one active composition owned by this session.",
                id_schema(),
                ToolKind::Stop,
            ),
        ]
        .into_iter()
        .map(|(name, description, parameters, kind)| {
            tools.register(ToolDefinition::new(
                name,
                description,
                parameters,
                CompositionTool {
                    registry: self.clone(),
                    kind,
                },
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
        Ok(CompositionTools {
            _registrations: registrations,
        })
    }

    pub async fn define(
        &self,
        owner: &SessionId,
        descriptor: CompositionDescriptor,
    ) -> Result<CompositionSnapshot, TessivumError> {
        let entry = descriptor.validate()?;
        let session = self.session(owner)?;
        let mut descriptors = session.descriptors.lock().await;
        if descriptors.contains_key(&descriptor.id) {
            return Err(composition_error(
                "COMPOSITION_DUPLICATE_ID",
                "this session already owns a composition with that id",
                json!({"owner": owner, "id": descriptor.id}),
            ));
        }
        if descriptors.len() >= MAX_COMPOSITIONS_PER_SESSION {
            return Err(composition_error(
                "COMPOSITION_LIMIT_EXCEEDED",
                "the session composition limit has been reached",
                json!({"owner": owner, "max": MAX_COMPOSITIONS_PER_SESSION}),
            ));
        }
        let id = descriptor.id.clone();
        descriptors.insert(
            id.clone(),
            DescriptorRecord {
                descriptor,
                entry,
                lifecycle: CompositionLifecycle::Draft,
                fiber_state: None,
                last_scope_state: None,
                last_failure: None,
                active: None,
            },
        );
        Ok(snapshot(
            owner,
            descriptors
                .get(&id)
                .expect("inserted composition is present"),
        ))
    }

    /// Resolves an entry, instantiates it in a detached child scope, then disposes
    /// the candidate without activating it.
    pub async fn validate(
        &self,
        owner: &SessionId,
        id: &str,
    ) -> Result<CompositionSnapshot, TessivumError> {
        validate_composition_id(id)?;
        let session = self.session(owner)?;
        let mut descriptors = session.descriptors.lock().await;
        let entry = match descriptors.get(id) {
            Some(record) => {
                require_transition(record, "validate", CompositionLifecycle::Draft)?;
                record.entry.clone()
            }
            None => return Err(composition_not_found(id)),
        };
        let result = self.validate_entry(&session.context, &entry).await;
        let record = descriptors
            .get_mut(id)
            .expect("composition cannot disappear while its session is locked");
        match result {
            Ok(()) => {
                record.lifecycle = CompositionLifecycle::Validated;
                record.fiber_state = None;
                record.last_scope_state = None;
                record.last_failure = None;
                Ok(snapshot(owner, record))
            }
            Err(error) => {
                record.last_failure = Some(failure(&error));
                Err(error)
            }
        }
    }

    /// Mounts one Core entry in a fresh session child scope. The entry is only
    /// committed after activation; every unsuccessful candidate is disposed.
    pub async fn run(
        &self,
        owner: &SessionId,
        id: &str,
    ) -> Result<CompositionSnapshot, TessivumError> {
        validate_composition_id(id)?;
        let session = self.session(owner)?;
        let mut descriptors = session.descriptors.lock().await;
        let entry = match descriptors.get(id) {
            Some(record) => {
                require_transition(record, "run", CompositionLifecycle::Validated)?;
                record.entry.clone()
            }
            None => return Err(composition_not_found(id)),
        };
        let (runtime, package) = match self.resolve_entry(&entry).await {
            Ok(prepared) => prepared,
            Err(error) => {
                let record = descriptors
                    .get_mut(id)
                    .expect("composition cannot disappear while its session is locked");
                record.last_failure = Some(failure(&error));
                return Err(error);
            }
        };
        let scope = match session.context.scope().child() {
            Ok(scope) => scope,
            Err(error) => {
                let error = composition_error(
                    "COMPOSITION_SCOPE_UNAVAILABLE",
                    "the session cannot create a composition child scope",
                    json!({"id": id, "error": bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES)}),
                );
                let record = descriptors
                    .get_mut(id)
                    .expect("composition cannot disappear while its session is locked");
                record.last_failure = Some(failure(&error));
                return Err(error);
            }
        };
        let context = session.context.with_scope(scope.clone());
        let mut handle = match runtime.instantiate(package, entry, context).await {
            Ok(handle) => handle,
            Err(error) => {
                let cleanup = scope
                    .dispose()
                    .await
                    .err()
                    .map(|error| bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES))
                    .into_iter()
                    .collect();
                let record = descriptors
                    .get_mut(id)
                    .expect("composition cannot disappear while its session is locked");
                let error = start_failure(id, &record.descriptor.entry, error.to_string(), cleanup);
                record.fiber_state = Some(FiberState::Disposed);
                record.last_scope_state = Some(scope.state());
                record.last_failure = Some(failure(&error));
                return Err(error);
            }
        };
        let start_error = match handle.activation().await {
            Ok(ActivationState::Active) => None,
            Ok(ActivationState::Pending) => {
                Some("required Core dependencies are not available".to_owned())
            }
            Err(error) => Some(error.to_string()),
        };
        if let Some(error) = start_error {
            let mut cleanup = Vec::new();
            if let Err(error) = handle.dispose().await {
                cleanup.push(bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES));
            }
            if let Err(error) = scope.dispose().await {
                cleanup.push(bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES));
            }
            let record = descriptors
                .get_mut(id)
                .expect("composition cannot disappear while its session is locked");
            let error = start_failure(id, &record.descriptor.entry, error, cleanup);
            record.fiber_state = Some(FiberState::Disposed);
            record.last_scope_state = Some(scope.state());
            record.last_failure = Some(failure(&error));
            return Err(error);
        }
        let record = descriptors
            .get_mut(id)
            .expect("composition cannot disappear while its session is locked");
        record.lifecycle = CompositionLifecycle::Active;
        record.fiber_state = Some(FiberState::Active);
        record.last_scope_state = Some(scope.state());
        record.last_failure = None;
        record.active = Some(ActiveComposition { handle, scope });
        Ok(snapshot(owner, record))
    }

    /// Stops a live entry once. Calling stop after a successful stop is a no-op.
    pub async fn stop(
        &self,
        owner: &SessionId,
        id: &str,
    ) -> Result<CompositionSnapshot, TessivumError> {
        validate_composition_id(id)?;
        let session = self.session(owner)?;
        let mut descriptors = session.descriptors.lock().await;
        let Some(record) = descriptors.get_mut(id) else {
            return Err(composition_not_found(id));
        };
        if record.lifecycle == CompositionLifecycle::Validated {
            return Ok(snapshot(owner, record));
        }
        require_transition(record, "stop", CompositionLifecycle::Active)?;
        let runtime = record.descriptor.entry.runtime;
        let package = record.descriptor.entry.package.clone();
        let mut active = record
            .active
            .take()
            .expect("active lifecycle always retains its Core runtime handle");
        let mut failures = Vec::new();
        if let Err(error) = active.handle.dispose().await {
            failures.push(bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES));
        }
        if let Err(error) = active.scope.dispose().await {
            failures.push(bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES));
        }
        record.lifecycle = CompositionLifecycle::Validated;
        record.fiber_state = Some(FiberState::Disposed);
        record.last_scope_state = Some(active.scope.state());
        if failures.is_empty() {
            record.last_failure = None;
            Ok(snapshot(owner, record))
        } else {
            let error = composition_error(
                "COMPOSITION_STOP_FAILED",
                "one or more composition resources could not be disposed cleanly",
                json!({
                    "id": id,
                    "runtime": runtime.as_str(),
                    "package": bounded(&package, MAX_COMPOSITION_PACKAGE_BYTES),
                    "failures": failures,
                }),
            );
            record.last_failure = Some(failure(&error));
            Err(error)
        }
    }

    pub async fn inspect(
        &self,
        owner: &SessionId,
        id: Option<&str>,
    ) -> Result<CompositionInspection, TessivumError> {
        if let Some(id) = id {
            validate_composition_id(id)?;
        }
        let session = self.session(owner)?;
        let descriptors = session.descriptors.lock().await;
        let rows = match id {
            Some(id) => match descriptors.get(id) {
                Some(record) => vec![snapshot(owner, record)],
                None => return Err(composition_not_found(id)),
            },
            None => descriptors
                .values()
                .take(MAX_COMPOSITIONS_PER_SESSION)
                .map(|record| snapshot(owner, record))
                .collect(),
        };
        let total = match id {
            Some(_) => rows.len(),
            None => descriptors.len(),
        };
        Ok(CompositionInspection {
            owner: owner.clone(),
            truncated: total > rows.len(),
            descriptors: rows,
            total,
        })
    }

    fn session(&self, owner: &SessionId) -> Result<Arc<SessionState>, TessivumError> {
        lock(&self.inner.sessions)
            .get(owner)
            .cloned()
            .ok_or_else(|| {
                composition_error(
                    "COMPOSITION_SESSION_UNAVAILABLE",
                    "the composition registry is not attached to this session",
                    json!({"owner": owner}),
                )
            })
    }

    async fn resolve_entry(
        &self,
        entry: &Entry,
    ) -> Result<(Arc<dyn LoaderRuntime>, ResolvedPackage), TessivumError> {
        let runtime = self
            .inner
            .catalog
            .runtimes
            .get(&entry.options.runtime)
            .cloned()
            .ok_or_else(|| {
                composition_error(
                    "COMPOSITION_RUNTIME_UNAVAILABLE",
                    "no injected Core runtime supports this composition entry",
                    json!({
                        "runtime": runtime_name(entry.options.runtime),
                        "package": bounded(&entry.package, MAX_COMPOSITION_PACKAGE_BYTES),
                    }),
                )
            })?;
        let package = self
            .inner
            .catalog
            .resolver
            .resolve(&entry.package, entry.options.runtime)
            .await
            .map_err(|error| {
                composition_error(
                    "COMPOSITION_SOURCE_UNRESOLVED",
                    "the composition entry source could not be resolved",
                    json!({
                        "runtime": runtime_name(entry.options.runtime),
                        "package": bounded(&entry.package, MAX_COMPOSITION_PACKAGE_BYTES),
                        "error": bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES),
                    }),
                )
            })?;
        Ok((runtime, package))
    }

    async fn validate_entry(
        &self,
        session_context: &ContextHandle,
        entry: &Entry,
    ) -> Result<(), TessivumError> {
        let (runtime, package) = self.resolve_entry(entry).await?;

        let scope = session_context.scope().child().map_err(|error| {
            composition_error(
                "COMPOSITION_SCOPE_UNAVAILABLE",
                "the session cannot create a composition validation child scope",
                json!({"error": bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES)}),
            )
        })?;
        let context = session_context.with_scope(scope.clone());
        let mut handle = match runtime.instantiate(package, entry.clone(), context).await {
            Ok(handle) => handle,
            Err(error) => {
                let cleanup = scope
                    .dispose()
                    .await
                    .err()
                    .map(|cleanup| bounded(&cleanup.to_string(), MAX_FAILURE_TEXT_BYTES));
                return Err(validation_error(
                    entry,
                    "instantiate",
                    match cleanup {
                        Some(cleanup) => format!("{error}; cleanup: {cleanup}"),
                        None => error.to_string(),
                    },
                ));
            }
        };
        let dispose = handle.dispose().await.err();
        let scope_dispose = scope.dispose().await.err();
        match (dispose, scope_dispose) {
            (None, None) => Ok(()),
            (dispose, scope_dispose) => Err(validation_error(
                entry,
                "cleanup",
                format!(
                    "{}{}",
                    dispose
                        .as_ref()
                        .map(|error| error.to_string())
                        .unwrap_or_default(),
                    scope_dispose
                        .as_ref()
                        .map(|error| format!("; scope cleanup: {error}"))
                        .unwrap_or_default(),
                ),
            )),
        }
    }

    fn composition_not_found(id: &str) -> TessivumError {
        composition_error(
            "COMPOSITION_NOT_FOUND",
            "the requested composition is not defined for this session",
            json!({"id": bounded(id, MAX_COMPOSITION_ID_BYTES)}),
        )
    }
}

#[derive(Clone, Copy)]
enum ToolKind {
    Inspect,
    Define,
    Validate,
    Run,
    Stop,
}

struct CompositionTool {
    registry: CompositionRegistry,
    kind: ToolKind,
}

#[async_trait]
impl ToolHandler for CompositionTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        match self.kind {
            ToolKind::Inspect => {
                let request: InspectRequest = parse_request(arguments)?;
                success(
                    self.registry
                        .inspect(&context.session, request.id.as_deref())
                        .await?,
                )
            }
            ToolKind::Define => {
                let request: DefineRequest = parse_request(arguments)?;
                success(
                    self.registry
                        .define(&context.session, request.descriptor)
                        .await?,
                )
            }
            ToolKind::Validate => {
                let request: IdRequest = parse_request(arguments)?;
                success(
                    self.registry
                        .validate(&context.session, &request.id)
                        .await?,
                )
            }
            ToolKind::Run => {
                let request: IdRequest = parse_request(arguments)?;
                success(self.registry.run(&context.session, &request.id).await?)
            }
            ToolKind::Stop => {
                let request: IdRequest = parse_request(arguments)?;
                success(self.registry.stop(&context.session, &request.id).await?)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InspectRequest {
    id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DefineRequest {
    descriptor: CompositionDescriptor,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IdRequest {
    id: String,
}

fn parse_request<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, TessivumError> {
    serde_json::from_value(value).map_err(|error| {
        composition_error(
            "COMPOSITION_REQUEST_INVALID",
            "composition tool arguments do not match the native descriptor contract",
            json!({"error": bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES)}),
        )
    })
}

fn success<T: Serialize>(value: T) -> ToolHandlerResult {
    let meta = serde_json::to_value(value).map_err(|error| {
        composition_error(
            "COMPOSITION_RESPONSE_ENCODING_FAILED",
            "composition snapshot could not be encoded",
            json!({"error": bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES)}),
        )
    })?;
    let text = serde_json::to_string(&meta).map_err(|error| {
        composition_error(
            "COMPOSITION_RESPONSE_ENCODING_FAILED",
            "composition snapshot could not be encoded",
            json!({"error": bounded(&error.to_string(), MAX_FAILURE_TEXT_BYTES)}),
        )
    })?;
    Ok(ToolOutput::new(
        vec![ContentBlock::Text { text }],
        false,
        meta,
    ))
}

fn snapshot(owner: &SessionId, record: &DescriptorRecord) -> CompositionSnapshot {
    let scope_state = record
        .active
        .as_ref()
        .map(|active| active.scope.state())
        .or(record.last_scope_state);
    CompositionSnapshot {
        owner: owner.clone(),
        descriptor: record.descriptor.clone(),
        lifecycle: record.lifecycle,
        core: CompositionCoreSnapshot {
            entry: record.entry.clone(),
            fiber_state: record.fiber_state,
            scope_state,
        },
        last_failure: record.last_failure.clone(),
    }
}

fn require_transition(
    record: &DescriptorRecord,
    operation: &str,
    expected: CompositionLifecycle,
) -> Result<(), TessivumError> {
    if record.lifecycle == expected {
        return Ok(());
    }
    Err(composition_error(
        "COMPOSITION_INVALID_TRANSITION",
        "the requested operation is invalid for the composition lifecycle state",
        json!({
            "id": record.descriptor.id,
            "operation": operation,
            "lifecycle": record.lifecycle,
            "expected": expected,
        }),
    ))
}

fn validate_config(config: &Value) -> Result<(), TessivumError> {
    if !config.is_object() {
        return Err(descriptor_error(
            "config must be a JSON object",
            Value::Null,
        ));
    }
    let size = serde_json::to_vec(config)
        .map_err(|error| {
            descriptor_error(
                "config cannot be encoded as JSON",
                json!({"error": error.to_string()}),
            )
        })?
        .len();
    if size > MAX_COMPOSITION_CONFIG_BYTES {
        return Err(descriptor_error(
            "config exceeds the composition document limit",
            json!({"maxBytes": MAX_COMPOSITION_CONFIG_BYTES, "bytes": size}),
        ));
    }
    let mut nodes = 0;
    validate_config_value(config, 0, &mut nodes)
}

fn validate_config_value(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), TessivumError> {
    if depth > MAX_COMPOSITION_CONFIG_DEPTH {
        return Err(descriptor_error(
            "config nesting exceeds the composition document limit",
            json!({"maxDepth": MAX_COMPOSITION_CONFIG_DEPTH}),
        ));
    }
    *nodes += 1;
    if *nodes > MAX_COMPOSITION_CONFIG_NODES {
        return Err(descriptor_error(
            "config contains too many JSON values",
            json!({"maxNodes": MAX_COMPOSITION_CONFIG_NODES}),
        ));
    }
    match value {
        Value::Array(values) => {
            for value in values {
                validate_config_value(value, depth + 1, nodes)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_config_value(value, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn descriptor_error(message: impl Into<String>, details: Value) -> TessivumError {
    composition_error("COMPOSITION_DESCRIPTOR_INVALID", message, details)
}

fn composition_error(
    code: impl Into<String>,
    message: impl Into<String>,
    details: Value,
) -> TessivumError {
    TessivumError::new(code, message, "composition", details)
}

fn failure(error: &TessivumError) -> CompositionFailure {
    CompositionFailure {
        code: error.code.clone(),
        message: bounded(&error.message, MAX_FAILURE_TEXT_BYTES),
        phase: error.phase.clone(),
        details: error.details.clone(),
    }
}

fn validation_error(entry: &Entry, step: &str, error: String) -> TessivumError {
    composition_error(
        "COMPOSITION_VALIDATION_FAILED",
        "the composition could not be validated without starting",
        json!({
            "runtime": runtime_name(entry.options.runtime),
            "package": bounded(&entry.package, MAX_COMPOSITION_PACKAGE_BYTES),
            "step": step,
            "error": bounded(&error, MAX_FAILURE_TEXT_BYTES),
        }),
    )
}

fn start_failure(
    id: &str,
    entry: &CompositionEntryReference,
    error: String,
    cleanup: Vec<String>,
) -> TessivumError {
    let mut details = Map::new();
    details.insert(
        "id".into(),
        Value::String(bounded(id, MAX_COMPOSITION_ID_BYTES)),
    );
    details.insert(
        "runtime".into(),
        Value::String(entry.runtime.as_str().into()),
    );
    details.insert(
        "package".into(),
        Value::String(bounded(&entry.package, MAX_COMPOSITION_PACKAGE_BYTES)),
    );
    details.insert(
        "error".into(),
        Value::String(bounded(&error, MAX_FAILURE_TEXT_BYTES)),
    );
    if !cleanup.is_empty() {
        details.insert(
            "rollback".into(),
            Value::Array(cleanup.into_iter().map(Value::String).collect()),
        );
    }
    composition_error(
        "COMPOSITION_START_FAILED",
        "the composition failed to start and its child scope was rolled back",
        Value::Object(details),
    )
}

fn runtime_name(runtime: RuntimeKind) -> &'static str {
    match runtime {
        RuntimeKind::Native => "native",
        RuntimeKind::Wasm => "wasm",
        RuntimeKind::LegacyNode => "legacy",
    }
}

fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.into();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"id": {"type": "string"}},
        "required": ["id"],
        "additionalProperties": false,
    })
}

fn inspect_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"id": {"type": "string"}},
        "additionalProperties": false,
    })
}

fn define_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "descriptor": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "entry": {
                        "type": "object",
                        "properties": {
                            "runtime": {"type": "string", "enum": ["native", "wasm", "legacy"]},
                            "package": {"type": "string"},
                        },
                        "required": ["runtime", "package"],
                        "additionalProperties": false,
                    },
                    "config": {"type": "object", "properties": {}, "additionalProperties": true},
                },
                "required": ["id", "entry", "config"],
                "additionalProperties": false,
            },
        },
        "required": ["descriptor"],
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use serde_json::json;
    use tessivum_core::{
        ContextHandle, LoaderFuture, NativeConfigSchema, NativePlugin, NativePluginDescriptor,
        NativePluginError, NativePluginFuture, NativePluginRuntime, ResolvedPackage,
    };

    use super::*;

    struct Resolver;

    impl PackageResolver for Resolver {
        fn resolve<'a>(
            &'a self,
            specifier: &'a str,
            _runtime: RuntimeKind,
        ) -> LoaderFuture<'a, ResolvedPackage> {
            let specifier = specifier.to_owned();
            Box::pin(async move {
                Ok(ResolvedPackage {
                    location: specifier.clone(),
                    specifier,
                })
            })
        }
    }


    struct FixturePlugin {
        live: Arc<AtomicUsize>,
        fail_start: bool,
    }

    impl NativePlugin for FixturePlugin {
        fn descriptor(&self) -> NativePluginDescriptor {
            NativePluginDescriptor {
                name: "composition-fixture".into(),
                version: "1.0.0".into(),
                dependencies: Vec::new(),
                config_schema: NativeConfigSchema::Any,
            }
        }

        fn start<'a>(
            &'a mut self,
            _context: ContextHandle,
            _config: &'a Value,
        ) -> NativePluginFuture<'a> {
            let live = Arc::clone(&self.live);
            let fail_start = self.fail_start;
            Box::pin(async move {
                live.fetch_add(1, Ordering::AcqRel);
                if fail_start {
                    return Err(NativePluginError::plugin(
                        tessivum_core::NativePluginPhase::Start,
                        "fixture start failure",
                    ));
                }
                Ok(())
            })
        }

        fn update<'a>(
            &'a mut self,
            _context: ContextHandle,
            _config: &'a Value,
        ) -> NativePluginFuture<'a> {
            Box::pin(async { Ok(()) })
        }

        fn stop<'a>(&'a mut self, _context: ContextHandle) -> NativePluginFuture<'a> {
            let live = Arc::clone(&self.live);
            Box::pin(async move {
                live.fetch_sub(1, Ordering::AcqRel);
                Ok(())
            })
        }
    }

    fn registry(live: Arc<AtomicUsize>, fail_start: bool) -> CompositionRegistry {
        registry_with_probe(live, fail_start).0
    }

    fn registry_with_probe(
        live: Arc<AtomicUsize>,
        fail_start: bool,
    ) -> (CompositionRegistry, Arc<AtomicUsize>) {
        let instantiated = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::clone(&instantiated);
        let mut native = NativePluginRuntime::new();
        native
            .register("fixture", move || {
                factory_calls.fetch_add(1, Ordering::AcqRel);
                FixturePlugin {
                    live: Arc::clone(&live),
                    fail_start,
                }
            })
            .unwrap();
        (
            CompositionRegistry::new(
                Arc::new(Resolver),
                [Arc::new(native) as Arc<dyn LoaderRuntime>],
            )
            .unwrap(),
            instantiated,
        )
    }

    fn descriptor(id: &str) -> CompositionDescriptor {
        CompositionDescriptor {
            id: id.into(),
            entry: CompositionEntryReference {
                runtime: CompositionRuntime::Native,
                package: "fixture".into(),
            },
            config: json!({}),
        }
    }

    #[test]
    fn descriptors_reject_entry_source_but_allow_opaque_bounded_config() {
        let unknown = serde_json::from_value::<CompositionDescriptor>(json!({
            "id": "fixture-entry",
            "entry": {"runtime": "native", "package": "fixture", "source": "ignored"},
            "config": {},
        }));
        assert!(unknown.is_err());

        let mut descriptor = descriptor("fixture-entry");
        descriptor.config = json!({"script": "opaque registered-plugin config"});
        descriptor.validate().unwrap();
    }

    #[tokio::test]
    async fn native_composition_define_validate_run_inspect_and_stop_has_no_live_resources() {
        let root = ContextHandle::root();
        let live = Arc::new(AtomicUsize::new(0));
        let (registry, instantiated) = registry_with_probe(Arc::clone(&live), false);
        let owner = SessionId::from("owner");
        registry
            .attach_session(owner.clone(), root.clone())
            .unwrap();

        assert_eq!(
            registry
                .define(&owner, descriptor("fixture-entry"))
                .await
                .unwrap()
                .lifecycle,
            CompositionLifecycle::Draft
        );
        assert_eq!(
            registry
                .validate(&owner, "fixture-entry")
                .await
                .unwrap()
                .lifecycle,
            CompositionLifecycle::Validated
        );
        assert_eq!(
            live.load(Ordering::Acquire),
            0,
            "validation must not start the native plugin"
        );
        assert_eq!(
            instantiated.load(Ordering::Acquire),
            1,
            "validation must instantiate one detached candidate"
        );
        let active = registry.run(&owner, "fixture-entry").await.unwrap();
        assert_eq!(active.lifecycle, CompositionLifecycle::Active);
        assert_eq!(active.core.fiber_state, Some(FiberState::Active));
        assert_eq!(live.load(Ordering::Acquire), 1);
        assert_eq!(instantiated.load(Ordering::Acquire), 2);
        assert_eq!(
            registry
                .inspect(&owner, Some("fixture-entry"))
                .await
                .unwrap()
                .descriptors
                .len(),
            1
        );
        let stopped = registry.stop(&owner, "fixture-entry").await.unwrap();
        assert_eq!(stopped.lifecycle, CompositionLifecycle::Validated);
        assert_eq!(stopped.core.fiber_state, Some(FiberState::Disposed));
        assert_eq!(live.load(Ordering::Acquire), 0);
        registry.stop(&owner, "fixture-entry").await.unwrap();
        assert_eq!(live.load(Ordering::Acquire), 0);
        root.scope().dispose().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_transitions_and_cross_session_reads() {
        let root = ContextHandle::root();
        let registry = registry(Arc::new(AtomicUsize::new(0)), false);
        let owner = SessionId::from("owner");
        let other = SessionId::from("other");
        registry
            .attach_session(owner.clone(), root.clone())
            .unwrap();
        registry
            .attach_session(other.clone(), root.clone())
            .unwrap();
        registry
            .define(&owner, descriptor("fixture-entry"))
            .await
            .unwrap();
        assert_eq!(
            registry
                .define(&owner, descriptor("fixture-entry"))
                .await
                .unwrap_err()
                .code,
            "COMPOSITION_DUPLICATE_ID"
        );

        assert_eq!(
            registry
                .run(&owner, "fixture-entry")
                .await
                .unwrap_err()
                .code,
            "COMPOSITION_INVALID_TRANSITION"
        );
        assert_eq!(
            registry
                .inspect(&other, Some("fixture-entry"))
                .await
                .unwrap_err()
                .code,
            "COMPOSITION_NOT_FOUND"
        );
        root.scope().dispose().await.unwrap();
    }

    #[tokio::test]
    async fn failed_start_rolls_back_the_native_fiber() {
        let root = ContextHandle::root();
        let live = Arc::new(AtomicUsize::new(0));
        let registry = registry(Arc::clone(&live), true);
        let owner = SessionId::from("owner");
        registry
            .attach_session(owner.clone(), root.clone())
            .unwrap();
        registry
            .define(&owner, descriptor("fixture-entry"))
            .await
            .unwrap();
        registry.validate(&owner, "fixture-entry").await.unwrap();

        assert_eq!(
            registry
                .run(&owner, "fixture-entry")
                .await
                .unwrap_err()
                .code,
            "COMPOSITION_START_FAILED"
        );
        assert_eq!(live.load(Ordering::Acquire), 0);
        let snapshot = registry
            .inspect(&owner, Some("fixture-entry"))
            .await
            .unwrap()
            .descriptors
            .pop()
            .unwrap();
        assert_eq!(snapshot.lifecycle, CompositionLifecycle::Validated);
        assert_eq!(
            snapshot
                .last_failure
                .as_ref()
                .map(|failure| failure.code.as_str()),
            Some("COMPOSITION_START_FAILED")
        );
        root.scope().dispose().await.unwrap();
    }

    #[tokio::test]
    async fn registers_exactly_five_session_routed_tools() {
        let root = ContextHandle::root();
        let live = Arc::new(AtomicUsize::new(0));
        let registry = registry(Arc::clone(&live), false);
        let owner = SessionId::from("owner");
        registry
            .attach_session(owner.clone(), root.clone())
            .unwrap();
        let tools = ToolRuntime::new();
        let _tools = registry.register_tools(&tools).unwrap();
        assert_eq!(
            tools
                .schemas()
                .into_iter()
                .map(|schema| schema.name)
                .collect::<Vec<_>>(),
            vec![
                "composition_define",
                "composition_inspect",
                "composition_run",
                "composition_stop",
                "composition_validate",
            ]
        );
        let context = |call: &str| ToolRunContext {
            session: owner.clone(),
            call: call.into(),
            cancellation: root.scope().cancellation(),
        };
        assert!(
            !tools
                .execute(
                    context("define"),
                    "composition_define",
                    json!({"descriptor": descriptor("fixture-entry")}),
                )
                .await
                .is_error
        );
        for (call, tool) in [
            ("validate", "composition_validate"),
            ("run", "composition_run"),
        ] {
            assert!(
                !tools
                    .execute(context(call), tool, json!({"id": "fixture-entry"}))
                    .await
                    .is_error
            );
        }
        assert!(
            !tools
                .execute(
                    context("stop"),
                    "composition_stop",
                    json!({"id": "fixture-entry"}),
                )
                .await
                .is_error
        );
        assert_eq!(live.load(Ordering::Acquire), 0);
        root.scope().dispose().await.unwrap();
    }
}

//! Scoped, cancellable skill discovery and model-facing invocation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};

use crate::{
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    ContentBlock, SessionId, TessivumError,
};

const DEFAULT_MAX_CATALOG_ENTRIES: usize = 256;
const DEFAULT_MAX_SKILL_BYTES: usize = 1_048_576;
const DEFAULT_MAX_RESOURCES: usize = 1_024;
const DEFAULT_MAX_RESOURCE_BYTES: usize = 1_048_576;
const MODEL_CATALOG_DESCRIPTION_MAX: usize = 500;

/// Stable key for the scoped skill capability.
pub fn skills_service_key() -> ServiceKey {
    ServiceKey::new("harness.skills", "1")
}

/// Per-session skill capability scopes selected by the host composition.
pub type SkillSessionScopes = Arc<Mutex<BTreeMap<SessionId, PathBuf>>>;

pub fn skill_session_scopes() -> SkillSessionScopes {
    Arc::new(Mutex::new(BTreeMap::new()))
}

macro_rules! opaque_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[repr(transparent)]
        #[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Preserves a provider-issued opaque value without interpreting it.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
    };
}

opaque_id!(SkillLocator, "Opaque provider-issued skill identity.");
opaque_id!(
    SkillResourceBase,
    "Opaque base identity for a skill's resources."
);

/// Invocation surfaces permitted by a discovered skill.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInvocation {
    pub model_invocable: bool,
    pub user_invocable: bool,
}

impl Default for SkillInvocation {
    fn default() -> Self {
        Self {
            model_invocable: true,
            user_invocable: true,
        }
    }
}

/// Model-safe summary returned by a provider during discovery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillListing {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default)]
    pub invocation: SkillInvocation,
    pub locator: SkillLocator,
    pub resource_base: SkillResourceBase,
}

impl SkillListing {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        locator: impl Into<SkillLocator>,
        resource_base: impl Into<SkillResourceBase>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            when_to_use: None,
            invocation: SkillInvocation::default(),
            locator: locator.into(),
            resource_base: resource_base.into(),
        }
    }

    fn with_frontmatter(
        mut self,
        when_to_use: Option<String>,
        invocation: SkillInvocation,
    ) -> Self {
        self.when_to_use = when_to_use;
        self.invocation = invocation;
        self
    }
}

/// A listed resource relative to an opaque [`SkillResourceBase`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SkillResource {
    pub path: String,
}

/// A bounded, loaded skill. Native paths never appear in this value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    pub locator: SkillLocator,
    pub resource_base: SkillResourceBase,
    pub body: String,
    #[serde(default)]
    pub resources: Vec<SkillResource>,
}

/// A policy-approved load paired with the provider selected by scoped discovery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvokedSkill {
    pub provider: String,
    #[serde(flatten)]
    pub skill: LoadedSkill,
}

/// Bounded text read from a listed resource.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SkillResourceContent {
    pub path: String,
    pub text: String,
}

/// A provider has only two roles: discover skills and load a provider-issued locator.
#[async_trait]
pub trait SkillProvider: Send + Sync {
    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SkillListing>, TessivumError>;

    async fn get(
        &self,
        locator: &SkillLocator,
        cancellation: CancellationToken,
    ) -> Result<LoadedSkill, TessivumError>;

    /// Providers with resources override this method. A locator and resource path
    /// are both untrusted and must be revalidated by the provider.
    async fn read_resource(
        &self,
        _locator: &SkillLocator,
        _path: &str,
        _max_bytes: usize,
        _cancellation: CancellationToken,
    ) -> Result<SkillResourceContent, TessivumError> {
        Err(skill_error(
            "SKILL_RESOURCE_UNAVAILABLE",
            "this skill provider does not expose resources",
            Value::Null,
        ))
    }
}

/// A catalog winner after scoped provider selection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillCatalogEntry {
    #[serde(flatten)]
    pub skill: SkillListing,
    pub provider: String,
}

/// A model catalog snapshot. `complete = false` means one or more providers
/// failed and this snapshot contains their last successful contribution, if any.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillCatalog {
    pub revision: u64,
    pub complete: bool,
    pub skills: Vec<SkillCatalogEntry>,
}

/// Provider-registry changes. Revision is bumped only after the in-memory state
/// has been admitted, before observers run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum SkillChange {
    Registered {
        provider: String,
        revision: u64,
    },
    Invalidated {
        provider: Option<String>,
        revision: u64,
    },
    Removed {
        provider: String,
        revision: u64,
    },
    Refreshed {
        provider: String,
        revision: u64,
    },
}

/// Observes admitted registry changes without access to providers or bodies.
pub trait SkillChangeObserver: Send + Sync {
    fn observe(&self, change: &SkillChange);
}

impl<F> SkillChangeObserver for F
where
    F: for<'a> Fn(&'a SkillChange) + Send + Sync,
{
    fn observe(&self, change: &SkillChange) {
        self(change);
    }
}

/// Limits applied before provider data becomes model-visible.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillLimits {
    pub max_catalog_entries: usize,
    pub max_skill_bytes: usize,
    pub max_resources: usize,
    pub max_resource_bytes: usize,
}

impl Default for SkillLimits {
    fn default() -> Self {
        Self {
            max_catalog_entries: DEFAULT_MAX_CATALOG_ENTRIES,
            max_skill_bytes: DEFAULT_MAX_SKILL_BYTES,
            max_resources: DEFAULT_MAX_RESOURCES,
            max_resource_bytes: DEFAULT_MAX_RESOURCE_BYTES,
        }
    }
}

impl SkillLimits {
    fn validate(&self) -> Result<(), TessivumError> {
        if self.max_catalog_entries == 0
            || self.max_skill_bytes == 0
            || self.max_resources == 0
            || self.max_resource_bytes == 0
        {
            return Err(skill_error(
                "INVALID_SKILL_LIMITS",
                "skill limits must be positive",
                Value::Null,
            ));
        }
        Ok(())
    }
}

/// The two policy checkpoints surrounding a provider load.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillPolicyStage {
    BeforeLoad,
    AfterLoad,
}

/// Policy is deliberately checked both before I/O and after parsing. The second
/// check prevents a skill revoked while it was loading from reaching the model.
#[async_trait]
pub trait SkillInvocationPolicy: Send + Sync {
    async fn allow(
        &self,
        stage: SkillPolicyStage,
        listing: &SkillListing,
        loaded: Option<&LoadedSkill>,
        cancellation: CancellationToken,
    ) -> Result<bool, TessivumError>;
}

/// The explicit no-restriction policy for hosts that do not need an approval gate.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowSkillInvocation;

#[async_trait]
impl SkillInvocationPolicy for AllowSkillInvocation {
    async fn allow(
        &self,
        _stage: SkillPolicyStage,
        _listing: &SkillListing,
        _loaded: Option<&LoadedSkill>,
        cancellation: CancellationToken,
    ) -> Result<bool, TessivumError> {
        check_cancelled(&cancellation)?;
        Ok(true)
    }
}

#[derive(Clone)]
struct ProviderSnapshot {
    id: u64,
    provider: String,
    source: Arc<dyn SkillProvider>,
    scope: Option<PathBuf>,
    rank: i64,
}

struct ProviderSlot {
    id: u64,
    source: Arc<dyn SkillProvider>,
    scope: Option<PathBuf>,
    rank: i64,
    last_good: Option<Vec<SkillListing>>,
}

#[derive(Default)]
struct SkillState {
    next_id: u64,
    revision: u64,
    providers: BTreeMap<String, ProviderSlot>,
    observers: BTreeMap<u64, Arc<dyn SkillChangeObserver>>,
}

struct SkillInner {
    state: Mutex<SkillState>,
    limits: SkillLimits,
}

/// Thread-safe provider registry. Provider calls always happen outside its lock.
#[derive(Clone)]
pub struct SkillRuntime {
    inner: Arc<SkillInner>,
}

impl Default for SkillRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SkillRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.inner.state);
        formatter
            .debug_struct("SkillRuntime")
            .field("revision", &state.revision)
            .field("providers", &state.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl SkillRuntime {
    pub fn new() -> Self {
        Self::with_limits(SkillLimits::default()).expect("default skill limits are valid")
    }

    pub fn with_limits(limits: SkillLimits) -> Result<Self, TessivumError> {
        limits.validate()?;
        Ok(Self {
            inner: Arc::new(SkillInner {
                state: Mutex::new(SkillState::default()),
                limits,
            }),
        })
    }

    pub fn publish(
        &self,
        context: &ContextHandle,
    ) -> Result<ServiceHandle<SkillRuntime>, CoreError> {
        context.provide(skills_service_key(), self.clone())
    }

    /// Registers one scoped provider. A provider is considered only when its
    /// scope contains the requested working directory.
    pub fn register(
        &self,
        provider: impl Into<String>,
        source: Arc<dyn SkillProvider>,
        scope: impl Into<PathBuf>,
        rank: i64,
    ) -> Result<SkillProviderRegistration, TessivumError> {
        self.register_inner(
            provider.into(),
            source,
            Some(canonical_scope(scope.into())?),
            rank,
        )
    }

    /// Registers a provider visible from every working directory.
    pub fn register_global(
        &self,
        provider: impl Into<String>,
        source: Arc<dyn SkillProvider>,
        rank: i64,
    ) -> Result<SkillProviderRegistration, TessivumError> {
        self.register_inner(provider.into(), source, None, rank)
    }

    fn register_inner(
        &self,
        provider: String,
        source: Arc<dyn SkillProvider>,
        scope: Option<PathBuf>,
        rank: i64,
    ) -> Result<SkillProviderRegistration, TessivumError> {
        validate_provider_name(&provider)?;
        let (id, revision, observers) = {
            let mut state = lock(&self.inner.state);
            if state.providers.contains_key(&provider) {
                return Err(skill_error(
                    "DUPLICATE_SKILL_PROVIDER",
                    "a skill provider is already registered for this name",
                    json!({"provider": provider}),
                ));
            }
            state.next_id = state.next_id.wrapping_add(1);
            let id = state.next_id;
            let revision = bump_revision(&mut state);
            state.providers.insert(
                provider.clone(),
                ProviderSlot {
                    id,
                    source,
                    scope,
                    rank,
                    last_good: None,
                },
            );
            (
                id,
                revision,
                state.observers.values().cloned().collect::<Vec<_>>(),
            )
        };
        notify(
            observers,
            SkillChange::Registered {
                provider: provider.clone(),
                revision,
            },
        );
        Ok(SkillProviderRegistration {
            inner: Arc::downgrade(&self.inner),
            provider,
            id,
            closed: AtomicBool::new(false),
        })
    }

    /// Calls an observer after future admitted changes. Dropping the returned
    /// subscription removes it without changing the catalog revision.
    pub fn on_change<F>(&self, observer: F) -> SkillChangeSubscription
    where
        F: SkillChangeObserver + 'static,
    {
        let id = {
            let mut state = lock(&self.inner.state);
            state.next_id = state.next_id.wrapping_add(1);
            let id = state.next_id;
            state.observers.insert(id, Arc::new(observer));
            id
        };
        SkillChangeSubscription {
            inner: Arc::downgrade(&self.inner),
            id,
            closed: AtomicBool::new(false),
        }
    }

    /// Removes cached discovery results. The next catalog request must attempt
    /// the affected provider again and reports a fresh monotonic revision.
    pub fn invalidate(&self, provider: Option<&str>) -> Result<u64, TessivumError> {
        let provider = provider.map(str::to_owned);
        let (revision, observers) = {
            let mut state = lock(&self.inner.state);
            if let Some(provider) = &provider {
                let slot = state.providers.get_mut(provider).ok_or_else(|| {
                    skill_error(
                        "SKILL_PROVIDER_NOT_FOUND",
                        "the requested skill provider is not registered",
                        json!({"provider": provider}),
                    )
                })?;
                slot.last_good = None;
            } else {
                for slot in state.providers.values_mut() {
                    slot.last_good = None;
                }
            }
            let revision = bump_revision(&mut state);
            (
                revision,
                state.observers.values().cloned().collect::<Vec<_>>(),
            )
        };
        notify(observers, SkillChange::Invalidated { provider, revision });
        Ok(revision)
    }

    /// Lists scoped winner skills. A provider failure returns the most recent
    /// good result and marks the catalog incomplete instead of hiding it.
    pub async fn catalog(
        &self,
        cwd: impl AsRef<Path>,
        cancellation: CancellationToken,
    ) -> Result<SkillCatalog, TessivumError> {
        let (_, catalog) = self
            .catalog_with_candidates(cwd.as_ref(), cancellation)
            .await?;
        Ok(catalog)
    }

    /// Loads a selected skill without an invocation gate. Hosts should prefer
    /// [`Self::invoke`] for data that can reach a model.
    pub async fn get(
        &self,
        cwd: impl AsRef<Path>,
        name: &str,
        cancellation: CancellationToken,
    ) -> Result<LoadedSkill, TessivumError> {
        let (candidates, _) = self
            .catalog_with_candidates(cwd.as_ref(), cancellation.clone())
            .await?;
        let candidate = candidates
            .get(name)
            .cloned()
            .ok_or_else(|| skill_not_found(name))?;
        self.load_candidate(&candidate, cancellation).await
    }

    /// Enforces policy before and after provider load, then returns model-safe
    /// content. A post-load denial never returns the loaded body to the caller.
    pub async fn invoke(
        &self,
        cwd: impl AsRef<Path>,
        name: &str,
        policy: &dyn SkillInvocationPolicy,
        cancellation: CancellationToken,
    ) -> Result<LoadedSkill, TessivumError> {
        Ok(self
            .invoke_with_provider(cwd, name, policy, cancellation)
            .await?
            .skill)
    }

    /// Like [`Self::invoke`], retaining the winner provider for the model-facing
    /// contract without exposing its native locator.
    pub async fn invoke_with_provider(
        &self,
        cwd: impl AsRef<Path>,
        name: &str,
        policy: &dyn SkillInvocationPolicy,
        cancellation: CancellationToken,
    ) -> Result<InvokedSkill, TessivumError> {
        check_cancelled(&cancellation)?;
        let (candidates, _) = self
            .catalog_with_candidates(cwd.as_ref(), cancellation.clone())
            .await?;
        let candidate = candidates
            .get(name)
            .cloned()
            .ok_or_else(|| skill_not_found(name))?;
        if !candidate.listing.invocation.model_invocable {
            return Err(skill_not_model_invocable(name));
        }
        if !policy
            .allow(
                SkillPolicyStage::BeforeLoad,
                &candidate.listing,
                None,
                cancellation.clone(),
            )
            .await?
        {
            return Err(skill_denied(SkillPolicyStage::BeforeLoad, name));
        }
        check_cancelled(&cancellation)?;
        let skill = self
            .load_candidate(&candidate, cancellation.clone())
            .await?;
        check_cancelled(&cancellation)?;
        if !policy
            .allow(
                SkillPolicyStage::AfterLoad,
                &candidate.listing,
                Some(&skill),
                cancellation.clone(),
            )
            .await?
        {
            return Err(skill_denied(SkillPolicyStage::AfterLoad, name));
        }
        check_cancelled(&cancellation)?;
        Ok(InvokedSkill {
            provider: candidate
                .provider
                .strip_prefix("filesystem:")
                .unwrap_or(&candidate.provider)
                .to_owned(),
            skill,
        })
    }

    /// Loads a listed resource through the winning provider after validating its
    /// relative path and configured result bounds.
    pub async fn read_resource(
        &self,
        cwd: impl AsRef<Path>,
        name: &str,
        path: &str,
        cancellation: CancellationToken,
    ) -> Result<SkillResourceContent, TessivumError> {
        validate_resource_path(path)?;
        let (candidates, _) = self
            .catalog_with_candidates(cwd.as_ref(), cancellation.clone())
            .await?;
        let candidate = candidates
            .get(name)
            .cloned()
            .ok_or_else(|| skill_not_found(name))?;
        check_cancelled(&cancellation)?;
        let result = candidate
            .source
            .read_resource(
                &candidate.listing.locator,
                path,
                self.inner.limits.max_resource_bytes,
                cancellation.clone(),
            )
            .await?;
        check_cancelled(&cancellation)?;
        if result.path != path || result.text.len() > self.inner.limits.max_resource_bytes {
            return Err(skill_error(
                "INVALID_SKILL_RESOURCE",
                "provider returned an invalid or oversized skill resource",
                json!({"name": name, "path": path}),
            ));
        }
        Ok(result)
    }

    async fn catalog_with_candidates(
        &self,
        cwd: &Path,
        cancellation: CancellationToken,
    ) -> Result<(BTreeMap<String, Candidate>, SkillCatalog), TessivumError> {
        check_cancelled(&cancellation)?;
        let cwd = canonical_or_absolute(cwd)?;
        let snapshots = {
            let state = lock(&self.inner.state);
            state
                .providers
                .iter()
                .filter(|(_, slot)| scope_contains(slot.scope.as_deref(), &cwd))
                .map(|(provider, slot)| ProviderSnapshot {
                    id: slot.id,
                    provider: provider.clone(),
                    source: Arc::clone(&slot.source),
                    scope: slot.scope.clone(),
                    rank: slot.rank,
                })
                .collect::<Vec<_>>()
        };

        let mut complete = true;
        let mut candidates = Vec::new();
        for snapshot in snapshots {
            check_cancelled(&cancellation)?;
            let result = snapshot.source.list(cancellation.clone()).await;
            check_cancelled(&cancellation)?;
            let listings = match result
                .and_then(|listings| self.validate_listings(&snapshot.provider, listings))
            {
                Ok(listings) => {
                    self.record_last_good(&snapshot, &listings);
                    listings
                }
                Err(_) => {
                    complete = false;
                    self.last_good(&snapshot)
                }
            };
            for (local_order, listing) in listings.into_iter().enumerate() {
                candidates.push(Candidate {
                    provider: snapshot.provider.clone(),
                    source: Arc::clone(&snapshot.source),
                    scope_depth: snapshot
                        .scope
                        .as_ref()
                        .map_or(0, |scope| scope.components().count()),
                    rank: snapshot.rank,
                    local_order,
                    listing,
                });
            }
        }

        let mut winners = BTreeMap::new();
        for candidate in candidates {
            match winners.get(&candidate.listing.name) {
                Some(current) if !candidate.wins_over(current) => {}
                _ => {
                    winners.insert(candidate.listing.name.clone(), candidate);
                }
            }
        }
        if winners.len() > self.inner.limits.max_catalog_entries {
            return Err(skill_error(
                "SKILL_CATALOG_LIMIT",
                "the selected skill catalog exceeds its configured entry limit",
                json!({"maxEntries": self.inner.limits.max_catalog_entries}),
            ));
        }
        let revision = lock(&self.inner.state).revision;
        let skills = winners
            .values()
            .map(|candidate| SkillCatalogEntry {
                skill: candidate.listing.clone(),
                provider: candidate.provider.clone(),
            })
            .collect();
        Ok((
            winners,
            SkillCatalog {
                revision,
                complete,
                skills,
            },
        ))
    }

    fn validate_listings(
        &self,
        provider: &str,
        listings: Vec<SkillListing>,
    ) -> Result<Vec<SkillListing>, TessivumError> {
        if listings.len() > self.inner.limits.max_catalog_entries {
            return Err(skill_error(
                "SKILL_CATALOG_LIMIT",
                "provider returned too many skill listings",
                json!({"provider": provider, "maxEntries": self.inner.limits.max_catalog_entries}),
            ));
        }
        let mut names = BTreeSet::new();
        let mut locators = BTreeSet::new();
        for listing in &listings {
            validate_skill_name(&listing.name)?;
            if listing.description.len() > self.inner.limits.max_skill_bytes
                || listing.locator.as_str().is_empty()
                || listing.resource_base.as_str().is_empty()
            {
                return Err(skill_error(
                    "INVALID_SKILL_LISTING",
                    "skill listing contains blank opaque identity or oversized description",
                    json!({"provider": provider, "name": listing.name}),
                ));
            }
            if !names.insert(listing.name.clone()) || !locators.insert(listing.locator.clone()) {
                return Err(skill_error(
                    "INVALID_SKILL_LISTING",
                    "provider returned duplicate skill names or locators",
                    json!({"provider": provider}),
                ));
            }
        }
        Ok(listings)
    }

    fn record_last_good(&self, snapshot: &ProviderSnapshot, listings: &[SkillListing]) {
        let change = {
            let mut state = lock(&self.inner.state);
            let Some(slot) = state.providers.get_mut(&snapshot.provider) else {
                return;
            };
            if slot.id != snapshot.id || slot.last_good.as_deref() == Some(listings) {
                return;
            }
            slot.last_good = Some(listings.to_vec());
            let revision = bump_revision(&mut state);
            Some((
                revision,
                state.observers.values().cloned().collect::<Vec<_>>(),
            ))
        };
        if let Some((revision, observers)) = change {
            notify(
                observers,
                SkillChange::Refreshed {
                    provider: snapshot.provider.clone(),
                    revision,
                },
            );
        }
    }

    fn last_good(&self, snapshot: &ProviderSnapshot) -> Vec<SkillListing> {
        lock(&self.inner.state)
            .providers
            .get(&snapshot.provider)
            .filter(|slot| slot.id == snapshot.id)
            .and_then(|slot| slot.last_good.clone())
            .unwrap_or_default()
    }

    async fn load_candidate(
        &self,
        candidate: &Candidate,
        cancellation: CancellationToken,
    ) -> Result<LoadedSkill, TessivumError> {
        check_cancelled(&cancellation)?;
        let loaded = candidate
            .source
            .get(&candidate.listing.locator, cancellation.clone())
            .await?;
        check_cancelled(&cancellation)?;
        if loaded.name != candidate.listing.name
            || loaded.description != candidate.listing.description
            || loaded.locator != candidate.listing.locator
            || loaded.resource_base != candidate.listing.resource_base
            || loaded.body.len() > self.inner.limits.max_skill_bytes
            || loaded.resources.len() > self.inner.limits.max_resources
        {
            return Err(skill_error(
                "INVALID_SKILL_CONTENT",
                "provider returned content that does not match its selected listing or configured bounds",
                json!({"name": candidate.listing.name, "provider": candidate.provider}),
            ));
        }
        let mut resources = BTreeSet::new();
        for resource in &loaded.resources {
            validate_resource_path(&resource.path)?;
            if !resources.insert(resource.path.clone()) {
                return Err(skill_error(
                    "INVALID_SKILL_CONTENT",
                    "provider returned duplicate skill resources",
                    json!({"name": loaded.name, "path": resource.path}),
                ));
            }
        }
        Ok(loaded)
    }
}

#[derive(Clone)]
struct Candidate {
    provider: String,
    source: Arc<dyn SkillProvider>,
    scope_depth: usize,
    rank: i64,
    local_order: usize,
    listing: SkillListing,
}

impl Candidate {
    /// More-specific scope, then higher rank, provider name, then provider-local
    /// list order form a total order independent of registration timing.
    fn wins_over(&self, other: &Self) -> bool {
        self.scope_depth > other.scope_depth
            || (self.scope_depth == other.scope_depth
                && (self.rank > other.rank
                    || (self.rank == other.rank
                        && (self.provider < other.provider
                            || (self.provider == other.provider
                                && self.local_order < other.local_order)))))
    }
}

/// Lifetime owner for one registered provider.
pub struct SkillProviderRegistration {
    inner: Weak<SkillInner>,
    provider: String,
    id: u64,
    closed: AtomicBool,
}

impl fmt::Debug for SkillProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillProviderRegistration")
            .field("provider", &self.provider)
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish()
    }
}

impl SkillProviderRegistration {
    /// Removes this exact registration and invalidates its contribution.
    pub fn close(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let (removed, revision, observers) = {
            let mut state = lock(&inner.state);
            let matches = state
                .providers
                .get(&self.provider)
                .is_some_and(|slot| slot.id == self.id);
            if !matches {
                return false;
            }
            state.providers.remove(&self.provider);
            let revision = bump_revision(&mut state);
            (
                true,
                revision,
                state.observers.values().cloned().collect::<Vec<_>>(),
            )
        };
        if removed {
            notify(
                observers,
                SkillChange::Removed {
                    provider: self.provider.clone(),
                    revision,
                },
            );
        }
        removed
    }

    pub fn is_active(&self) -> bool {
        self.inner.upgrade().is_some_and(|inner| {
            lock(&inner.state)
                .providers
                .get(&self.provider)
                .is_some_and(|slot| slot.id == self.id)
        })
    }
}

impl Drop for SkillProviderRegistration {
    fn drop(&mut self) {
        self.close();
    }
}

/// Lifetime owner for a registry-change observer.
pub struct SkillChangeSubscription {
    inner: Weak<SkillInner>,
    id: u64,
    closed: AtomicBool,
}

impl SkillChangeSubscription {
    pub fn close(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.inner
            .upgrade()
            .is_some_and(|inner| lock(&inner.state).observers.remove(&self.id).is_some())
    }
}

impl Drop for SkillChangeSubscription {
    fn drop(&mut self) {
        self.close();
    }
}

/// Renders the upstream model-facing catalog in deterministic skill-name order.
pub fn model_catalog(catalog: &SkillCatalog) -> String {
    let mut entries = catalog.skills.clone();
    entries.sort_by(|left, right| left.skill.name.cmp(&right.skill.name));
    let mut lines = vec![
        "<system-reminder>".to_owned(),
        "A skill is a reusable set of task-specific instructions. The following skills are available in this session:".to_owned(),
        String::new(),
        "<available_skills>".to_owned(),
    ];
    lines.extend(
        entries
            .into_iter()
            .filter(|entry| entry.skill.invocation.model_invocable)
            .map(|entry| {
                format!(
                    "- `{}`: {}",
                    entry.skill.name,
                    escape_text(&catalog_description(&entry.skill.description))
                )
            }),
    );
    lines.extend([
        "</available_skills>".to_owned(),
        String::new(),
        "If the user names a skill, or the task clearly matches a skill's description, call the `skill` tool with the exact skill name before taking task actions. Load all applicable skills, then follow their full instructions. This catalog contains summaries only; do not infer or follow a skill's instructions until it has been loaded.".to_owned(),
        "A user may also invoke a skill directly; its <skill_content> block then appears in this conversation. Follow it, and do not call the `skill` tool again for that skill.".to_owned(),
        "</system-reminder>".to_owned(),
    ]);
    lines.join("\n")
}
fn catalog_description(value: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MODEL_CATALOG_DESCRIPTION_MAX {
        return normalized;
    }
    let end = normalized
        .char_indices()
        .nth(MODEL_CATALOG_DESCRIPTION_MAX - 3)
        .map(|(index, _)| index)
        .unwrap_or(normalized.len());
    format!("{}...", &normalized[..end])
}

/// Renders the canonical model instruction block for a loaded skill.
pub fn skill_result_tag(skill: &LoadedSkill) -> String {
    [
        format!("<skill_content name=\"{}\">", escape_attr(&skill.name)),
        "<skill_resources>".to_owned(),
        format!(
            "Resources for this skill: {}",
            escape_text(skill.resource_base.as_str())
        ),
        "Load referenced resources only as needed.".to_owned(),
        "</skill_resources>".to_owned(),
        String::new(),
        "<skill_instructions>".to_owned(),
        skill.body.clone(),
        "</skill_instructions>".to_owned(),
        "</skill_content>".to_owned(),
    ]
    .join("\n")
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('\"', "&quot;")
        .replace('<', "&lt;")
}

fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escapes all five XML-sensitive characters without attempting to parse input.
pub fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Registers the one upstream-compatible model-facing skill loader.
pub struct SkillTools {
    registrations: Vec<ToolRegistration>,
}

impl SkillTools {
    pub fn register_for_scopes(
        tools: &ToolRuntime,
        skills: SkillRuntime,
        scopes: SkillSessionScopes,
        policy: Arc<dyn SkillInvocationPolicy>,
    ) -> Result<Self, TessivumError> {
        let registration = tools.register(ToolDefinition::new(
            "skill",
            "Load the full instructions for an available skill. Call this with the exact skill name from the session skill catalog before acting on a task that names or clearly matches that skill.",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"],
                "additionalProperties":false
            }),
            SkillTool {
                skills,
                scopes,
                policy,
            },
        ))?;
        Ok(Self {
            registrations: vec![registration],
        })
    }

    pub fn schemas(&self) -> usize {
        self.registrations.len()
    }
}

struct SkillTool {
    skills: SkillRuntime,
    scopes: SkillSessionScopes,
    policy: Arc<dyn SkillInvocationPolicy>,
}

#[async_trait]
impl ToolHandler for SkillTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                skill_error(
                    "INVALID_SKILL_ARGUMENTS",
                    "skill name must be a string",
                    Value::Null,
                )
            })?;
        validate_skill_name(name)?;
        let cwd = lock(&self.scopes)
            .get(&context.session)
            .cloned()
            .ok_or_else(|| skill_not_available(&context.session))?;
        let invoked = self
            .skills
            .invoke_with_provider(
                &cwd,
                name,
                self.policy.as_ref(),
                context.cancellation.clone(),
            )
            .await?;
        let skill = invoked.skill;
        let text = skill_result_tag(&skill);
        let value = json!({
            "name": skill.name,
            "provider": invoked.provider,
            "resourceBase": {"kind": "opaque", "description": skill.resource_base.as_str()},
            "content": skill.body,
        });
        Ok(ToolOutput::new(
            vec![ContentBlock::Text { text }],
            false,
            value,
        ))
    }
}

/// Root-confined provider for conventional `SKILL.md` directories.
#[derive(Clone)]
pub struct FilesystemSkillProvider {
    roots: Arc<Vec<PathBuf>>,
    entries: Arc<Mutex<BTreeMap<SkillLocator, FilesystemSkillEntry>>>,
    max_skill_bytes: usize,
    max_resources: usize,
}

#[derive(Clone)]
struct FilesystemSkillEntry {
    root: PathBuf,
    directory: PathBuf,
    listing: SkillListing,
}

impl fmt::Debug for FilesystemSkillProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FilesystemSkillProvider")
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}

impl FilesystemSkillProvider {
    pub fn new(roots: Vec<PathBuf>) -> Result<Self, TessivumError> {
        if roots.is_empty() {
            return Err(skill_error(
                "INVALID_SKILL_ROOT",
                "at least one skill root is required",
                Value::Null,
            ));
        }
        let mut canonical = BTreeSet::new();
        for root in roots {
            let root = fs::canonicalize(&root).map_err(|error| {
                skill_error(
                    "INVALID_SKILL_ROOT",
                    "skill root must be an existing directory",
                    json!({"root": root.display().to_string(), "error": error.to_string()}),
                )
            })?;
            if !root.is_dir() {
                return Err(skill_error(
                    "INVALID_SKILL_ROOT",
                    "skill root must be a directory",
                    json!({"root": root.display().to_string()}),
                ));
            }
            canonical.insert(root);
        }
        Ok(Self {
            roots: Arc::new(canonical.into_iter().collect()),
            entries: Arc::new(Mutex::new(BTreeMap::new())),
            max_skill_bytes: DEFAULT_MAX_SKILL_BYTES,
            max_resources: DEFAULT_MAX_RESOURCES,
        })
    }

    pub fn from_root(root: impl Into<PathBuf>) -> Result<Self, TessivumError> {
        Self::new(vec![root.into()])
    }

    pub fn with_limits(
        mut self,
        max_skill_bytes: usize,
        max_resources: usize,
    ) -> Result<Self, TessivumError> {
        if max_skill_bytes == 0 || max_resources == 0 {
            return Err(skill_error(
                "INVALID_SKILL_LIMITS",
                "filesystem skill limits must be positive",
                Value::Null,
            ));
        }
        self.max_skill_bytes = max_skill_bytes;
        self.max_resources = max_resources;
        Ok(self)
    }

    fn scan(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<FilesystemSkillEntry>, TessivumError> {
        check_cancelled(cancellation)?;
        let mut markdowns = Vec::new();
        for root in self.roots.iter() {
            collect_skill_markdown(root, root, &mut markdowns, cancellation)?;
        }
        markdowns.sort();
        let mut entries = Vec::with_capacity(markdowns.len());
        for path in markdowns {
            check_cancelled(cancellation)?;
            let directory = path.parent().expect("SKILL.md has a parent").to_path_buf();
            let root = self
                .roots
                .iter()
                .find(|root| directory.starts_with(root))
                .cloned()
                .expect("scanner returns only configured roots");
            let text = read_text_bounded(&path, self.max_skill_bytes)?;
            let parsed = parse_skill_markdown(&text, &path)?;
            let token = opaque_token(&path);
            let listing = SkillListing::new(
                parsed.name,
                parsed.description,
                format!("skill://filesystem/{token}"),
                format!("resource://filesystem/{token}/"),
            )
            .with_frontmatter(parsed.when_to_use, parsed.invocation);
            entries.push(FilesystemSkillEntry {
                root,
                directory,
                listing,
            });
        }
        Ok(entries)
    }

    fn entry(&self, locator: &SkillLocator) -> Result<FilesystemSkillEntry, TessivumError> {
        lock(&self.entries).get(locator).cloned().ok_or_else(|| {
            skill_error(
                "SKILL_NOT_FOUND",
                "skill locator is not available",
                Value::Null,
            )
        })
    }
}

#[async_trait]
impl SkillProvider for FilesystemSkillProvider {
    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SkillListing>, TessivumError> {
        let entries = self.scan(&cancellation)?;
        let listings = entries
            .iter()
            .map(|entry| entry.listing.clone())
            .collect::<Vec<_>>();
        let mut map = lock(&self.entries);
        map.clear();
        for entry in entries {
            map.insert(entry.listing.locator.clone(), entry);
        }
        Ok(listings)
    }

    async fn get(
        &self,
        locator: &SkillLocator,
        cancellation: CancellationToken,
    ) -> Result<LoadedSkill, TessivumError> {
        check_cancelled(&cancellation)?;
        let entry = self.entry(locator)?;
        let skill_file = confined_file(&entry.root, &entry.directory.join("SKILL.md"))?;
        let text = read_text_bounded(&skill_file, self.max_skill_bytes)?;
        let parsed = parse_skill_markdown(&text, &skill_file)?;
        if parsed.name != entry.listing.name || parsed.description != entry.listing.description {
            return Err(skill_error(
                "SKILL_CHANGED",
                "skill metadata changed after discovery; refresh the catalog before loading",
                json!({"locator": locator.as_str()}),
            ));
        }
        let mut resources = Vec::new();
        collect_resources(
            &entry.root,
            &entry.directory,
            &entry.directory,
            &mut resources,
            self.max_resources,
            &cancellation,
        )?;
        check_cancelled(&cancellation)?;
        Ok(LoadedSkill {
            name: parsed.name,
            description: parsed.description,
            locator: entry.listing.locator,
            resource_base: entry.listing.resource_base,
            body: parsed.body,
            resources,
        })
    }

    async fn read_resource(
        &self,
        locator: &SkillLocator,
        path: &str,
        max_bytes: usize,
        cancellation: CancellationToken,
    ) -> Result<SkillResourceContent, TessivumError> {
        check_cancelled(&cancellation)?;
        validate_resource_path(path)?;
        let entry = self.entry(locator)?;
        let target = confined_file(&entry.root, &entry.directory.join(path))?;
        let text = read_text_bounded(&target, max_bytes.min(self.max_skill_bytes))?;
        check_cancelled(&cancellation)?;
        Ok(SkillResourceContent {
            path: path.into(),
            text,
        })
    }
}

#[derive(Deserialize)]
struct SkillFrontmatter {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "when-to-use")]
    when_to_use: Option<String>,
    #[serde(rename = "disable-model-invocation")]
    disable_model_invocation: Option<serde_yaml::Value>,
    #[serde(rename = "user-invocable")]
    user_invocable: Option<serde_yaml::Value>,
}

struct ParsedSkillMarkdown {
    name: String,
    description: String,
    when_to_use: Option<String>,
    invocation: SkillInvocation,
    body: String,
}

fn parse_skill_markdown(text: &str, path: &Path) -> Result<ParsedSkillMarkdown, TessivumError> {
    let text = text.replace("\r\n", "\n");
    let Some(after_open) = text.strip_prefix("---\n") else {
        return Err(skill_error(
            "INVALID_SKILL_FRONTMATTER",
            "SKILL.md must begin with YAML frontmatter",
            json!({"path": path.display().to_string()}),
        ));
    };
    let Some(end) = after_open.find("\n---\n") else {
        return Err(skill_error(
            "INVALID_SKILL_FRONTMATTER",
            "SKILL.md frontmatter must end with a delimiter line",
            json!({"path": path.display().to_string()}),
        ));
    };
    let source = &after_open[..end];
    let value: serde_yaml::Value = serde_yaml::from_str(source).map_err(|error| {
        skill_error(
            "INVALID_SKILL_FRONTMATTER",
            "SKILL.md frontmatter is not valid YAML",
            json!({"path": path.display().to_string(), "error": error.to_string()}),
        )
    })?;
    for legacy in ["disableModelInvocation", "modelInvocable", "userInvocable"] {
        if value
            .as_mapping()
            .is_some_and(|fields| fields.contains_key(serde_yaml::Value::String(legacy.into())))
        {
            return Err(skill_error(
                "INVALID_SKILL_FRONTMATTER",
                format!("frontmatter field {legacy:?} is unsupported"),
                json!({"path": path.display().to_string()}),
            ));
        }
    }
    let frontmatter: SkillFrontmatter = serde_yaml::from_value(value).map_err(|error| {
        skill_error(
            "INVALID_SKILL_FRONTMATTER",
            "SKILL.md frontmatter is not valid YAML",
            json!({"path": path.display().to_string(), "error": error.to_string()}),
        )
    })?;
    validate_skill_name(&frontmatter.name)?;
    Ok(ParsedSkillMarkdown {
        name: frontmatter.name,
        description: frontmatter.description,
        when_to_use: frontmatter.when_to_use.filter(|value| !value.is_empty()),
        invocation: SkillInvocation {
            model_invocable: !(frontmatter_bool(
                frontmatter.disable_model_invocation,
                "disable-model-invocation",
                path,
            )?
            .unwrap_or(false)),
            user_invocable: frontmatter_bool(frontmatter.user_invocable, "user-invocable", path)?
                .unwrap_or(true),
        },
        body: after_open[end + "\n---\n".len()..].to_owned(),
    })
}

fn frontmatter_bool(
    value: Option<serde_yaml::Value>,
    field: &str,
    path: &Path,
) -> Result<Option<bool>, TessivumError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = match value {
        serde_yaml::Value::Bool(value) => Some(value),
        serde_yaml::Value::Number(value) => match value.as_i64() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        },
        serde_yaml::Value::String(value) => match value.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Some(true),
            "false" | "no" | "off" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    };
    parsed.map(Some).ok_or_else(|| {
        skill_error(
            "INVALID_SKILL_FRONTMATTER",
            format!("frontmatter field {field:?} must be a boolean"),
            json!({"path": path.display().to_string()}),
        )
    })
}

fn collect_skill_markdown(
    root: &Path,
    directory: &Path,
    output: &mut Vec<PathBuf>,
    cancellation: &CancellationToken,
) -> Result<(), TessivumError> {
    check_cancelled(cancellation)?;
    let mut children = fs::read_dir(directory)
        .map_err(|error| skill_io("list skill directory", directory, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| skill_io("list skill directory", directory, error))?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        check_cancelled(cancellation)?;
        let file_type = child
            .file_type()
            .map_err(|error| skill_io("inspect skill directory entry", &child.path(), error))?;
        let path = child.path();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_skill_markdown(root, &path, output, cancellation)?;
        } else if file_type.is_file() && child.file_name() == "SKILL.md" {
            let path = confined_file(root, &path)?;
            output.push(path);
        }
    }
    Ok(())
}

fn collect_resources(
    root: &Path,
    base: &Path,
    directory: &Path,
    output: &mut Vec<SkillResource>,
    max_resources: usize,
    cancellation: &CancellationToken,
) -> Result<(), TessivumError> {
    let mut children = fs::read_dir(directory)
        .map_err(|error| skill_io("list skill resources", directory, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| skill_io("list skill resources", directory, error))?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        check_cancelled(cancellation)?;
        let kind = child
            .file_type()
            .map_err(|error| skill_io("inspect skill resource", &child.path(), error))?;
        let path = child.path();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect_resources(root, base, &path, output, max_resources, cancellation)?;
        } else if kind.is_file() && child.file_name() != "SKILL.md" {
            let path = confined_file(root, &path)?;
            let relative = path.strip_prefix(base).map_err(|_| {
                skill_error(
                    "SKILL_ROOT_ESCAPE",
                    "skill resource escaped its base directory",
                    Value::Null,
                )
            })?;
            let relative = relative
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            validate_resource_path(&relative)?;
            output.push(SkillResource { path: relative });
            if output.len() > max_resources {
                return Err(skill_error(
                    "SKILL_RESOURCE_LIMIT",
                    "skill has too many resources",
                    json!({"maxResources": max_resources}),
                ));
            }
        }
    }
    Ok(())
}

fn read_text_bounded(path: &Path, max_bytes: usize) -> Result<String, TessivumError> {
    let metadata =
        fs::metadata(path).map_err(|error| skill_io("inspect skill file", path, error))?;
    if !metadata.is_file() || metadata.len() > max_bytes as u64 {
        return Err(skill_error(
            "SKILL_CONTENT_LIMIT",
            "skill file is not a regular file or exceeds its configured size limit",
            json!({"path": path.display().to_string(), "maxBytes": max_bytes}),
        ));
    }
    let bytes = fs::read(path).map_err(|error| skill_io("read skill file", path, error))?;
    String::from_utf8(bytes).map_err(|_| {
        skill_error(
            "SKILL_NOT_TEXT",
            "skill files and resources must be UTF-8 text",
            json!({"path": path.display().to_string()}),
        )
    })
}

fn confined_file(root: &Path, path: &Path) -> Result<PathBuf, TessivumError> {
    let path =
        fs::canonicalize(path).map_err(|error| skill_io("resolve skill path", path, error))?;
    if !path.starts_with(root) {
        return Err(skill_error(
            "SKILL_ROOT_ESCAPE",
            "skill path resolves outside a configured root",
            Value::Null,
        ));
    }
    let metadata = fs::metadata(&path)
        .map_err(|error| skill_io("inspect resolved skill path", &path, error))?;
    if !metadata.is_file() {
        return Err(skill_error(
            "SKILL_NOT_REGULAR_FILE",
            "skill path must resolve to a regular file",
            Value::Null,
        ));
    }
    Ok(path)
}

fn opaque_token(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_skill_name(name: &str) -> Result<(), TessivumError> {
    let valid = !name.is_empty()
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--");
    if valid {
        Ok(())
    } else {
        Err(skill_error(
            "INVALID_SKILL_NAME",
            "skill names must be lowercase kebab-case identifiers",
            json!({"name": name}),
        ))
    }
}

fn validate_provider_name(name: &str) -> Result<(), TessivumError> {
    if name.trim().is_empty() {
        Err(skill_error(
            "INVALID_SKILL_PROVIDER",
            "skill provider name must not be blank",
            Value::Null,
        ))
    } else {
        Ok(())
    }
}

fn validate_resource_path(path: &str) -> Result<(), TessivumError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(skill_error(
            "INVALID_SKILL_RESOURCE_PATH",
            "skill resource paths must be non-empty and relative",
            json!({"path": path.display().to_string()}),
        ));
    }
    Ok(())
}

fn canonical_scope(scope: PathBuf) -> Result<PathBuf, TessivumError> {
    fs::canonicalize(&scope).map_err(|error| {
        skill_error(
            "INVALID_SKILL_SCOPE",
            "skill provider scope must exist",
            json!({"scope": scope.display().to_string(), "error": error.to_string()}),
        )
    })
}

fn canonical_or_absolute(path: &Path) -> Result<PathBuf, TessivumError> {
    if let Ok(path) = fs::canonicalize(path) {
        return Ok(path);
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                skill_error(
                    "SKILL_CWD_UNAVAILABLE",
                    "current directory is unavailable",
                    json!({"error": error.to_string()}),
                )
            })?
            .join(path)
    };
    Ok(normalize_path(path))
}

fn normalize_path(path: PathBuf) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn scope_contains(scope: Option<&Path>, cwd: &Path) -> bool {
    scope.is_none_or(|scope| cwd.starts_with(scope))
}

fn bump_revision(state: &mut SkillState) -> u64 {
    state.revision = state.revision.wrapping_add(1);
    state.revision
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), TessivumError> {
    if cancellation.is_cancelled() {
        Err(skill_error(
            "SKILL_CANCELLED",
            "skill operation was cancelled",
            Value::Null,
        ))
    } else {
        Ok(())
    }
}

fn skill_not_available(session: &SessionId) -> TessivumError {
    skill_error(
        "SKILL_NOT_AVAILABLE",
        "the skill tool is not enabled for this session",
        json!({"sessionId": session}),
    )
}

fn skill_not_found(name: &str) -> TessivumError {
    skill_error(
        "SKILL_NOT_FOUND",
        "the requested skill is not available in this scope",
        json!({"name": name}),
    )
}

fn skill_denied(stage: SkillPolicyStage, name: &str) -> TessivumError {
    skill_error(
        "SKILL_DENIED",
        "skill invocation was denied by policy",
        json!({"name": name, "stage": match stage { SkillPolicyStage::BeforeLoad => "before-load", SkillPolicyStage::AfterLoad => "after-load"}}),
    )
}

fn skill_not_model_invocable(name: &str) -> TessivumError {
    skill_error(
        "SKILL_NOT_MODEL_INVOCABLE",
        "the requested skill is not available for model invocation",
        json!({"name": name}),
    )
}

fn skill_io(action: &str, path: &Path, error: std::io::Error) -> TessivumError {
    skill_error(
        "SKILL_IO_FAILED",
        "skill provider could not access a confined path",
        json!({"action": action, "path": path.display().to_string(), "error": error.to_string()}),
    )
}

fn skill_error(
    code: impl Into<String>,
    message: impl Into<String>,
    details: Value,
) -> TessivumError {
    TessivumError::new(code, message, "skills", details)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn notify(observers: Vec<Arc<dyn SkillChangeObserver>>, change: SkillChange) {
    for observer in observers {
        let _ =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer.observe(&change)));
    }
}

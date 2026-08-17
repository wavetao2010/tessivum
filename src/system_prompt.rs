use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
};

use serde_json::json;
use tessivum_core::{ContextHandle, CoreError, ServiceHandle, ServiceKey};

use crate::{TessivumError, ToolSchema};

/// Returns the stable native service key for system-prompt assembly.
pub fn system_prompt_service_key() -> ServiceKey {
    ServiceKey::new("harness.system-prompt", "1")
}

/// One independently ordered contribution to a system prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptSection {
    /// Stable identifier used for deterministic ordering and replacement checks.
    pub id: String,
    /// Lower values appear first; ties are ordered by [`Self::id`].
    pub order: i64,
    /// Verbatim section text. Empty text is omitted from assembled prompts.
    pub text: String,
}

impl PromptSection {
    pub fn new(id: impl Into<String>, order: i64, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            order,
            text: text.into(),
        }
    }
}

/// The assembled model input retained alongside the caller-provided tool schemas.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptAssembly {
    pub text: String,
    pub tools: Vec<ToolSchema>,
}

/// A thread-safe registry of owned system-prompt sections.
#[derive(Clone, Default)]
pub struct SystemPrompt {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SystemPrompt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SystemPrompt")
            .finish_non_exhaustive()
    }
}

struct Inner {
    state: Mutex<State>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
        }
    }
}

#[derive(Default)]
struct State {
    next_token: u64,
    sections: BTreeMap<String, RegisteredSection>,
    observers: BTreeMap<u64, Observer>,
}

#[derive(Clone)]
struct RegisteredSection {
    token: u64,
    section: PromptSection,
}

type Observer = Arc<dyn Fn() + Send + Sync + 'static>;

impl SystemPrompt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes this registry into the caller's lifetime-owned context scope.
    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(system_prompt_service_key(), self)
    }

    /// Registers one owned section. Dropping or removing the returned handle
    /// removes only this registration and not a later registration with the same ID.
    pub fn register(&self, section: PromptSection) -> Result<PromptRegistration, TessivumError> {
        validate_section_id(&section.id, "registered")?;
        let id = section.id.clone();
        let (token, observers) = {
            let mut state = lock(&self.inner.state);
            if state.sections.contains_key(&id) {
                return Err(duplicate_section_id("registered", &id));
            }
            let token = next_token(&mut state);
            state
                .sections
                .insert(id.clone(), RegisteredSection { token, section });
            let observers = state.observers.values().cloned().collect();
            (token, observers)
        };
        notify(observers);

        Ok(PromptRegistration {
            inner: Arc::downgrade(&self.inner),
            id,
            token,
            removed: AtomicBool::new(false),
        })
    }

    /// Subscribes to future registration or removal changes. Panics from one
    /// observer are contained and never prevent later observers from running.
    pub fn subscribe<F>(&self, observer: F) -> PromptSubscription
    where
        F: Fn() + Send + Sync + 'static,
    {
        let token = {
            let mut state = lock(&self.inner.state);
            let token = next_token(&mut state);
            state.observers.insert(token, Arc::new(observer));
            token
        };
        PromptSubscription {
            inner: Arc::downgrade(&self.inner),
            token,
            removed: AtomicBool::new(false),
        }
    }

    /// Merges a snapshot of registered sections with caller-owned runtime sections.
    pub fn assemble<I>(
        &self,
        runtime_sections: I,
        tools: Vec<ToolSchema>,
    ) -> Result<PromptAssembly, TessivumError>
    where
        I: IntoIterator,
        I::Item: Borrow<PromptSection>,
    {
        let mut runtime_ids = BTreeSet::new();
        let mut sections = Vec::new();
        for (position, section) in runtime_sections.into_iter().enumerate() {
            let section = section.borrow().clone();
            validate_section_id(&section.id, "runtime")?;
            if !runtime_ids.insert(section.id.clone()) {
                return Err(duplicate_section_id("runtime", &section.id));
            }
            sections.push(OrderedSection {
                section,
                source: Source::Runtime,
                position,
            });
        }

        let registered = {
            let state = lock(&self.inner.state);
            state
                .sections
                .values()
                .cloned()
                .map(|registered| registered.section)
                .collect::<Vec<_>>()
        };
        sections.reserve(registered.len());
        sections.extend(
            registered
                .into_iter()
                .enumerate()
                .map(|(position, section)| OrderedSection {
                    section,
                    source: Source::Registered,
                    position,
                }),
        );
        sections.sort_by(|left, right| {
            left.section
                .order
                .cmp(&right.section.order)
                .then_with(|| left.section.id.cmp(&right.section.id))
                .then_with(|| left.source.cmp(&right.source))
                .then_with(|| left.position.cmp(&right.position))
        });

        Ok(PromptAssembly {
            text: sections
                .into_iter()
                .filter_map(|section| {
                    (!section.section.text.is_empty()).then_some(section.section.text)
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
            tools,
        })
    }
}

/// An owned registration whose removal is idempotent.
pub struct PromptRegistration {
    inner: Weak<Inner>,
    id: String,
    token: u64,
    removed: AtomicBool,
}

impl std::fmt::Debug for PromptRegistration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PromptRegistration")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl PromptRegistration {
    /// Removes this section from future assemblies, returning whether it changed the registry.
    pub fn remove(&self) -> bool {
        if self.removed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.inner
            .upgrade()
            .is_some_and(|inner| inner.remove_registration(&self.id, self.token))
    }
}

impl Drop for PromptRegistration {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// An owned change subscription whose removal is idempotent.
pub struct PromptSubscription {
    inner: Weak<Inner>,
    token: u64,
    removed: AtomicBool,
}

impl std::fmt::Debug for PromptSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PromptSubscription")
            .finish_non_exhaustive()
    }
}

impl PromptSubscription {
    /// Stops future notifications, returning whether this call removed the observer.
    pub fn remove(&self) -> bool {
        if self.removed.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.inner
            .upgrade()
            .is_some_and(|inner| inner.remove_subscription(self.token))
    }
}

impl Drop for PromptSubscription {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum Source {
    Registered,
    Runtime,
}

struct OrderedSection {
    section: PromptSection,
    source: Source,
    position: usize,
}

impl Inner {
    fn remove_registration(&self, id: &str, token: u64) -> bool {
        let observers = {
            let mut state = lock(&self.state);
            if state.sections.get(id).map(|section| section.token) != Some(token) {
                return false;
            }
            state.sections.remove(id);
            state.observers.values().cloned().collect()
        };
        notify(observers);
        true
    }

    fn remove_subscription(&self, token: u64) -> bool {
        lock(&self.state).observers.remove(&token).is_some()
    }
}

fn next_token(state: &mut State) -> u64 {
    let token = state.next_token;
    state.next_token = state
        .next_token
        .checked_add(1)
        .expect("system prompt registration token exhausted");
    token
}

fn validate_section_id(id: &str, source: &str) -> Result<(), TessivumError> {
    if id.trim().is_empty() {
        return Err(TessivumError::new(
            "INVALID_PROMPT_SECTION_ID",
            "prompt section ID must not be blank",
            "system-prompt",
            json!({"source": source}),
        ));
    }
    Ok(())
}

fn duplicate_section_id(source: &str, id: &str) -> TessivumError {
    TessivumError::new(
        "DUPLICATE_PROMPT_SECTION_ID",
        "prompt section ID is already registered",
        "system-prompt",
        json!({"source": source, "id": id}),
    )
}

fn notify(observers: Vec<Observer>) {
    for observer in observers {
        let _ = catch_unwind(AssertUnwindSafe(|| observer()));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

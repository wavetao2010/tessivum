//! Session-owned, bounded local background jobs.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{
    BoxDisposer, CancellationToken, ContextHandle, CoreError, FiberState, ServiceHandle, ServiceKey,
};
use thiserror::Error;
use tokio::{sync::Notify, time};

use crate::{
    agent::AgentAuthority,
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    ContentBlock, SessionEvent, SessionId, TessivumError,
};

const MAX_OUTPUT_LIMIT: usize = 1_048_576;
const MAX_READ_BYTES: usize = 65_536;
const MAX_LIST_JOBS: usize = 128;
const MAX_WAIT: Duration = Duration::from_secs(60);
const MAX_TEXT_BYTES: usize = 16_384;

tokio::task_local! {
    static RUNNING_JOB: Arc<Job>;
}

/// Stable key for the process-local jobs capability.
pub fn jobs_service_key() -> ServiceKey {
    ServiceKey::new("harness.jobs", "1")
}

/// A generated job identity. IDs always have the form `<kind>-N`.
#[repr(transparent)]
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct JobId(String);

impl JobId {
    fn generated(kind: &str, number: u64) -> Self {
        Self(format!("{kind}-{number}"))
    }

    /// Parses an externally supplied generated identifier.
    pub fn parse(value: impl Into<String>) -> Result<Self, JobError> {
        let value = value.into();
        let Some((kind, number)) = value.rsplit_once('-') else {
            return Err(JobError::InvalidJobId);
        };
        if !valid_kind(kind)
            || number.is_empty()
            || (number.len() > 1 && number.starts_with('0'))
            || number
                .parse::<u64>()
                .ok()
                .filter(|number| *number > 0)
                .is_none()
        {
            return Err(JobError::InvalidJobId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for JobId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The externally visible lifecycle of one job.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Running,
    Stopping,
    Completed,
    Killed,
    Failed,
}

impl JobStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Killed | Self::Failed)
    }
}

/// The terminal result returned by a job's run hook.
pub type JobRunResult = Result<Value, String>;
/// A sendable, owned run future.
pub type JobRunFuture = Pin<Box<dyn Future<Output = JobRunResult> + Send + 'static>>;
/// The one executable role a local job needs.
pub type JobRunHook = Arc<dyn Fn(JobControl) -> JobRunFuture + Send + Sync + 'static>;

/// Validated input for a newly published job.
pub struct JobStart {
    pub kind: String,
    pub label: String,
    pub output_limit: usize,
    pub run: JobRunHook,
}

impl fmt::Debug for JobStart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JobStart")
            .field("kind", &self.kind)
            .field("label", &self.label)
            .field("output_limit", &self.output_limit)
            .finish_non_exhaustive()
    }
}

impl JobStart {
    pub fn new<F, Fut>(
        kind: impl Into<String>,
        label: impl Into<String>,
        output_limit: usize,
        run: F,
    ) -> Self
    where
        F: Fn(JobControl) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = JobRunResult> + Send + 'static,
    {
        Self {
            kind: kind.into(),
            label: label.into(),
            output_limit,
            run: Arc::new(move |control| Box::pin(run(control))),
        }
    }

    fn validate(&self) -> Result<(), JobError> {
        if !valid_kind(&self.kind) {
            return Err(JobError::InvalidKind);
        }
        if self.label.trim().is_empty() || self.label.len() > 256 {
            return Err(JobError::InvalidLabel);
        }
        if self.output_limit == 0 || self.output_limit > MAX_OUTPUT_LIMIT {
            return Err(JobError::InvalidOutputLimit);
        }
        Ok(())
    }
}

/// Immutable public state. Output itself is read through a cursor.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub id: JobId,
    pub owner: SessionId,
    pub kind: String,
    pub label: String,
    pub status: JobStatus,
    pub total_output_bytes: u64,
    pub output_available_from: u64,
    pub reported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One consuming-cursor output page. `lost` tells the caller how much of an
/// old cursor fell outside the bounded tail.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobOutputRead {
    pub cursor: u64,
    pub next_cursor: u64,
    pub total_bytes: u64,
    pub available_from: u64,
    pub lost: u64,
    pub bytes: Vec<u8>,
}

/// A terminal snapshot not yet durably shown to the model.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobCompletionNotice {
    pub owner: SessionId,
    pub job: JobSnapshot,
}

impl JobCompletionNotice {
    /// Builds the caller-persisted model event. Call [`JobOwner::report`] only
    /// after this event has been appended successfully.
    pub fn session_event(&self, seq: u64, time: u64) -> SessionEvent {
        SessionEvent {
            event_type: "job/done".into(),
            seq,
            time,
            data: json!({"job": self.job}),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
        }
    }
}

/// Job-capability errors. Foreign owners are deliberately reported as absent.
#[derive(Debug, Error)]
pub enum JobError {
    #[error("job registry is disposed")]
    Disposed,
    #[error("job owner is not attached")]
    OwnerNotAttached,
    #[error("job owner is disposing")]
    OwnerDisposing,
    #[error("job does not exist")]
    NotFound,
    #[error("job id is invalid")]
    InvalidJobId,
    #[error("job kind is invalid")]
    InvalidKind,
    #[error("job label is invalid")]
    InvalidLabel,
    #[error("job output limit is invalid")]
    InvalidOutputLimit,
    #[error("job output cursor is invalid")]
    InvalidCursor,
    #[error("job output read exceeds the limit")]
    ReadTooLarge,
    #[error("job wait timed out")]
    TimedOut,
    #[error("job wait was cancelled")]
    Cancelled,
    #[error("job id sequence is exhausted")]
    IdExhausted,
    #[error("job run requires an attached Tokio controller")]
    ControllerNotAttached,
    #[error(transparent)]
    Core(#[from] CoreError),
}

#[derive(Clone)]
enum Settlement {
    Completed(Value),
    Killed,
    Failed(String),
}

struct JobState {
    status: JobStatus,
    output: VecDeque<u8>,
    total_output_bytes: u64,
    settlement: Option<Settlement>,
    settled: bool,
    reported: bool,
    result: Option<Value>,
    error: Option<String>,
}

struct Job {
    id: JobId,
    owner: SessionId,
    owner_generation: u64,
    kind: String,
    label: String,
    output_limit: usize,
    cancellation: CancellationToken,
    state: Mutex<JobState>,
    done: Notify,
}

impl Job {
    fn snapshot(&self) -> JobSnapshot {
        let state = lock(&self.state);
        JobSnapshot {
            id: self.id.clone(),
            owner: self.owner.clone(),
            kind: self.kind.clone(),
            label: self.label.clone(),
            status: state.status,
            total_output_bytes: state.total_output_bytes,
            output_available_from: state
                .total_output_bytes
                .saturating_sub(state.output.len() as u64),
            reported: state.reported,
            result: state.result.clone(),
            error: state.error.clone(),
        }
    }

    fn write(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        let mut state = lock(&self.state);
        if state.settled {
            return false;
        }
        state.total_output_bytes = state.total_output_bytes.saturating_add(bytes.len() as u64);
        state.output.extend(bytes);
        let excess = state.output.len().saturating_sub(self.output_limit);
        state.output.drain(..excess);
        true
    }

    fn claim(&self, settlement: Settlement) -> bool {
        let mut state = lock(&self.state);
        if state.settled || state.settlement.is_some() {
            return false;
        }
        state.settlement = Some(settlement);
        true
    }

    fn request_stop(&self) -> bool {
        let changed = {
            let mut state = lock(&self.state);
            if state.settled {
                false
            } else {
                if state.settlement.is_none() {
                    state.settlement = Some(Settlement::Killed);
                }
                if state.status == JobStatus::Running {
                    state.status = JobStatus::Stopping;
                    true
                } else {
                    false
                }
            }
        };
        self.cancellation.cancel();
        changed
    }

    fn finalize(&self, fallback: Settlement) -> bool {
        let mut state = lock(&self.state);
        if state.settled {
            return false;
        }
        let settlement = state.settlement.take().unwrap_or(fallback);
        match settlement {
            Settlement::Completed(result) => {
                state.status = JobStatus::Completed;
                state.result = Some(result);
            }
            Settlement::Killed => state.status = JobStatus::Killed,
            Settlement::Failed(error) => {
                state.status = JobStatus::Failed;
                state.error = Some(trim_text(error));
            }
        }
        state.settled = true;
        true
    }

    fn read(&self, cursor: u64, max_bytes: usize) -> Result<JobOutputRead, JobError> {
        if max_bytes > MAX_READ_BYTES {
            return Err(JobError::ReadTooLarge);
        }
        let state = lock(&self.state);
        if cursor > state.total_output_bytes {
            return Err(JobError::InvalidCursor);
        }
        let available_from = state
            .total_output_bytes
            .saturating_sub(state.output.len() as u64);
        let lost = available_from.saturating_sub(cursor);
        let start = cursor.max(available_from);
        let available = state.total_output_bytes.saturating_sub(start) as usize;
        let count = max_bytes.min(available);
        let offset = (start - available_from) as usize;
        let bytes = state
            .output
            .iter()
            .skip(offset)
            .take(count)
            .copied()
            .collect();
        Ok(JobOutputRead {
            cursor,
            next_cursor: start + count as u64,
            total_bytes: state.total_output_bytes,
            available_from,
            lost,
            bytes,
        })
    }

    fn completion_notice(&self) -> Option<JobCompletionNotice> {
        let snapshot = self.snapshot();
        (snapshot.status.is_terminal() && !snapshot.reported).then(|| JobCompletionNotice {
            owner: snapshot.owner.clone(),
            job: snapshot,
        })
    }

    fn mark_reported(&self) -> bool {
        let mut state = lock(&self.state);
        if !state.settled || state.reported {
            return false;
        }
        state.reported = true;
        true
    }
}

/// Cooperative control held only by the job run hook.
#[derive(Clone)]
pub struct JobControl {
    job: Arc<Job>,
    registry: Weak<RegistryInner>,
}

impl fmt::Debug for JobControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JobControl")
            .field("id", &self.job.id)
            .finish_non_exhaustive()
    }
}

impl JobControl {
    pub fn id(&self) -> JobId {
        self.job.id.clone()
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.job.cancellation.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.job.cancellation.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.job.cancellation.cancelled().await;
    }

    /// Appends bytes to the bounded tail while the owned resource is live.
    pub fn write(&self, bytes: impl AsRef<[u8]>) -> bool {
        let changed = self.job.write(bytes.as_ref());
        if changed {
            notify_changed(&self.registry, &self.job);
        }
        changed
    }

    pub fn write_text(&self, text: impl AsRef<str>) -> bool {
        self.write(text.as_ref().as_bytes())
    }

    /// Claims successful settlement. It is made visible only after the run hook
    /// has returned, so `wait` never reports done before its resource is released.
    pub fn complete(&self, result: Value) -> bool {
        self.job.claim(Settlement::Completed(result))
    }

    /// Claims failed settlement. Later settlement attempts lose deterministically.
    pub fn fail(&self, error: impl Into<String>) -> bool {
        self.job.claim(Settlement::Failed(error.into()))
    }
}

struct OwnerState {
    authority: AgentAuthority,
    generation: u64,
    disposing: bool,
    completion: Option<Arc<Notify>>,
}

type JobObserver = Arc<dyn Fn(&JobSnapshot) + Send + Sync>;

struct RegistryState {
    next_id: u64,
    next_owner_generation: u64,
    disposed: bool,
    dispose_finished: bool,
    dispose_completion: Option<Arc<Notify>>,
    owners: BTreeMap<SessionId, OwnerState>,
    jobs: BTreeMap<JobId, Arc<Job>>,
    next_observer: u64,
    done_observers: BTreeMap<u64, JobObserver>,
    change_observers: BTreeMap<u64, JobObserver>,
}

enum Disposal {
    Start(Vec<Arc<Job>>),
    Join(Arc<Notify>),
    Done,
}

struct RegistryInner {
    state: Mutex<RegistryState>,
}

/// A process-local registry. Publication occurs under one lock before the run
/// task is spawned, so a concurrently finishing job is always discoverable.
#[derive(Clone)]
pub struct LocalJobRegistry {
    inner: Arc<RegistryInner>,
}

impl Default for LocalJobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for LocalJobRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalJobRegistry")
            .field("jobs", &lock(&self.inner.state).jobs.len())
            .finish()
    }
}

impl LocalJobRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                state: Mutex::new(RegistryState {
                    next_id: 1,
                    next_owner_generation: 1,
                    disposed: false,
                    dispose_finished: false,
                    dispose_completion: None,
                    owners: BTreeMap::new(),
                    jobs: BTreeMap::new(),
                    next_observer: 1,
                    done_observers: BTreeMap::new(),
                    change_observers: BTreeMap::new(),
                }),
            }),
        }
    }

    /// Publishes the versioned service and makes context disposal cancel and join all jobs.
    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        self.bind_runtime(context)?;
        context.provide(jobs_service_key(), self)
    }

    /// Binds runtime disposal to a controller scope without publishing a service.
    pub fn bind_runtime(&self, controller: &ContextHandle) -> Result<(), CoreError> {
        let registry = self.clone();
        let effect: BoxDisposer = Box::new(move || {
            Box::pin(async move {
                registry.dispose().await;
                Ok(())
            })
        });
        controller.scope().add_effect("jobs.dispose", effect)?;
        Ok(())
    }

    /// Attaches a scope-bound, generation-bearing authority from one live agent authority.
    pub fn attach_owner(
        &self,
        authority: &AgentAuthority,
        controller: &ContextHandle,
    ) -> Result<JobOwner, JobError> {
        if !authority.is_live() {
            return Err(JobError::OwnerNotAttached);
        }
        let owner = authority.id();
        if controller.scope().state() != FiberState::Active {
            return Err(JobError::ControllerNotAttached);
        }
        let generation = {
            let mut state = lock(&self.inner.state);
            if state.disposed {
                return Err(JobError::Disposed);
            }
            if state.owners.contains_key(&owner) {
                return Err(JobError::OwnerNotAttached);
            }
            let generation = state.next_owner_generation;
            state.next_owner_generation = state
                .next_owner_generation
                .checked_add(1)
                .ok_or(JobError::IdExhausted)?;
            state.owners.insert(
                owner.clone(),
                OwnerState {
                    authority: authority.clone(),
                    generation,
                    disposing: false,
                    completion: None,
                },
            );
            generation
        };
        let attached = JobOwner {
            registry: self.clone(),
            authority: authority.clone(),
            owner,
            generation,
        };
        let cleanup = attached.clone();
        let effect: BoxDisposer = Box::new(move || {
            Box::pin(async move {
                cleanup.dispose().await;
                Ok(())
            })
        });
        if let Err(error) = controller.scope().add_effect("jobs.owner", effect) {
            self.remove_owner(&attached);
            return Err(error.into());
        }
        Ok(attached)
    }

    /// Cancels all live jobs and waits for every run hook to release its resource.
    pub async fn dispose(&self) {
        match self.begin_dispose() {
            Disposal::Start(jobs) => self.stop_and_join(jobs).await,
            Disposal::Join(completion) if !self.current_job_is_registered() => {
                self.wait_for_dispose(completion).await;
            }
            Disposal::Join(_) | Disposal::Done => {}
        }
    }

    fn install_tools(
        &self,
        owner: JobOwner,
        runtime: &ToolRuntime,
    ) -> Result<JobTools, TessivumError> {
        let mut registrations = Vec::with_capacity(4);
        for action in [
            JobToolAction::List,
            JobToolAction::Read,
            JobToolAction::Kill,
            JobToolAction::Wait,
        ] {
            registrations.push(runtime.register(ToolDefinition::new(
                action.name(),
                action.description(),
                action.schema(),
                JobTool {
                    owner: owner.clone(),
                    action,
                },
            ))?);
        }
        Ok(JobTools { registrations })
    }

    fn start(&self, owner: &JobOwner, start: JobStart) -> Result<JobSnapshot, JobError> {
        start.validate()?;
        let handle =
            tokio::runtime::Handle::try_current().map_err(|_| JobError::ControllerNotAttached)?;
        let cancellation = ContextHandle::root().scope().cancellation();
        let job = {
            let mut state = lock(&self.inner.state);
            Self::authorize_owner(&state, owner)?;
            let number = state.next_id;
            state.next_id = state.next_id.checked_add(1).ok_or(JobError::IdExhausted)?;
            let job = Arc::new(Job {
                id: JobId::generated(&start.kind, number),
                owner: owner.owner.clone(),
                owner_generation: owner.generation,
                kind: start.kind,
                label: start.label,
                output_limit: start.output_limit,
                cancellation,
                state: Mutex::new(JobState {
                    status: JobStatus::Running,
                    output: VecDeque::new(),
                    total_output_bytes: 0,
                    settlement: None,
                    settled: false,
                    reported: false,
                    result: None,
                    error: None,
                }),
                done: Notify::new(),
            });
            state.jobs.insert(job.id.clone(), Arc::clone(&job));
            job
        };
        notify_changed(&Arc::downgrade(&self.inner), &job);
        handle.spawn(run_job(
            Arc::downgrade(&self.inner),
            Arc::clone(&job),
            start.run,
        ));
        Ok(job.snapshot())
    }

    fn get(&self, owner: &JobOwner, id: &JobId) -> Result<JobSnapshot, JobError> {
        Ok(self.owned(owner, id)?.snapshot())
    }

    fn list(&self, owner: &JobOwner) -> Result<Vec<JobSnapshot>, JobError> {
        let jobs: Vec<_> = {
            let state = lock(&self.inner.state);
            Self::authorize_owner(&state, owner)?;
            state
                .jobs
                .values()
                .filter(|job| job.owner == owner.owner && job.owner_generation == owner.generation)
                .take(MAX_LIST_JOBS)
                .cloned()
                .collect()
        };
        Ok(jobs.into_iter().map(|job| job.snapshot()).collect())
    }

    fn read(
        &self,
        owner: &JobOwner,
        id: &JobId,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<JobOutputRead, JobError> {
        self.owned(owner, id)?.read(cursor, max_bytes)
    }

    fn kill(&self, owner: &JobOwner, id: &JobId) -> Result<JobSnapshot, JobError> {
        let job = self.owned(owner, id)?;
        if job.request_stop() {
            notify_changed(&Arc::downgrade(&self.inner), &job);
        }
        Ok(job.snapshot())
    }

    async fn wait(
        &self,
        owner: &JobOwner,
        id: &JobId,
        timeout: Option<Duration>,
        cancellation: Option<CancellationToken>,
    ) -> Result<JobSnapshot, JobError> {
        let job = self.owned(owner, id)?;
        wait_for(&job, timeout, cancellation).await
    }

    fn completion_notice(
        &self,
        owner: &JobOwner,
        id: &JobId,
    ) -> Result<Option<JobCompletionNotice>, JobError> {
        Ok(self.owned(owner, id)?.completion_notice())
    }

    fn report(&self, owner: &JobOwner, id: &JobId) -> Result<bool, JobError> {
        let job = self.owned(owner, id)?;
        let changed = job.mark_reported();
        if changed {
            notify_changed(&Arc::downgrade(&self.inner), &job);
        }
        Ok(changed)
    }

    async fn dispose_owner(&self, owner: &JobOwner) {
        match self.begin_owner_dispose(owner) {
            Disposal::Start(jobs) => self.stop_and_join(jobs).await,
            Disposal::Join(completion) if !self.current_job_is_owned_by(owner) => {
                self.wait_for_owner_dispose(owner, completion).await;
            }
            Disposal::Join(_) | Disposal::Done => {}
        }
    }

    fn begin_owner_dispose(&self, owner: &JobOwner) -> Disposal {
        let mut state = lock(&self.inner.state);
        let Some(attached) = state.owners.get_mut(&owner.owner) else {
            return Disposal::Done;
        };
        if attached.generation != owner.generation
            || !attached.authority.same_authority(&owner.authority)
        {
            return Disposal::Done;
        }
        if attached.disposing {
            return Disposal::Join(
                attached
                    .completion
                    .clone()
                    .expect("disposing owner has completion"),
            );
        }
        attached.disposing = true;
        attached.completion = Some(Arc::new(Notify::new()));
        Disposal::Start(
            state
                .jobs
                .values()
                .filter(|job| {
                    job.owner == owner.owner
                        && job.owner_generation == owner.generation
                        && !job.snapshot().status.is_terminal()
                })
                .cloned()
                .collect(),
        )
    }

    fn begin_dispose(&self) -> Disposal {
        let mut state = lock(&self.inner.state);
        if state.disposed {
            return if state.dispose_finished {
                Disposal::Done
            } else {
                Disposal::Join(
                    state
                        .dispose_completion
                        .clone()
                        .expect("disposing registry has completion"),
                )
            };
        }
        state.disposed = true;
        state.dispose_finished = false;
        state.dispose_completion = Some(Arc::new(Notify::new()));
        for owner in state.owners.values_mut() {
            if !owner.disposing {
                owner.disposing = true;
                owner.completion = Some(Arc::new(Notify::new()));
            }
        }
        Disposal::Start(
            state
                .jobs
                .values()
                .filter(|job| !job.snapshot().status.is_terminal())
                .cloned()
                .collect(),
        )
    }

    async fn stop_and_join(&self, jobs: Vec<Arc<Job>>) {
        for job in &jobs {
            if job.request_stop() {
                notify_changed(&Arc::downgrade(&self.inner), job);
            }
        }
        for job in jobs {
            if !self.current_job_is(&job) {
                let _ = wait_for(&job, None, None).await;
            }
        }
        self.finish_disposals();
    }

    async fn wait_for_dispose(&self, completion: Arc<Notify>) {
        loop {
            let notified = completion.notified();
            if lock(&self.inner.state).dispose_finished {
                return;
            }
            notified.await;
        }
    }

    async fn wait_for_owner_dispose(&self, owner: &JobOwner, completion: Arc<Notify>) {
        loop {
            let notified = completion.notified();
            let still_disposing = lock(&self.inner.state)
                .owners
                .get(&owner.owner)
                .is_some_and(|attached| {
                    attached.generation == owner.generation
                        && attached.authority.same_authority(&owner.authority)
                        && attached.disposing
                });
            if !still_disposing {
                return;
            }
            notified.await;
        }
    }

    fn finish_disposals(&self) {
        let notifications = {
            let mut state = lock(&self.inner.state);
            let removable: Vec<_> = state
                .owners
                .iter()
                .filter(|entry| {
                    let (id, owner) = *entry;
                    owner.disposing
                        && !state.jobs.values().any(|job| {
                            job.owner == *id
                                && job.owner_generation == owner.generation
                                && !job.snapshot().status.is_terminal()
                        })
                })
                .map(|entry| entry.0.clone())
                .collect();
            let mut notifications: Vec<_> = removable
                .into_iter()
                .filter_map(|id| state.owners.remove(&id).and_then(|owner| owner.completion))
                .collect();
            if state.disposed
                && !state.dispose_finished
                && state
                    .jobs
                    .values()
                    .all(|job| job.snapshot().status.is_terminal())
            {
                state.dispose_finished = true;
                if let Some(completion) = &state.dispose_completion {
                    notifications.push(Arc::clone(completion));
                }
            }
            notifications
        };
        for notification in notifications {
            notification.notify_waiters();
        }
    }

    fn remove_owner(&self, owner: &JobOwner) {
        let mut state = lock(&self.inner.state);
        if state.owners.get(&owner.owner).is_some_and(|attached| {
            attached.generation == owner.generation
                && attached.authority.same_authority(&owner.authority)
        }) {
            state.owners.remove(&owner.owner);
        }
    }

    fn authorize_owner(state: &RegistryState, owner: &JobOwner) -> Result<(), JobError> {
        if !owner.authority.is_live() {
            return Err(JobError::OwnerNotAttached);
        }

        if state.disposed {
            return Err(JobError::Disposed);
        }
        match state.owners.get(&owner.owner) {
            Some(attached)
                if attached.generation == owner.generation
                    && attached.authority.same_authority(&owner.authority)
                    && !attached.disposing =>
            {
                Ok(())
            }
            Some(attached)
                if attached.generation == owner.generation
                    && attached.authority.same_authority(&owner.authority) =>
            {
                Err(JobError::OwnerDisposing)
            }
            _ => Err(JobError::OwnerNotAttached),
        }
    }

    fn owned(&self, owner: &JobOwner, id: &JobId) -> Result<Arc<Job>, JobError> {
        let state = lock(&self.inner.state);
        Self::authorize_owner(&state, owner)?;
        state
            .jobs
            .get(id)
            .filter(|job| job.owner == owner.owner && job.owner_generation == owner.generation)
            .cloned()
            .ok_or(JobError::NotFound)
    }

    fn current_job_is(&self, job: &Arc<Job>) -> bool {
        RUNNING_JOB
            .try_with(|current| Arc::ptr_eq(current, job))
            .unwrap_or(false)
    }

    fn current_job_is_registered(&self) -> bool {
        RUNNING_JOB
            .try_with(|current| {
                lock(&self.inner.state)
                    .jobs
                    .get(&current.id)
                    .is_some_and(|known| Arc::ptr_eq(known, current))
            })
            .unwrap_or(false)
    }

    fn current_job_is_owned_by(&self, owner: &JobOwner) -> bool {
        RUNNING_JOB
            .try_with(|current| {
                lock(&self.inner.state)
                    .jobs
                    .get(&current.id)
                    .is_some_and(|known| {
                        Arc::ptr_eq(known, current)
                            && known.owner == owner.owner
                            && known.owner_generation == owner.generation
                    })
            })
            .unwrap_or(false)
    }

    pub fn on_done<F>(&self, observer: F) -> JobObserverRegistration
    where
        F: Fn(&JobSnapshot) + Send + Sync + 'static,
    {
        self.observe(ObserverKind::Done, Arc::new(observer))
    }

    pub fn on_changed<F>(&self, observer: F) -> JobObserverRegistration
    where
        F: Fn(&JobSnapshot) + Send + Sync + 'static,
    {
        self.observe(ObserverKind::Changed, Arc::new(observer))
    }
    fn observe(&self, kind: ObserverKind, observer: JobObserver) -> JobObserverRegistration {
        let id = {
            let mut state = lock(&self.inner.state);
            let id = state.next_observer;
            state.next_observer = state.next_observer.checked_add(1).unwrap_or(1);
            match kind {
                ObserverKind::Done => state.done_observers.insert(id, observer),
                ObserverKind::Changed => state.change_observers.insert(id, observer),
            };
            id
        };
        JobObserverRegistration {
            inner: Arc::downgrade(&self.inner),
            id,
            kind,
            closed: AtomicBool::new(false),
        }
    }
}

/// Scope-bound, generation-bearing authority for exactly one live agent attachment.
#[derive(Clone, Debug)]
pub struct JobOwner {
    registry: LocalJobRegistry,
    authority: AgentAuthority,
    owner: SessionId,
    generation: u64,
}

impl JobOwner {
    pub fn start(&self, start: JobStart) -> Result<JobSnapshot, JobError> {
        self.registry.start(self, start)
    }

    pub fn list(&self) -> Result<Vec<JobSnapshot>, JobError> {
        self.registry.list(self)
    }

    pub fn get(&self, id: &JobId) -> Result<JobSnapshot, JobError> {
        self.registry.get(self, id)
    }

    pub fn read(
        &self,
        id: &JobId,
        cursor: u64,
        max_bytes: usize,
    ) -> Result<JobOutputRead, JobError> {
        self.registry.read(self, id, cursor, max_bytes)
    }

    pub fn kill(&self, id: &JobId) -> Result<JobSnapshot, JobError> {
        self.registry.kill(self, id)
    }

    pub async fn wait(
        &self,
        id: &JobId,
        timeout: Option<Duration>,
        cancellation: Option<CancellationToken>,
    ) -> Result<JobSnapshot, JobError> {
        self.registry.wait(self, id, timeout, cancellation).await
    }

    /// Returns a terminal notification until [`Self::report`] records its durable delivery.
    pub fn completion_notice(&self, id: &JobId) -> Result<Option<JobCompletionNotice>, JobError> {
        self.registry.completion_notice(self, id)
    }

    /// Records that the caller durably delivered a terminal completion notice.
    pub fn report(&self, id: &JobId) -> Result<bool, JobError> {
        self.registry.report(self, id)
    }

    /// Registers tools that retain this owner capability instead of trusting a raw session ID.
    pub fn install_tools(&self, runtime: &ToolRuntime) -> Result<JobTools, TessivumError> {
        self.registry.install_tools(self.clone(), runtime)
    }

    pub async fn dispose(&self) {
        self.registry.dispose_owner(self).await;
    }
}

#[derive(Clone, Copy)]
enum ObserverKind {
    Done,
    Changed,
}

/// Lifetime-owned observer registration. Panicking observers are contained.
pub struct JobObserverRegistration {
    inner: Weak<RegistryInner>,
    id: u64,
    kind: ObserverKind,
    closed: AtomicBool,
}

impl JobObserverRegistration {
    pub fn close(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        match self.kind {
            ObserverKind::Done => lock(&inner.state).done_observers.remove(&self.id),
            ObserverKind::Changed => lock(&inner.state).change_observers.remove(&self.id),
        }
        .is_some()
    }
}

impl Drop for JobObserverRegistration {
    fn drop(&mut self) {
        self.close();
    }
}

/// Keeps all four tool registrations alive together.
pub struct JobTools {
    registrations: Vec<ToolRegistration>,
}

impl JobTools {
    pub fn len(&self) -> usize {
        self.registrations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }
}

#[derive(Clone, Copy)]
enum JobToolAction {
    List,
    Read,
    Kill,
    Wait,
}

impl JobToolAction {
    fn name(self) -> &'static str {
        match self {
            Self::List => "jobs.list",
            Self::Read => "jobs.read",
            Self::Kill => "jobs.kill",
            Self::Wait => "jobs.wait",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::List => "Lists jobs owned by this session.",
            Self::Read => "Reads a bounded output page from one owned job.",
            Self::Kill => "Requests cancellation of one owned job.",
            Self::Wait => "Waits a bounded time for one owned job without cancelling it.",
        }
    }

    fn schema(self) -> Value {
        match self {
            Self::List => json!({"type":"object","properties":{},"additionalProperties":false}),
            Self::Read => json!({
                "type":"object",
                "properties":{"jobId":{"type":"string"},"cursor":{"type":"integer"},"maxBytes":{"type":"integer"}},
                "required":["jobId","cursor","maxBytes"],
                "additionalProperties":false
            }),
            Self::Kill => json!({
                "type":"object",
                "properties":{"jobId":{"type":"string"}},
                "required":["jobId"],
                "additionalProperties":false
            }),
            Self::Wait => json!({
                "type":"object",
                "properties":{"jobId":{"type":"string"},"timeoutMs":{"type":"integer"}},
                "required":["jobId"],
                "additionalProperties":false
            }),
        }
    }
}

struct JobTool {
    owner: JobOwner,
    action: JobToolAction,
}

#[async_trait::async_trait]
impl ToolHandler for JobTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        if context.session != self.owner.owner {
            return Err(job_tool_error(JobError::NotFound));
        }
        let owner = &self.owner;
        let output = match self.action {
            JobToolAction::List => serde_json::to_value(owner.list().map_err(job_tool_error)?)
                .expect("job snapshots are serializable"),
            JobToolAction::Read => {
                let id = tool_id(&arguments)?;
                let cursor = tool_u64(&arguments, "cursor")?;
                let max_bytes = tool_u64(&arguments, "maxBytes")?;
                let max_bytes = usize::try_from(max_bytes)
                    .map_err(|_| job_tool_error(JobError::ReadTooLarge))?;
                let read = owner.read(&id, cursor, max_bytes).map_err(job_tool_error)?;
                json!({
                    "cursor": read.cursor,
                    "nextCursor": read.next_cursor,
                    "totalBytes": read.total_bytes,
                    "availableFrom": read.available_from,
                    "lost": read.lost,
                    "text": String::from_utf8_lossy(&read.bytes),
                })
            }
            JobToolAction::Kill => {
                let id = tool_id(&arguments)?;
                serde_json::to_value(owner.kill(&id).map_err(job_tool_error)?)
                    .expect("job snapshots are serializable")
            }
            JobToolAction::Wait => {
                let id = tool_id(&arguments)?;
                let timeout = match arguments.get("timeoutMs") {
                    None => Some(MAX_WAIT),
                    Some(value) => {
                        let milliseconds = value.as_u64().ok_or_else(|| {
                            tool_error(
                                "INVALID_JOB_TIMEOUT",
                                "timeoutMs must be a non-negative integer",
                            )
                        })?;
                        let duration = Duration::from_millis(milliseconds);
                        if duration > MAX_WAIT {
                            return Err(tool_error(
                                "JOB_WAIT_TOO_LONG",
                                "timeoutMs exceeds the job wait limit",
                            ));
                        }
                        Some(duration)
                    }
                };
                let snapshot = owner
                    .wait(&id, timeout, Some(context.cancellation.clone()))
                    .await
                    .map_err(job_tool_error)?;
                serde_json::to_value(snapshot).expect("job snapshots are serializable")
            }
        };
        Ok(json_output(output))
    }
}

async fn run_job(registry: Weak<RegistryInner>, job: Arc<Job>, run: JobRunHook) {
    let fallback = RUNNING_JOB
        .scope(Arc::clone(&job), async {
            let control = JobControl {
                job: Arc::clone(&job),
                registry: registry.clone(),
            };
            match catch_unwind(AssertUnwindSafe(|| run(control))) {
                Ok(future) => match AssertUnwindSafe(future).catch_unwind().await {
                    Ok(Ok(result)) => Settlement::Completed(result),
                    Ok(Err(error)) => Settlement::Failed(error),
                    Err(_) => Settlement::Failed("job run hook panicked".into()),
                },
                Err(_) => Settlement::Failed("job run hook panicked".into()),
            }
        })
        .await;
    // The future and its captured resources have been dropped before this state is terminal.
    drop(run);
    if job.finalize(fallback) {
        notify_changed(&registry, &job);
        notify_done(&registry, &job);
        job.done.notify_waiters();
        if let Some(inner) = registry.upgrade() {
            LocalJobRegistry { inner }.finish_disposals();
        }
    }
}

async fn wait_for(
    job: &Arc<Job>,
    timeout: Option<Duration>,
    cancellation: Option<CancellationToken>,
) -> Result<JobSnapshot, JobError> {
    let deadline = timeout.map(|duration| time::Instant::now() + duration);
    loop {
        let notified = job.done.notified();
        let snapshot = job.snapshot();
        if snapshot.status.is_terminal() {
            return Ok(snapshot);
        }
        match (deadline, cancellation.clone()) {
            (Some(deadline), Some(cancellation)) => tokio::select! {
                _ = notified => {},
                _ = cancellation.cancelled() => return Err(JobError::Cancelled),
                _ = time::sleep_until(deadline) => return Err(JobError::TimedOut),
            },
            (Some(deadline), None) => tokio::select! {
                _ = notified => {},
                _ = time::sleep_until(deadline) => return Err(JobError::TimedOut),
            },
            (None, Some(cancellation)) => tokio::select! {
                _ = notified => {},
                _ = cancellation.cancelled() => return Err(JobError::Cancelled),
            },
            (None, None) => notified.await,
        }
    }
}

fn notify_changed(inner: &Weak<RegistryInner>, job: &Arc<Job>) {
    notify(inner, job, false);
}

fn notify_done(inner: &Weak<RegistryInner>, job: &Arc<Job>) {
    notify(inner, job, true);
}

fn notify(inner: &Weak<RegistryInner>, job: &Arc<Job>, done: bool) {
    let Some(inner) = inner.upgrade() else {
        return;
    };
    let observers: Vec<_> = {
        let state = lock(&inner.state);
        if done {
            state.done_observers.values().cloned().collect()
        } else {
            state.change_observers.values().cloned().collect()
        }
    };
    let snapshot = job.snapshot();
    for observer in observers {
        let _ = catch_unwind(AssertUnwindSafe(|| observer(&snapshot)));
    }
}

fn tool_id(arguments: &Value) -> Result<JobId, TessivumError> {
    let Some(value) = arguments.get("jobId").and_then(Value::as_str) else {
        return Err(tool_error(
            "INVALID_JOB_ID",
            "jobId must be a generated job identifier",
        ));
    };
    JobId::parse(value).map_err(job_tool_error)
}

fn tool_u64(arguments: &Value, name: &str) -> Result<u64, TessivumError> {
    arguments.get(name).and_then(Value::as_u64).ok_or_else(|| {
        tool_error(
            "INVALID_JOB_ARGUMENT",
            format!("{name} must be a non-negative integer"),
        )
    })
}

fn json_output(value: Value) -> ToolOutput {
    ToolOutput::new(
        vec![ContentBlock::Text {
            text: serde_json::to_string(&value).expect("JSON value serializes"),
        }],
        false,
        Value::Null,
    )
}

fn job_tool_error(error: JobError) -> TessivumError {
    let (code, message) = match error {
        JobError::NotFound | JobError::OwnerNotAttached => ("JOB_NOT_FOUND", "job does not exist"),
        JobError::OwnerDisposing | JobError::Disposed => {
            ("JOB_UNAVAILABLE", "job registry is unavailable")
        }
        JobError::InvalidJobId => ("INVALID_JOB_ID", "job id is invalid"),
        JobError::InvalidCursor => ("INVALID_JOB_CURSOR", "job output cursor is invalid"),
        JobError::ReadTooLarge => ("JOB_READ_TOO_LARGE", "job output read exceeds the limit"),
        JobError::TimedOut => ("JOB_WAIT_TIMEOUT", "job did not finish before timeout"),
        JobError::Cancelled => ("CANCELLED", "job wait was cancelled"),
        JobError::InvalidKind | JobError::InvalidLabel | JobError::InvalidOutputLimit => {
            ("INVALID_JOB", "job request is invalid")
        }
        JobError::IdExhausted => ("JOB_ID_EXHAUSTED", "job id sequence is exhausted"),
        JobError::ControllerNotAttached => (
            "JOB_CONTROLLER_UNAVAILABLE",
            "job controller is unavailable",
        ),
        JobError::Core(error) => {
            return TessivumError::new("JOB_CORE_ERROR", error.to_string(), "jobs", Value::Null)
        }
    };
    TessivumError::new(code, message, "jobs", Value::Null)
}

fn tool_error(code: impl Into<String>, message: impl Into<String>) -> TessivumError {
    TessivumError::new(code, message, "jobs", Value::Null)
}

fn valid_kind(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && !value.ends_with('-')
}

fn trim_text(mut value: String) -> String {
    if value.len() > MAX_TEXT_BYTES {
        let mut boundary = MAX_TEXT_BYTES;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
    }
    value
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

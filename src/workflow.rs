//! One durable workflow engine over untyped JSON input.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::{broadcast, Notify};

use crate::{
    agent::AgentHandle,
    protocol::SessionEvent,
    session::{Session, SessionError, SessionStore},
    subagent::{
        SubagentActivation, SubagentError, SubagentParent, SubagentRunResult, SubagentService,
        SubagentStartRequest,
    },
    TessivumError,
};

/// Stable service key for the single workflow engine capability.
pub fn workflow_service_key() -> ServiceKey {
    ServiceKey::new("harness.workflow", "1")
}

/// Plain JSON accepted by the workflow engine. The runtime intentionally does
/// not interpret script, metadata, or arguments.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRequest {
    pub script: Value,
    pub meta: Value,
    pub args: Value,
}

/// Observable workflow lifecycle facts. They never affect engine control flow.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowEvent {
    pub run_id: u64,
    pub kind: WorkflowEventKind,
    pub name: String,
    pub data: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowEventKind {
    PhaseStart,
    PhaseEnd,
    LogStart,
    LogEnd,
    AgentStart,
    AgentEnd,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowRunStatus {
    Completed,
    Cancelled,
    Error,
}

/// A terminal workflow result. Operational engine failures are values, not
/// rejected workflow invocations.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunResult {
    pub run_id: u64,
    pub status: WorkflowRunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WorkflowFailure>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowFailure {
    pub code: String,
    pub message: String,
}

/// Fatal caller misuse. Once a run is admitted, its result is always a
/// [`WorkflowRunResult`].
#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("parent session is required and must be live")]
    ParentRequired,
    #[error("maximum total agents must be at least one")]
    InvalidAgentLimit,
    #[error("workflow child limit exceeded")]
    AgentLimitExceeded,
    #[error("workflow child admission is closing")]
    Closing,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Subagent(#[from] SubagentError),
    #[error(transparent)]
    Protocol(#[from] TessivumError),
}

/// The sole engine implementation used by a runtime instance.
#[async_trait]
pub trait WorkflowEngine: Send + Sync {
    async fn run(
        &self,
        context: WorkflowContext,
        request: WorkflowRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError>;
}

struct ChildAdmissions {
    closing: bool,
    pending: usize,
    children: Vec<SubagentActivation>,
    late_children: Vec<SubagentActivation>,
}

struct WorkflowAdmissionPermit {
    state: Arc<RunState>,
    released: bool,
}

impl WorkflowAdmissionPermit {
    fn admit_or_queue(&self, activation: SubagentActivation) -> bool {
        let mut admissions = self.state.children.lock();
        if admissions.closing {
            admissions.late_children.push(activation);
            false
        } else {
            admissions.children.push(activation);
            true
        }
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let wake = {
            let mut admissions = self.state.children.lock();
            admissions.pending -= 1;
            admissions.closing && admissions.pending == 0
        };
        if wake {
            self.state.admissions_quiesced.notify_waiters();
        }
    }
}

impl Drop for WorkflowAdmissionPermit {
    fn drop(&mut self) {
        self.release();
    }
}

struct RunState {
    run_id: u64,
    parent: Arc<Session>,
    cancellation: CancellationToken,
    subagents: SubagentParent,
    children: Mutex<ChildAdmissions>,
    admissions_quiesced: Notify,
    cleanup_started: AtomicBool,
    cleanup_done: AtomicBool,
    cleanup_finished: Notify,
    cleanup_results: Mutex<Vec<SubagentRunResult>>,
}

impl RunState {
    fn reserve_admission(self: &Arc<Self>) -> Option<WorkflowAdmissionPermit> {
        let mut admissions = self.children.lock();
        if admissions.closing {
            return None;
        }
        admissions.pending += 1;
        Some(WorkflowAdmissionPermit {
            state: Arc::clone(self),
            released: false,
        })
    }

    fn begin_cleanup(self: &Arc<Self>) {
        self.cancellation.cancel();
        if self.cleanup_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let results = cleanup_children(Arc::clone(&state)).await;
            *state.cleanup_results.lock() = results;
            state.cleanup_done.store(true, Ordering::Release);
            state.cleanup_finished.notify_waiters();
        });
    }

    async fn close_and_dispose(self: &Arc<Self>) -> Vec<SubagentRunResult> {
        self.begin_cleanup();
        loop {
            let notified = self.cleanup_finished.notified();
            if self.cleanup_done.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
        self.cleanup_results.lock().clone()
    }
}

struct WorkflowInner {
    sessions: SessionStore,
    subagents: SubagentService,
    engine: Arc<dyn WorkflowEngine>,
    max_total_agents: usize,
    next_run_id: AtomicU64,
    active: Mutex<BTreeMap<u64, Arc<RunState>>>,
    events: broadcast::Sender<WorkflowEvent>,
}

/// The single workflow engine and its parent-owned child cleanup boundary.
#[derive(Clone)]
pub struct WorkflowRuntime {
    inner: Arc<WorkflowInner>,
}

impl fmt::Debug for WorkflowRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowRuntime")
            .field("active_runs", &lock(&self.inner.active).len())
            .field("max_total_agents", &self.inner.max_total_agents)
            .finish_non_exhaustive()
    }
}

/// An opaque capability attached to one live workflow-parent agent generation.
#[derive(Clone)]
pub struct WorkflowParent {
    inner: Arc<WorkflowInner>,
    parent: Arc<AgentHandle>,
}

impl fmt::Debug for WorkflowParent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowParent")
            .field("parent", &self.parent.id())
            .finish_non_exhaustive()
    }
}

impl WorkflowParent {
    /// Runs one workflow under this parent generation.
    pub async fn run(
        &self,
        request: WorkflowRequest,
        caller_cancellation: CancellationToken,
    ) -> Result<WorkflowRunResult, WorkflowError> {
        run_workflow(&self.inner, &self.parent, request, caller_cancellation).await
    }
}

/// An opaque accepted run capability. It is the only way to cancel a run;
/// numeric run IDs remain observational data only.
#[derive(Clone)]
pub struct WorkflowRun {
    state: Arc<RunState>,
}

impl WorkflowRun {
    pub fn run_id(&self) -> u64 {
        self.state.run_id
    }

    pub async fn dispose(&self) -> Vec<SubagentRunResult> {
        self.state.close_and_dispose().await
    }
}

impl WorkflowRuntime {
    pub fn new(
        sessions: SessionStore,
        subagents: SubagentService,
        engine: Arc<dyn WorkflowEngine>,
        max_total_agents: usize,
    ) -> Result<Self, WorkflowError> {
        if max_total_agents == 0 {
            return Err(WorkflowError::InvalidAgentLimit);
        }
        let (events, _) = broadcast::channel(128);
        Ok(Self {
            inner: Arc::new(WorkflowInner {
                sessions,
                subagents,
                engine,
                max_total_agents,
                next_run_id: AtomicU64::new(0),
                active: Mutex::new(BTreeMap::new()),
                events,
            }),
        })
    }

    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(workflow_service_key(), self)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WorkflowEvent> {
        self.inner.events.subscribe()
    }

    /// Derives a workflow parent capability from the live owner attachment.
    pub fn attach(&self, parent: Arc<AgentHandle>) -> Result<WorkflowParent, WorkflowError> {
        require_live_parent(&self.inner, &parent)?;
        Ok(WorkflowParent {
            inner: Arc::clone(&self.inner),
            parent,
        })
    }
}

/// Engine-facing context. It exposes only observation, a run capability, and
/// parent-bound child creation; none accepts caller-supplied authority IDs.
#[derive(Clone)]
pub struct WorkflowContext {
    observer: WorkflowObserver,
    run: WorkflowRun,
}

impl WorkflowContext {
    pub fn run_id(&self) -> u64 {
        self.observer.run_id
    }

    pub fn run(&self) -> WorkflowRun {
        self.run.clone()
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.observer.state.cancellation.clone()
    }

    pub async fn phase_start(&self, name: impl Into<String>, data: Value) {
        self.observer
            .emit(WorkflowEventKind::PhaseStart, name.into(), data)
            .await;
    }

    pub async fn phase_end(&self, name: impl Into<String>, data: Value) {
        self.observer
            .emit(WorkflowEventKind::PhaseEnd, name.into(), data)
            .await;
    }

    /// A log is represented by a contained start/end pair, preserving paired
    /// durable observation without giving log writes control over the engine.
    pub async fn log(&self, name: impl Into<String>, data: Value) {
        let name = name.into();
        self.observer
            .emit(WorkflowEventKind::LogStart, name.clone(), data.clone())
            .await;
        self.observer
            .emit(WorkflowEventKind::LogEnd, name, data)
            .await;
    }

    /// Starts a bounded child and admits it through the closing gate. A child
    /// accepted while cleanup begins is disposed before this call returns.
    pub async fn start_agent(
        &self,
        request: SubagentStartRequest,
    ) -> Result<SubagentActivation, WorkflowError> {
        let count = self.observer.agents.fetch_add(1, Ordering::AcqRel) + 1;
        if count > self.observer.inner.max_total_agents {
            self.observer.agents.fetch_sub(1, Ordering::AcqRel);
            return Err(WorkflowError::AgentLimitExceeded);
        }
        let mut permit = self
            .observer
            .state
            .reserve_admission()
            .ok_or(WorkflowError::Closing)?;
        let agent_name = request.agent_id.clone();
        let (_, activation) = self
            .observer
            .state
            .subagents
            .start(request, self.observer.state.cancellation.clone())
            .await?;
        let admitted = permit.admit_or_queue(activation.clone());
        permit.release();
        if !admitted {
            self.observer.state.begin_cleanup();
            return Err(WorkflowError::Closing);
        }
        self.observer
            .emit(
                WorkflowEventKind::AgentStart,
                agent_name,
                json!({"acceptanceId": activation.acceptance_id()}),
            )
            .await;
        Ok(activation)
    }

    pub async fn end_agent(&self, activation: &SubagentActivation, result: &SubagentRunResult) {
        self.observer
            .emit(
                WorkflowEventKind::AgentEnd,
                activation.acceptance_id().to_string(),
                json!({"status": result.status, "error": result.error}),
            )
            .await;
    }
}

#[derive(Clone)]
struct WorkflowObserver {
    inner: Arc<WorkflowInner>,
    state: Arc<RunState>,
    run_id: u64,
    failed_write: Arc<AtomicBool>,
    agents: Arc<AtomicUsize>,
}

impl WorkflowObserver {
    async fn append_run(&self, request: &WorkflowRequest) -> Result<(), SessionError> {
        append(
            &self.state.parent,
            "workflow/run",
            json!({"runId": self.run_id, "script": request.script, "meta": request.meta, "args": request.args}),
        )
        .await
    }

    async fn append_run_end(&self, data: Value) -> Result<(), SessionError> {
        // Always attempt the terminal marker. It has an independent root token,
        // even when a prior member append failed.
        append(
            &self.state.parent,
            "workflow/run-end",
            json!({"runId": self.run_id, "result": data}),
        )
        .await
    }

    async fn emit(&self, kind: WorkflowEventKind, name: String, data: Value) {
        if name.trim().is_empty() {
            return;
        }
        let event = WorkflowEvent {
            run_id: self.run_id,
            kind,
            name,
            data,
        };
        let _ = self.inner.events.send(event.clone());
        if self.failed_write.load(Ordering::Acquire) {
            return;
        }
        if append(
            &self.state.parent,
            "workflow/member",
            json!({"runId": self.run_id, "kind": event.kind, "name": event.name, "data": event.data}),
        )
        .await
        .is_err()
        {
            self.failed_write.store(true, Ordering::Release);
        }
    }
}

async fn run_workflow(
    inner: &Arc<WorkflowInner>,
    parent_agent: &Arc<AgentHandle>,
    request: WorkflowRequest,
    caller_cancellation: CancellationToken,
) -> Result<WorkflowRunResult, WorkflowError> {
    let parent = require_live_parent(inner, parent_agent)?;
    let subagents = inner.subagents.attach(Arc::clone(parent_agent))?;
    let run_id = inner
        .next_run_id
        .fetch_add(1, Ordering::Relaxed)
        .checked_add(1)
        .unwrap_or(1);
    let state = Arc::new(RunState {
        run_id,
        parent,
        cancellation: ContextHandle::root().scope().cancellation(),
        subagents,
        children: Mutex::new(ChildAdmissions {
            closing: false,
            pending: 0,
            children: Vec::new(),
            late_children: Vec::new(),
        }),
        admissions_quiesced: Notify::new(),
        cleanup_started: AtomicBool::new(false),
        cleanup_done: AtomicBool::new(false),
        cleanup_finished: Notify::new(),
        cleanup_results: Mutex::new(Vec::new()),
    });
    let observer = WorkflowObserver {
        inner: Arc::clone(inner),
        state: Arc::clone(&state),
        run_id,
        failed_write: Arc::new(AtomicBool::new(false)),
        agents: Arc::new(AtomicUsize::new(0)),
    };
    // A failed first append is pre-admission: no engine, children, or run-end.
    if let Err(error) = observer.append_run(&request).await {
        observer.failed_write.store(true, Ordering::Release);
        return Ok(error_result(
            run_id,
            "WORKFLOW_EVENT_APPEND_FAILED",
            error.to_string(),
        ));
    }
    lock(&inner.active).insert(run_id, Arc::clone(&state));

    let parent_cancellation = parent_agent.cancellation();
    let result = if caller_cancellation.is_cancelled() || parent_cancellation.is_cancelled() {
        state.cancellation.cancel();
        cancelled_result(run_id)
    } else {
        let execution = inner.engine.run(
            WorkflowContext {
                observer: observer.clone(),
                run: WorkflowRun {
                    state: Arc::clone(&state),
                },
            },
            request,
            state.cancellation.clone(),
        );
        tokio::pin!(execution);
        let outcome = tokio::select! {
            outcome = &mut execution => outcome,
            _ = caller_cancellation.cancelled() => {
                state.cancellation.cancel();
                execution.await
            }
            _ = parent_cancellation.cancelled() => {
                state.cancellation.cancel();
                execution.await
            }
        };
        if state.cancellation.is_cancelled() {
            cancelled_result(run_id)
        } else {
            match outcome {
                Ok(value) => WorkflowRunResult {
                    run_id,
                    status: WorkflowRunStatus::Completed,
                    value: Some(value),
                    error: None,
                },
                Err(error) => error_result(run_id, error.code, error.message),
            }
        }
    };

    let child_results = state.close_and_dispose().await;
    let mut result = result;
    if child_results
        .iter()
        .any(|child| matches!(child.status, crate::subagent::SubagentRunStatus::Error))
    {
        result = error_result(
            run_id,
            "WORKFLOW_CHILD_FAILED",
            "a workflow child failed during shutdown",
        );
    }
    if observer.failed_write.load(Ordering::Acquire) {
        result = error_result(
            run_id,
            "WORKFLOW_EVENT_APPEND_FAILED",
            "workflow event persistence failed",
        );
    }
    let end_data = json!({"status": result.status, "value": result.value, "error": result.error});
    if observer.append_run_end(end_data).await.is_err() {
        result = error_result(
            run_id,
            "WORKFLOW_EVENT_APPEND_FAILED",
            "workflow end could not be persisted",
        );
    }
    lock(&inner.active).remove(&run_id);
    Ok(result)
}

async fn cleanup_children(state: Arc<RunState>) -> Vec<SubagentRunResult> {
    let children = {
        let mut admissions = state.children.lock();
        admissions.closing = true;
        std::mem::take(&mut admissions.children)
    };
    let mut results = Vec::with_capacity(children.len());
    for child in children {
        if let Ok(result) = child.dispose().await {
            results.push(result);
        }
    }
    loop {
        let notified = state.admissions_quiesced.notified();
        if state.children.lock().pending == 0 {
            break;
        }
        notified.await;
    }
    let late_children = std::mem::take(&mut state.children.lock().late_children);
    for child in late_children {
        if let Ok(result) = child.dispose().await {
            results.push(result);
        }
    }
    results
}

fn require_live_parent(
    inner: &WorkflowInner,
    parent: &AgentHandle,
) -> Result<Arc<Session>, WorkflowError> {
    if parent.is_disposed() {
        return Err(WorkflowError::ParentRequired);
    }
    let attached = parent.session();
    inner
        .sessions
        .get(&parent.id())
        .filter(|live| Arc::ptr_eq(live, &attached))
        .ok_or(WorkflowError::ParentRequired)
}
async fn append(parent: &Session, event_type: &str, data: Value) -> Result<(), SessionError> {
    let event = SessionEvent {
        event_type: event_type.into(),
        seq: parent.next_seq()?,
        time: 0,
        data,
        ignorable: Some(true),
        source_event_seqs: None,
        surface_op: None,
    };
    parent
        .append(event, ContextHandle::root().scope().cancellation())
        .await
}

fn cancelled_result(run_id: u64) -> WorkflowRunResult {
    WorkflowRunResult {
        run_id,
        status: WorkflowRunStatus::Cancelled,
        value: None,
        error: None,
    }
}

fn error_result(
    run_id: u64,
    code: impl Into<String>,
    message: impl Into<String>,
) -> WorkflowRunResult {
    WorkflowRunResult {
        run_id,
        status: WorkflowRunStatus::Error,
        value: None,
        error: Some(WorkflowFailure {
            code: code.into(),
            message: message.into(),
        }),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    mutex.lock()
}

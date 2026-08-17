use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tessivum::{
    agent::{
        AgentError, AgentFactory, AgentHandle, AgentOptions, AgentRegistry, AgentRuntime,
        AgentStatus, Inbox,
    },
    protocol::{Message, SessionEvent, SessionHeader, SessionId, SESSION_FORMAT_VERSION},
    session::{
        MemorySessionPersistence, SessionError, SessionInspection, SessionPersistence, SessionStore,
    },
    subagent::{
        NativeSubagentProvider, SubagentError, SubagentProvider, SubagentRunStatus,
        SubagentService, SubagentStartRequest,
    },
    workflow::{
        WorkflowContext, WorkflowEngine, WorkflowError, WorkflowRequest, WorkflowRun,
        WorkflowRunStatus, WorkflowRuntime,
    },
    TessivumError,
};
use tessivum_core::{CancellationToken, ContextHandle};
use tokio::sync::{oneshot, Notify};

fn cancellation() -> CancellationToken {
    ContextHandle::root().scope().cancellation()
}

fn header(id: &str, parent: Option<&str>) -> SessionHeader {
    SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: SessionId::from(id),
        created_at: 0,
        cwd: None,
        parent_session: parent.map(SessionId::from),
        seed_length: None,
        origin: None,
        delegation_depth: None,
        agent_preset: None,
    }
}

fn options() -> AgentOptions {
    AgentOptions {
        provider: "fake".into(),
        model: "fake".into(),
        max_tokens: Some(8),
    }
}

fn message(id: &str) -> Message {
    serde_json::from_value(json!({
        "id": id, "role": "user", "content": [{"type": "text", "text": id}], "source": {"kind": "user"}
    }))
    .unwrap()
}

fn request(id: &str) -> SubagentStartRequest {
    SubagentStartRequest {
        provider: "native".into(),
        agent_id: "scout".into(),
        child_session_id: SessionId::from(id),
        capabilities: vec!["scout".into()],
        options: options(),
        created_at: 0,
        cwd: None,
        resume: false,
        initial_message: None,
    }
}

struct Idle;

#[async_trait]
impl AgentRuntime for Idle {
    fn status(&self) -> AgentStatus {
        AgentStatus::Idle
    }
    async fn wake(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn when_idle(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn dispose(&self) -> Result<(), AgentError> {
        Ok(())
    }
}

struct Factory;

#[async_trait]
impl AgentFactory for Factory {
    async fn create(
        &self,
        _: Arc<tessivum::session::Session>,
        _: AgentOptions,
        _: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        Ok(Arc::new(Idle))
    }
}

struct CountingProvider {
    native: NativeSubagentProvider,
    calls: AtomicUsize,
}

#[async_trait]
impl SubagentProvider for CountingProvider {
    fn capabilities(&self) -> BTreeSet<String> {
        self.native.capabilities()
    }

    async fn start(
        &self,
        request: tessivum::subagent::ProviderStart,
        cancellation: CancellationToken,
    ) -> Result<AgentHandle, SubagentError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.native.start(request, cancellation).await
    }
}

struct Harness {
    service: SubagentService,
    provider: Arc<CountingProvider>,
    parent: Arc<AgentHandle>,
    agents: AgentRegistry,
    sessions: SessionStore,
}

async fn setup_with(
    persistence: Arc<dyn SessionPersistence>,
    factory: Arc<dyn AgentFactory>,
) -> Harness {
    let sessions = SessionStore::new(Arc::clone(&persistence));
    let agents = AgentRegistry::new(sessions.clone());
    std::mem::forget(agents.register_factory(factory).unwrap());
    let provider = Arc::new(CountingProvider {
        native: NativeSubagentProvider::new(agents.clone(), ["scout".into()]),
        calls: AtomicUsize::new(0),
    });
    let service = SubagentService::new(agents.clone(), sessions.clone(), persistence);
    std::mem::forget(service.register("native", provider.clone()).unwrap());
    let parent = Arc::new(
        agents
            .create(header("parent", None), options(), cancellation())
            .await
            .unwrap(),
    );
    Harness {
        service,
        provider,
        parent,
        agents,
        sessions,
    }
}

async fn setup() -> Harness {
    setup_with(Arc::new(MemorySessionPersistence::new()), Arc::new(Factory)).await
}

#[tokio::test]
async fn capability_preflight_happens_before_provider_or_events() {
    let harness = setup().await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let mut denied = request("denied");
    denied.capabilities = vec!["admin".into()];
    assert!(matches!(
        parent.start(denied, cancellation()).await,
        Err(SubagentError::CapabilityDenied { .. })
    ));
    assert_eq!(harness.provider.calls.load(Ordering::Acquire), 0);
    assert!(harness.parent.session().events().is_empty());
}

#[tokio::test]
async fn parent_attachment_requires_a_tokio_runtime() {
    let harness = setup().await;
    let service = harness.service.clone();
    let parent = harness.parent.clone();
    let result = std::thread::spawn(move || service.attach(parent))
        .join()
        .unwrap();
    assert!(matches!(result, Err(SubagentError::ParentRuntimeRequired)));
}

#[tokio::test]
async fn parent_capability_is_generation_bound_and_child_control_is_opaque() {
    let harness = setup().await;
    let old_parent = harness.service.attach(harness.parent.clone()).unwrap();
    let (_, child) = old_parent
        .start(request("child"), cancellation())
        .await
        .unwrap();
    assert!(child.followup(message("followup")).await.is_ok());
    assert!(child.interrupt());
    assert_eq!(
        child.run().await.unwrap().status,
        SubagentRunStatus::Cancelled
    );

    harness.parent.dispose().await.unwrap();
    assert!(matches!(
        old_parent.start(request("stale"), cancellation()).await,
        Err(SubagentError::ParentRequired)
    ));
    let replacement = Arc::new(
        harness
            .agents
            .resume(harness.parent.id(), options(), cancellation())
            .await
            .unwrap(),
    );
    let fresh_parent = harness.service.attach(replacement).unwrap();
    let (_, fresh_child) = fresh_parent
        .start(request("fresh"), cancellation())
        .await
        .unwrap();
    fresh_child.dispose().await.unwrap();

    let event_types = harness
        .parent
        .session()
        .events()
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        [
            "subagent/contained-start",
            "subagent/contained-end",
            "subagent/contained-start",
            "subagent/contained-end",
        ]
    );
}

#[tokio::test]
async fn cold_resume_uses_durable_child_header() {
    let harness = setup().await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let (_, child) = parent
        .start(request("child"), cancellation())
        .await
        .unwrap();
    child.dispose().await.unwrap();
    let mut resumed = request("child");
    resumed.resume = true;
    let (_, resumed_child) = parent.start(resumed, cancellation()).await.unwrap();
    assert_eq!(
        resumed_child.run().await.unwrap().status,
        SubagentRunStatus::Completed
    );
}

#[tokio::test]
async fn parent_handle_disposal_closes_and_joins_direct_children() {
    let harness = setup().await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let (_, child) = parent
        .start(request("owned-child"), cancellation())
        .await
        .unwrap();
    harness.parent.dispose().await.unwrap();
    let results = parent.dispose().await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, SubagentRunStatus::Cancelled);
    assert!(matches!(child.run().await, Err(SubagentError::AlreadyRun)));
}

struct WakeFails;

#[async_trait]
impl AgentRuntime for WakeFails {
    fn status(&self) -> AgentStatus {
        AgentStatus::Idle
    }
    async fn wake(&self) -> Result<(), AgentError> {
        Err(AgentError::Runtime("delivery failed".into()))
    }
    async fn when_idle(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn dispose(&self) -> Result<(), AgentError> {
        Ok(())
    }
}

struct WakeFailFactory;

#[async_trait]
impl AgentFactory for WakeFailFactory {
    async fn create(
        &self,
        _: Arc<tessivum::session::Session>,
        _: AgentOptions,
        _: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        Ok(Arc::new(WakeFails))
    }
}

#[tokio::test]
async fn precommit_initial_delivery_failure_disposes_without_contained_end() {
    let harness = setup_with(
        Arc::new(MemorySessionPersistence::new()),
        Arc::new(WakeFailFactory),
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let mut start = request("delivery-fails");
    start.initial_message = Some(message("initial"));
    assert!(matches!(
        parent.start(start, cancellation()).await,
        Err(SubagentError::Agent(AgentError::Runtime(_)))
    ));
    assert!(harness.parent.session().events().is_empty());
}

struct BlockingIdle {
    idle_started: Arc<Notify>,
    release_idle: Arc<Notify>,
}

#[async_trait]
impl AgentRuntime for BlockingIdle {
    fn status(&self) -> AgentStatus {
        AgentStatus::Idle
    }
    async fn wake(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn when_idle(&self) -> Result<(), AgentError> {
        self.idle_started.notify_one();
        self.release_idle.notified().await;
        Ok(())
    }
    async fn dispose(&self) -> Result<(), AgentError> {
        Ok(())
    }
}

struct BlockingIdleFactory {
    idle_started: Arc<Notify>,
    release_idle: Arc<Notify>,
}

#[async_trait]
impl AgentFactory for BlockingIdleFactory {
    async fn create(
        &self,
        _: Arc<tessivum::session::Session>,
        _: AgentOptions,
        _: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        Ok(Arc::new(BlockingIdle {
            idle_started: self.idle_started.clone(),
            release_idle: self.release_idle.clone(),
        }))
    }
}

#[tokio::test]
async fn interrupt_proceeds_while_child_waits_for_idle() {
    let idle_started = Arc::new(Notify::new());
    let release_idle = Arc::new(Notify::new());
    let harness = setup_with(
        Arc::new(MemorySessionPersistence::new()),
        Arc::new(BlockingIdleFactory {
            idle_started: idle_started.clone(),
            release_idle: release_idle.clone(),
        }),
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let (_, child) = parent
        .start(request("blocking-child"), cancellation())
        .await
        .unwrap();
    let running = tokio::spawn({
        let child = child.clone();
        async move { child.run().await.unwrap() }
    });
    idle_started.notified().await;
    assert!(child.interrupt());
    release_idle.notify_one();
    assert_eq!(running.await.unwrap().status, SubagentRunStatus::Cancelled);
}

struct FailingEngine;

#[async_trait]
impl WorkflowEngine for FailingEngine {
    async fn run(
        &self,
        context: WorkflowContext,
        _: WorkflowRequest,
        _: CancellationToken,
    ) -> Result<Value, TessivumError> {
        context.phase_start("parallel", Value::Null).await;
        context.phase_end("parallel", Value::Null).await;
        Err(TessivumError::new(
            "PARALLEL_FAILED",
            "child failed",
            "workflow",
            Value::Null,
        ))
    }
}

#[tokio::test]
async fn workflow_failure_is_a_result_and_durable_prefixes_are_legal() {
    let harness = setup().await;
    let workflow = WorkflowRuntime::new(
        harness.sessions,
        harness.service,
        Arc::new(FailingEngine),
        2,
    )
    .unwrap();
    let parent = workflow.attach(harness.parent.clone()).unwrap();
    let result = parent
        .run(
            WorkflowRequest {
                script: json!({"parallel": true}),
                meta: Value::Null,
                args: Value::Null,
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, WorkflowRunStatus::Error);
    let events = harness.parent.session().events();
    let event_types = events
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        [
            "workflow/run",
            "workflow/member",
            "workflow/member",
            "workflow/run-end"
        ]
    );
    assert!(harness
        .parent
        .session()
        .events()
        .iter()
        .all(|event| event.validate().is_ok()));
}

struct CompleteEngine;

#[async_trait]
impl WorkflowEngine for CompleteEngine {
    async fn run(
        &self,
        _: WorkflowContext,
        _: WorkflowRequest,
        _: CancellationToken,
    ) -> Result<Value, TessivumError> {
        Ok(json!({"ok": true}))
    }
}

#[tokio::test]
async fn successful_workflow_only_cancels_its_owned_token() {
    let harness = setup().await;
    let workflow = WorkflowRuntime::new(
        harness.sessions,
        harness.service,
        Arc::new(CompleteEngine),
        1,
    )
    .unwrap();
    let caller = cancellation();
    let result = workflow
        .attach(harness.parent.clone())
        .unwrap()
        .run(
            WorkflowRequest {
                script: Value::Null,
                meta: Value::Null,
                args: Value::Null,
            },
            caller.clone(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, WorkflowRunStatus::Completed);
    assert!(!caller.is_cancelled());
}

#[tokio::test]
async fn disposed_agent_cannot_authorize_a_workflow_run() {
    let harness = setup().await;
    let workflow = WorkflowRuntime::new(
        harness.sessions,
        harness.service,
        Arc::new(CompleteEngine),
        1,
    )
    .unwrap();
    harness.parent.dispose().await.unwrap();
    assert!(matches!(
        workflow.attach(harness.parent),
        Err(WorkflowError::ParentRequired)
    ));
}

struct ParentCancellationEngine {
    started: Arc<Notify>,
}

#[async_trait]
impl WorkflowEngine for ParentCancellationEngine {
    async fn run(
        &self,
        context: WorkflowContext,
        _: WorkflowRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError> {
        context
            .start_agent(request("parent-cancel-child"))
            .await
            .unwrap();
        self.started.notify_one();
        cancellation.cancelled().await;
        Ok(Value::Null)
    }
}

#[tokio::test]
async fn parent_handle_disposal_cancels_and_joins_workflow_children() {
    let harness = setup().await;
    let started = Arc::new(Notify::new());
    let workflow = WorkflowRuntime::new(
        harness.sessions,
        harness.service,
        Arc::new(ParentCancellationEngine {
            started: started.clone(),
        }),
        1,
    )
    .unwrap();
    let parent = workflow.attach(harness.parent.clone()).unwrap();
    let running = tokio::spawn(async move {
        parent
            .run(
                WorkflowRequest {
                    script: Value::Null,
                    meta: Value::Null,
                    args: Value::Null,
                },
                cancellation(),
            )
            .await
            .unwrap()
    });
    started.notified().await;
    harness.parent.dispose().await.unwrap();
    assert_eq!(running.await.unwrap().status, WorkflowRunStatus::Cancelled);
}
struct DisposeCountingRuntime {
    disposals: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentRuntime for DisposeCountingRuntime {
    fn status(&self) -> AgentStatus {
        AgentStatus::Idle
    }
    async fn wake(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn when_idle(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn dispose(&self) -> Result<(), AgentError> {
        self.disposals.fetch_add(1, Ordering::AcqRel);
        Err(AgentError::Runtime("late disposal failed".into()))
    }
}

struct DisposeCountingFactory {
    disposals: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentFactory for DisposeCountingFactory {
    async fn create(
        &self,
        _: Arc<tessivum::session::Session>,
        _: AgentOptions,
        _: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        Ok(Arc::new(DisposeCountingRuntime {
            disposals: self.disposals.clone(),
        }))
    }
}

struct GatedStartPersistence {
    inner: MemorySessionPersistence,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl SessionPersistence for GatedStartPersistence {
    async fn create(
        &self,
        header: &SessionHeader,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.inner.create(header, cancellation).await
    }

    async fn append(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        if event.event_type == "subagent/contained-start" {
            self.started.notify_one();
            self.release.notified().await;
        }
        self.inner.append(session_id, event, cancellation).await
    }

    async fn load(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionHeader>, SessionError> {
        self.inner.load(session_id, cancellation).await
    }

    async fn inspect(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionInspection>, SessionError> {
        self.inner.inspect(session_id, cancellation).await
    }

    async fn read_from(
        &self,
        session_id: &SessionId,
        from_seq: u64,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.inner
            .read_from(session_id, from_seq, cancellation)
            .await
    }

    async fn flush(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.inner.flush(session_id, cancellation).await
    }

    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SessionError> {
        self.inner.list(cancellation).await
    }
}

struct LateAdmissionEngine {
    handle: Mutex<Option<oneshot::Sender<WorkflowRun>>>,
    result: Mutex<Option<oneshot::Sender<bool>>>,
}

#[async_trait]
impl WorkflowEngine for LateAdmissionEngine {
    async fn run(
        &self,
        context: WorkflowContext,
        _: WorkflowRequest,
        _: CancellationToken,
    ) -> Result<Value, TessivumError> {
        assert!(self
            .handle
            .lock()
            .take()
            .unwrap()
            .send(context.run())
            .is_ok());
        let start = request("late-child");
        let closed = matches!(
            context.start_agent(start).await,
            Err(WorkflowError::Closing)
        );
        self.result.lock().take().unwrap().send(closed).unwrap();
        Ok(Value::Null)
    }
}

#[tokio::test]
async fn workflow_dispose_waits_for_late_child_admission_to_quiesce() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let persistence = Arc::new(GatedStartPersistence {
        inner: MemorySessionPersistence::new(),
        started: started.clone(),
        release: release.clone(),
    });
    let child_disposals = Arc::new(AtomicUsize::new(0));
    let harness = setup_with(
        persistence,
        Arc::new(DisposeCountingFactory {
            disposals: child_disposals.clone(),
        }),
    )
    .await;
    let (handle_tx, handle_rx) = oneshot::channel();
    let (result_tx, result_rx) = oneshot::channel();
    let workflow = WorkflowRuntime::new(
        harness.sessions,
        harness.service,
        Arc::new(LateAdmissionEngine {
            handle: Mutex::new(Some(handle_tx)),
            result: Mutex::new(Some(result_tx)),
        }),
        1,
    )
    .unwrap();
    let parent = workflow.attach(harness.parent.clone()).unwrap();
    let running = tokio::spawn(async move {
        parent
            .run(
                WorkflowRequest {
                    script: Value::Null,
                    meta: Value::Null,
                    args: Value::Null,
                },
                cancellation(),
            )
            .await
            .unwrap()
    });
    let handle = handle_rx.await.unwrap();
    started.notified().await;
    let second_handle = handle.clone();
    let abandoned_handle = handle.clone();
    let disposing = tokio::spawn(async move { handle.dispose().await });
    let joining = tokio::spawn(async move { second_handle.dispose().await });
    let abandoned = tokio::spawn(async move { abandoned_handle.dispose().await });
    tokio::task::yield_now().await;
    abandoned.abort();
    assert!(!disposing.is_finished());
    assert!(!joining.is_finished());
    release.notify_one();
    assert!(result_rx.await.unwrap());
    let first = disposing.await.unwrap();
    let second = joining.await.unwrap();
    assert_eq!(first, second);
    assert_eq!(child_disposals.load(Ordering::Acquire), 1);
    assert_eq!(running.await.unwrap().status, WorkflowRunStatus::Error);
}

struct FailOneMember {
    inner: MemorySessionPersistence,
    failed: AtomicBool,
    run_end_attempts: AtomicUsize,
}

impl FailOneMember {
    fn new() -> Self {
        Self {
            inner: MemorySessionPersistence::new(),
            failed: AtomicBool::new(false),
            run_end_attempts: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SessionPersistence for FailOneMember {
    async fn create(
        &self,
        header: &SessionHeader,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.inner.create(header, cancellation).await
    }

    async fn append(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        if event.event_type == "workflow/member" && !self.failed.swap(true, Ordering::AcqRel) {
            return Err(SessionError::Cancelled);
        }
        if event.event_type == "workflow/run-end" {
            self.run_end_attempts.fetch_add(1, Ordering::AcqRel);
        }
        self.inner.append(session_id, event, cancellation).await
    }

    async fn load(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionHeader>, SessionError> {
        self.inner.load(session_id, cancellation).await
    }

    async fn inspect(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionInspection>, SessionError> {
        self.inner.inspect(session_id, cancellation).await
    }

    async fn read_from(
        &self,
        session_id: &SessionId,
        from_seq: u64,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.inner
            .read_from(session_id, from_seq, cancellation)
            .await
    }

    async fn flush(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.inner.flush(session_id, cancellation).await
    }

    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SessionError> {
        self.inner.list(cancellation).await
    }
}

struct MemberEngine;

#[async_trait]
impl WorkflowEngine for MemberEngine {
    async fn run(
        &self,
        context: WorkflowContext,
        _: WorkflowRequest,
        _: CancellationToken,
    ) -> Result<Value, TessivumError> {
        context.phase_start("member", Value::Null).await;
        Ok(Value::Null)
    }
}

#[tokio::test]
async fn workflow_attempts_run_end_after_member_write_failure() {
    let persistence = Arc::new(FailOneMember::new());
    let harness = setup_with(persistence.clone(), Arc::new(Factory)).await;
    let workflow =
        WorkflowRuntime::new(harness.sessions, harness.service, Arc::new(MemberEngine), 1).unwrap();
    let result = workflow
        .attach(harness.parent.clone())
        .unwrap()
        .run(
            WorkflowRequest {
                script: Value::Null,
                meta: Value::Null,
                args: Value::Null,
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, WorkflowRunStatus::Error);
    assert_eq!(persistence.run_end_attempts.load(Ordering::Acquire), 1);
    let events = harness.parent.session().events();
    assert_eq!(
        events
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        ["workflow/run", "workflow/run-end"]
    );
}

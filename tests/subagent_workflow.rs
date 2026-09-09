use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
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
        AgentCancelCause, AgentError, AgentFactory, AgentHandle, AgentOptions, AgentRegistry,
        AgentRuntime, AgentStatus, Inbox,
    },
    agent_mode::AgentModeId,
    persistence_jsonl::JsonlSessionPersistence,
    protocol::{
        ContentBlock, Message, SessionEvent, SessionHeader, SessionId, SessionOrigin, SurfaceOp,
        SESSION_FORMAT_VERSION,
    },
    session::{
        MemorySessionPersistence, RestoreMode, SessionError, SessionInspection, SessionPersistence,
        SessionStore,
    },
    subagent::{
        NativeSubagentProvider, SubagentDeleteRequest, SubagentError, SubagentHistoryRequest,
        SubagentInterruptRequest, SubagentMode, SubagentProvider, SubagentRunStatus,
        SubagentService, SubagentStartRequest, SubagentStatus, SubagentTools,
    },
    tools::{ToolRunContext, ToolRuntime},
    workflow::{
        NativeWorkflowEngine, WorkflowContext, WorkflowEngine, WorkflowError, WorkflowRequest,
        WorkflowRun, WorkflowRunStatus, WorkflowRuntime,
    },
    workspace::{WorkspaceError, WorkspaceRegistry},
    TessivumError, ToolCallId,
};
use tessivum_core::{CancellationToken, ContextHandle};
use tokio::sync::{oneshot, Notify};
use uuid::Uuid;

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
        agent_mode: None,
    }
}

fn options() -> AgentOptions {
    AgentOptions {
        provider: "fake".into(),
        model: "fake".into(),
        reasoning_effort: None,
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
        agent_mode: None,
        mode: SubagentMode::OneShot,
        capabilities: vec!["scout".into()],
        options: options(),
        created_at: 0,
        cwd: None,
        resume: false,
        initial_message: None,
    }
}
fn is_accepted_child_lifecycle_event(event: &SessionEvent) -> bool {
    matches!(
        event.event_type.as_str(),
        "subagent/contained-start" | "subagent/contained-end"
    ) || (event.event_type == "subagent/tree-creation"
        && event.data.get("accepted").and_then(Value::as_bool) == Some(true))
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

struct Running;

#[async_trait]
impl AgentRuntime for Running {
    fn status(&self) -> AgentStatus {
        AgentStatus::Running
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
struct LifecycleFactory;

#[async_trait]
impl AgentFactory for LifecycleFactory {
    async fn create(
        &self,
        session: Arc<tessivum::session::Session>,
        _: AgentOptions,
        _: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        if matches!(session.id().as_str(), "parent" | "a-running") {
            Ok(Arc::new(Running))
        } else {
            Ok(Arc::new(Idle))
        }
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
struct GatedNativeFactory {
    gated_id: Option<&'static str>,
    open: AtomicBool,
    starts: AtomicUsize,
    release: Notify,
}

#[async_trait]
impl AgentFactory for GatedNativeFactory {
    async fn create(
        &self,
        session: Arc<tessivum::session::Session>,
        _: AgentOptions,
        _: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        let gated = session.header().origin == Some(SessionOrigin::Subagent)
            && self.gated_id.map_or(true, |id| session.id().as_str() == id);
        let released = self.release.notified();
        if gated && !self.open.load(Ordering::Acquire) {
            self.starts.fetch_add(1, Ordering::AcqRel);
            released.await;
        }
        Ok(Arc::new(Idle))
    }
}

struct StatusRuntime(AgentStatus);

#[async_trait]
impl AgentRuntime for StatusRuntime {
    fn status(&self) -> AgentStatus {
        self.0
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

struct MixedFactory;

#[async_trait]
impl AgentFactory for MixedFactory {
    async fn create(
        &self,
        session: Arc<tessivum::session::Session>,
        _: AgentOptions,
        _: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        let status = if session.id().as_str() == "running-child" {
            AgentStatus::Running
        } else {
            AgentStatus::Idle
        };
        Ok(Arc::new(StatusRuntime(status)))
    }
}

struct DurablePromptRuntime {
    session: Arc<tessivum::session::Session>,
    inbox: Inbox,
}

#[async_trait]
impl AgentRuntime for DurablePromptRuntime {
    fn status(&self) -> AgentStatus {
        AgentStatus::Idle
    }
    async fn wake(&self) -> Result<(), AgentError> {
        if let Some(message) = self.inbox.take_next_turn() {
            self.session
                .append(
                    SessionEvent {
                        event_type: "user/message".into(),
                        seq: self.session.next_seq()?,
                        time: 0,
                        data: serde_json::to_value(message)
                            .map_err(|error| AgentError::Runtime(error.to_string()))?,
                        ignorable: None,
                        source_event_seqs: None,
                        surface_op: Some(SurfaceOp::Append),
                    },
                    cancellation(),
                )
                .await?;
        }
        Ok(())
    }
    async fn when_idle(&self) -> Result<(), AgentError> {
        Ok(())
    }
    async fn dispose(&self) -> Result<(), AgentError> {
        Ok(())
    }
}

struct DurablePromptFactory;

#[async_trait]
impl AgentFactory for DurablePromptFactory {
    async fn create(
        &self,
        session: Arc<tessivum::session::Session>,
        _: AgentOptions,
        inbox: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        Ok(Arc::new(DurablePromptRuntime { session, inbox }))
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

struct GatedProvider {
    native: NativeSubagentProvider,
    calls: AtomicUsize,
    open: AtomicBool,
    release: Notify,
}

#[async_trait]
impl SubagentProvider for GatedProvider {
    fn capabilities(&self) -> BTreeSet<String> {
        self.native.capabilities()
    }

    async fn start(
        &self,
        request: tessivum::subagent::ProviderStart,
        cancellation: CancellationToken,
    ) -> Result<AgentHandle, SubagentError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let released = self.release.notified();
        if !self.open.load(Ordering::Acquire) {
            tokio::select! {
                _ = released => {}
                _ = cancellation.cancelled() => return Err(SubagentError::CancelledBeforeAcceptance),
            }
        }
        self.native.start(request, cancellation).await
    }
}

struct Harness {
    service: SubagentService,
    provider: Arc<CountingProvider>,
    parent: Arc<AgentHandle>,
    agents: AgentRegistry,
    sessions: SessionStore,
    persistence: Arc<dyn SessionPersistence>,
}

async fn setup_with(
    persistence: Arc<dyn SessionPersistence>,
    factory: Arc<dyn AgentFactory>,
) -> Harness {
    setup_with_parent_header(persistence, factory, header("parent", None)).await
}

async fn setup_with_parent_header(
    persistence: Arc<dyn SessionPersistence>,
    factory: Arc<dyn AgentFactory>,
    parent_header: SessionHeader,
) -> Harness {
    let sessions = SessionStore::new(Arc::clone(&persistence));
    let agents = AgentRegistry::new(sessions.clone());
    std::mem::forget(agents.register_factory(factory).unwrap());
    let provider = Arc::new(CountingProvider {
        native: NativeSubagentProvider::new(agents.clone(), ["scout".into()]),
        calls: AtomicUsize::new(0),
    });
    let service = SubagentService::new(agents.clone(), sessions.clone(), Arc::clone(&persistence));
    std::mem::forget(service.register("native", provider.clone()).unwrap());
    let parent = Arc::new(
        agents
            .create(parent_header, options(), cancellation())
            .await
            .unwrap(),
    );
    Harness {
        service,
        provider,
        parent,
        agents,
        sessions,
        persistence,
    }
}

async fn setup() -> Harness {
    setup_with(Arc::new(MemorySessionPersistence::new()), Arc::new(Factory)).await
}

#[tokio::test]
async fn operator_catalog_sorts_status_and_deletes_only_inactive_leaves() {
    let root = TempDir::new("operator-delete");
    let persistence = Arc::new(JsonlSessionPersistence::new(root.path().join("data")));
    let harness = setup_with(persistence, Arc::new(LifecycleFactory)).await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();

    let (_, z) = parent
        .start(request("z-ready"), cancellation())
        .await
        .unwrap();
    z.run().await.unwrap();
    let (_, b) = parent
        .start(request("b-ready"), cancellation())
        .await
        .unwrap();
    let nested_parent = harness
        .service
        .attach(
            harness
                .agents
                .get(&SessionId::from("b-ready"))
                .unwrap()
                .into(),
        )
        .unwrap();
    let (_, grandchild) = nested_parent
        .start(request("grandchild"), cancellation())
        .await
        .unwrap();
    grandchild.run().await.unwrap();
    b.run().await.unwrap();
    let _ = parent
        .start(request("a-running"), cancellation())
        .await
        .unwrap();

    let _ = parent
        .start(request("c-idle"), cancellation())
        .await
        .unwrap();
    let _ = parent
        .start(request("a-idle"), cancellation())
        .await
        .unwrap();
    let entries = harness
        .service
        .list(SessionId::from("parent"), cancellation())
        .await
        .unwrap();
    let listed = entries
        .iter()
        .filter_map(|entry| match entry {
            tessivum::subagent::SubagentListEntry::Child { id, status, .. } => {
                Some((id.as_str(), *status))
            }
            tessivum::subagent::SubagentListEntry::Diagnostic { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        vec![
            ("a-running", SubagentStatus::Running),
            ("a-idle", SubagentStatus::Idle),
            ("c-idle", SubagentStatus::Idle),
            ("b-ready", SubagentStatus::Ready),
            ("z-ready", SubagentStatus::Ready),
        ]
    );

    assert!(matches!(
        harness
            .service
            .delete(
                SubagentDeleteRequest {
                    parent_session_id: SessionId::from("parent"),
                    child_session_id: SessionId::from("a-running"),
                },
                cancellation(),
            )
            .await,
        Err(SubagentError::DeleteActive)
    ));
    assert!(matches!(
        harness
            .service
            .delete(
                SubagentDeleteRequest {
                    parent_session_id: SessionId::from("parent"),
                    child_session_id: SessionId::from("a-idle"),
                },
                cancellation(),
            )
            .await,
        Err(SubagentError::DeleteActive)
    ));
    assert!(matches!(
        harness
            .service
            .delete(
                SubagentDeleteRequest {
                    parent_session_id: SessionId::from("foreign"),
                    child_session_id: SessionId::from("z-ready"),
                },
                cancellation(),
            )
            .await,
        Err(SubagentError::DirectParentMismatch)
    ));
    assert!(matches!(
        harness
            .service
            .delete(
                SubagentDeleteRequest {
                    parent_session_id: SessionId::from("parent"),
                    child_session_id: SessionId::from("parent"),
                },
                cancellation(),
            )
            .await,
        Err(SubagentError::DirectParentMismatch)
    ));
    assert!(matches!(
        harness
            .service
            .delete(
                SubagentDeleteRequest {
                    parent_session_id: SessionId::from("parent"),
                    child_session_id: SessionId::from("b-ready"),
                },
                cancellation(),
            )
            .await,
        Err(SubagentError::DeleteHasChildren)
    ));

    harness
        .service
        .delete(
            SubagentDeleteRequest {
                parent_session_id: SessionId::from("b-ready"),
                child_session_id: SessionId::from("grandchild"),
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert!(harness
        .persistence
        .inspect(&SessionId::from("grandchild"), cancellation())
        .await
        .unwrap()
        .is_none());
    assert!(
        harness
            .service
            .delete(
                SubagentDeleteRequest {
                    parent_session_id: SessionId::from("parent"),
                    child_session_id: SessionId::from("b-ready"),
                },
                cancellation(),
            )
            .await
            .unwrap()
            .deleted
    );
    assert!(harness
        .persistence
        .inspect(&SessionId::from("b-ready"), cancellation())
        .await
        .unwrap()
        .is_none());
    let remaining = harness
        .service
        .list(SessionId::from("parent"), cancellation())
        .await
        .unwrap();
    assert_eq!(
        remaining
            .iter()
            .filter_map(|entry| match entry {
                tessivum::subagent::SubagentListEntry::Child { id, .. } => {
                    Some(id.as_str())
                }
                tessivum::subagent::SubagentListEntry::Diagnostic { .. } => None,
            })
            .collect::<Vec<_>>(),
        vec!["a-running", "a-idle", "c-idle", "z-ready"]
    );
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tessivum-subagent-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct WorkspaceHarness {
    service: SubagentService,
    provider: Arc<CountingProvider>,
    parent: Arc<AgentHandle>,
    agents: AgentRegistry,
    sessions: SessionStore,
    persistence: Arc<dyn SessionPersistence>,
    registry: WorkspaceRegistry,
    workspace_id: String,
    workspace: PathBuf,
    root: TempDir,
}

async fn setup_workspace() -> WorkspaceHarness {
    let root = TempDir::new("workspace");
    let workspace = root.dir("workspace");
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let sessions = SessionStore::new(Arc::clone(&persistence));
    let agents = AgentRegistry::new(sessions.clone());
    std::mem::forget(agents.register_factory(Arc::new(Factory)).unwrap());
    let mut parent_header = header("parent", None);
    parent_header.cwd = Some(
        workspace
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    let parent = Arc::new(
        agents
            .create(parent_header, options(), cancellation())
            .await
            .unwrap(),
    );
    let registry = WorkspaceRegistry::open(
        root.path().join("data"),
        &workspace,
        persistence.list(cancellation()).await.unwrap(),
    )
    .unwrap();
    let workspace_id = registry
        .workspace_for_session(parent.id())
        .unwrap()
        .workspace_id
        .to_string();
    let provider = Arc::new(CountingProvider {
        native: NativeSubagentProvider::new(agents.clone(), ["scout".into()]),
        calls: AtomicUsize::new(0),
    });
    let service = SubagentService::new_with_workspace_registry(
        agents.clone(),
        sessions.clone(),
        Arc::clone(&persistence),
        registry.clone(),
    );
    std::mem::forget(service.register("native", provider.clone()).unwrap());
    WorkspaceHarness {
        service,
        provider,
        parent,
        agents,
        sessions,
        persistence,
        registry,
        workspace_id,
        workspace,
        root,
    }
}

#[tokio::test]
async fn direct_child_history_stays_cold_and_interrupt_preserves_fifo() {
    let harness = setup().await;
    let mut cold_header = header("cold-child", Some("parent"));
    cold_header.origin = Some(SessionOrigin::Subagent);
    harness
        .persistence
        .create(&cold_header, cancellation())
        .await
        .unwrap();
    harness
        .persistence
        .append(
            &cold_header.id,
            &SessionEvent {
                event_type: "turn/start".into(),
                seq: 0,
                time: 0,
                data: json!({"turn": 0}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let history = harness
        .service
        .history(
            SubagentHistoryRequest {
                parent_session_id: SessionId::from("parent"),
                child_session_id: cold_header.id.clone(),
                mode: SubagentMode::OneShot,
                before_seq: None,
                max_messages: Some(1),
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(history.events.len(), 1);
    assert_eq!(history.projections.unwrap().as_of_seq, 0);
    assert!(harness.sessions.get(&cold_header.id).is_none());
    assert!(harness.agents.get(&cold_header.id).is_none());
    assert!(matches!(
        harness
            .service
            .history(
                SubagentHistoryRequest {
                    parent_session_id: SessionId::from("forged-parent"),
                    child_session_id: cold_header.id,
                    mode: SubagentMode::OneShot,
                    before_seq: None,
                    max_messages: None,
                },
                cancellation(),
            )
            .await,
        Err(SubagentError::DirectParentMismatch)
    ));

    let mut live_header = header("live-child", Some("parent"));
    live_header.origin = Some(SessionOrigin::Subagent);
    let child = harness
        .agents
        .create(live_header, options(), cancellation())
        .await
        .unwrap();
    child.followup(message("queued")).await.unwrap();
    let interrupted = harness
        .service
        .interrupt(
            SubagentInterruptRequest {
                parent_session_id: SessionId::from("parent"),
                child_session_id: SessionId::from("live-child"),
                mode: SubagentMode::Continuable,
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert!(interrupted.accepted);
    assert_eq!(child.inbox().len(), 1);
    assert_eq!(
        child.cancel_options().unwrap().cause,
        AgentCancelCause::Parent
    );

    assert!(
        harness
            .service
            .interrupt(
                SubagentInterruptRequest {
                    parent_session_id: SessionId::from("parent"),
                    child_session_id: SessionId::from("unknown-child"),
                    mode: SubagentMode::Continuable,
                },
                cancellation(),
            )
            .await
            .unwrap()
            .accepted
    );
    assert!(matches!(
        harness
            .service
            .interrupt(
                SubagentInterruptRequest {
                    parent_session_id: SessionId::from("forged-parent"),
                    child_session_id: SessionId::from("live-child"),
                    mode: SubagentMode::Continuable,
                },
                cancellation(),
            )
            .await,
        Err(SubagentError::DirectParentMismatch)
    ));
    assert!(matches!(
        harness
            .service
            .interrupt(
                SubagentInterruptRequest {
                    parent_session_id: SessionId::from("parent"),
                    child_session_id: SessionId::from("live-child"),
                    mode: SubagentMode::OneShot,
                },
                cancellation(),
            )
            .await,
        Err(SubagentError::ContinuableRequired)
    ));
}

#[tokio::test]
async fn model_subagent_tools_register_and_enforce_parent_authority() {
    let harness = setup_with(
        Arc::new(MemorySessionPersistence::new()),
        Arc::new(MixedFactory),
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let mut running = request("running-child");
    running.mode = SubagentMode::Continuable;
    parent.start(running, cancellation()).await.unwrap();
    let running_child = Arc::new(
        harness
            .agents
            .get(&SessionId::from("running-child"))
            .unwrap(),
    );
    let child_parent = harness.service.attach(running_child.clone()).unwrap();
    let mut inactive = request("inactive-grandchild");
    inactive.mode = SubagentMode::Continuable;
    child_parent.start(inactive, cancellation()).await.unwrap();

    let tools = ToolRuntime::new();
    let _subagent_tools = SubagentTools::install(&tools, harness.service.clone()).unwrap();
    assert_eq!(
        tools
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>(),
        ["interrupt_agent", "list_agents", "send_message"]
    );
    let context = |session: &str, call: &str| ToolRunContext {
        session: SessionId::from(session),
        call: ToolCallId::from(call),
        cancellation: cancellation(),
    };
    let listed = tools
        .execute(
            context("parent", "list"),
            "list_agents",
            json!({"scope": "descendants"}),
        )
        .await;
    assert!(!listed.is_error);
    assert_eq!(
        listed.meta,
        json!([
            {
                "kind": "child",
                "id": "running-child",
                "label": "scout",
                "status": "running",
                "activity": "running",
                "mode": "continuable",
                "hasChildren": true,
                "parent": "parent",
                "depth": 1
            },
            {
                "kind": "child",
                "id": "inactive-grandchild",
                "label": "scout",
                "status": "idle",
                "activity": "inactive",
                "mode": "continuable",
                "hasChildren": false,
                "parent": "running-child",
                "depth": 2
            }
        ])
    );

    let _unrelated = harness
        .agents
        .create(header("unrelated", None), options(), cancellation())
        .await
        .unwrap();
    let unrelated_list = tools
        .execute(context("unrelated", "list"), "list_agents", json!({}))
        .await;
    assert!(!unrelated_list.is_error);
    assert_eq!(unrelated_list.meta, json!([]));
    let undelivered = tools
        .execute(
            context("unrelated", "send"),
            "send_message",
            json!({"subagent_id": "running-child", "message": "forged"}),
        )
        .await;
    assert!(undelivered.is_error);
    assert_eq!(undelivered.meta["code"], "SUBAGENT_PARENT_MISMATCH");
    let interrupted = tools
        .execute(
            context("unrelated", "interrupt"),
            "interrupt_agent",
            json!({"agent_id": "running-child"}),
        )
        .await;
    assert!(interrupted.is_error);
    assert_eq!(interrupted.meta["code"], "SUBAGENT_PARENT_MISMATCH");
    assert!(running_child.cancel_options().is_none());
}

#[tokio::test]
async fn continuable_prompt_returns_only_after_fifo_message_persists() {
    let harness = setup_with(
        Arc::new(MemorySessionPersistence::new()),
        Arc::new(DurablePromptFactory),
    )
    .await;
    let mut child_header = header("prompt-child", Some("parent"));
    child_header.origin = Some(SessionOrigin::Subagent);
    let child = harness
        .agents
        .create(child_header, options(), cancellation())
        .await
        .unwrap();
    let first = harness
        .service
        .prompt(
            tessivum::subagent::SubagentPromptRequest {
                parent_session_id: SessionId::from("parent"),
                child_session_id: SessionId::from("prompt-child"),
                mode: SubagentMode::Continuable,
                content: vec![ContentBlock::Text {
                    text: "first".into(),
                }],
                client_time_zone: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let second = harness
        .service
        .prompt(
            tessivum::subagent::SubagentPromptRequest {
                parent_session_id: SessionId::from("parent"),
                child_session_id: SessionId::from("prompt-child"),
                mode: SubagentMode::Continuable,
                content: vec![ContentBlock::Text {
                    text: "second".into(),
                }],
                client_time_zone: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert_ne!(first.message_id, second.message_id);
    let messages = child
        .session()
        .events()
        .into_iter()
        .filter(|event| event.event_type == "user/message")
        .map(|event| event.data["content"][0]["text"].clone())
        .collect::<Vec<_>>();
    assert_eq!(messages, vec![json!("first"), json!("second")]);

    assert!(
        harness
            .service
            .interrupt(
                SubagentInterruptRequest {
                    parent_session_id: SessionId::from("parent"),
                    child_session_id: SessionId::from("prompt-child"),
                    mode: SubagentMode::Continuable,
                },
                cancellation(),
            )
            .await
            .unwrap()
            .accepted
    );
    assert!(matches!(
        harness
            .service
            .prompt(
                tessivum::subagent::SubagentPromptRequest {
                    parent_session_id: SessionId::from("parent"),
                    child_session_id: SessionId::from("prompt-child"),
                    mode: SubagentMode::Continuable,
                    content: vec![ContentBlock::Text {
                        text: "resumed".into(),
                    }],
                    client_time_zone: None,
                },
                cancellation(),
            )
            .await,
        Err(SubagentError::AlreadyRun)
    ));
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
    assert!(!harness
        .parent
        .session()
        .events()
        .iter()
        .any(is_accepted_child_lifecycle_event));
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
    assert_eq!(harness.provider.calls.load(Ordering::Acquire), 1);
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
async fn nonworkspace_service_inherits_parent_mode_and_rejects_cwd_override() {
    let mut parent_header = header("parent", None);
    parent_header.cwd = Some("/parent-root".into());
    parent_header.agent_mode = Some(AgentModeId::minimal());
    let harness = setup_with_parent_header(
        Arc::new(MemorySessionPersistence::new()),
        Arc::new(Factory),
        parent_header,
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let (_, child) = parent
        .start(request("inherited-child"), cancellation())
        .await
        .unwrap();
    let inherited_header = harness
        .persistence
        .load(&SessionId::from("inherited-child"), cancellation())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inherited_header.cwd, Some("/parent-root".into()));
    assert_eq!(inherited_header.agent_mode, Some(AgentModeId::minimal()));
    child.dispose().await.unwrap();

    let mut explicit_request = request("explicit-mode-child");
    explicit_request.agent_mode = Some(AgentModeId::composition());
    let (_, explicit_child) = parent
        .start(explicit_request, cancellation())
        .await
        .unwrap();
    assert_eq!(
        harness
            .persistence
            .load(&SessionId::from("explicit-mode-child"), cancellation())
            .await
            .unwrap()
            .unwrap()
            .agent_mode,
        Some(AgentModeId::composition())
    );
    explicit_child.dispose().await.unwrap();

    let calls = harness.provider.calls.load(Ordering::Acquire);
    let mut override_request = request("override-child");
    override_request.cwd = Some("/other-root".into());
    assert!(matches!(
        parent.start(override_request, cancellation()).await,
        Err(SubagentError::CwdOverrideUnsupported)
    ));
    assert_eq!(harness.provider.calls.load(Ordering::Acquire), calls);
}

#[tokio::test]
async fn workspace_children_inherit_and_resume_after_restart() {
    let harness = setup_workspace().await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let (_, child) = parent
        .start(request("workspace-child"), cancellation())
        .await
        .unwrap();
    let expected_cwd = Some(
        harness
            .workspace
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    assert_eq!(
        harness
            .persistence
            .load(&SessionId::from("workspace-child"), cancellation())
            .await
            .unwrap()
            .unwrap()
            .cwd,
        expected_cwd.clone()
    );
    assert_eq!(
        harness
            .registry
            .workspace_for_session("workspace-child")
            .unwrap()
            .workspace_id
            .to_string(),
        harness.workspace_id
    );
    child.dispose().await.unwrap();
    harness.parent.dispose().await.unwrap();
    harness.registry.shutdown();

    let registry = WorkspaceRegistry::open(
        harness.root.path().join("data"),
        &harness.workspace,
        harness.persistence.list(cancellation()).await.unwrap(),
    )
    .unwrap();
    let sessions = SessionStore::new(harness.persistence.clone());
    let agents = AgentRegistry::new(sessions.clone());
    std::mem::forget(agents.register_factory(Arc::new(Factory)).unwrap());
    let provider = Arc::new(CountingProvider {
        native: NativeSubagentProvider::new(agents.clone(), ["scout".into()]),
        calls: AtomicUsize::new(0),
    });
    let service = SubagentService::new_with_workspace_registry(
        agents.clone(),
        sessions,
        harness.persistence.clone(),
        registry.clone(),
    );
    std::mem::forget(service.register("native", provider).unwrap());
    let resumed_parent = Arc::new(
        agents
            .resume(SessionId::from("parent"), options(), cancellation())
            .await
            .unwrap(),
    );
    let parent = service.attach(resumed_parent).unwrap();
    let mut resumed = request("workspace-child");
    resumed.resume = true;
    let (_, resumed_child) = parent.start(resumed, cancellation()).await.unwrap();
    assert_eq!(
        harness
            .persistence
            .load(&SessionId::from("workspace-child"), cancellation())
            .await
            .unwrap()
            .unwrap()
            .cwd,
        expected_cwd
    );
    resumed_child.dispose().await.unwrap();
    assert_eq!(
        registry
            .workspace_for_session("workspace-child")
            .unwrap()
            .workspace_id
            .to_string(),
        harness.workspace_id
    );
}

#[tokio::test]
async fn workspace_resume_rejects_foreign_and_removed_parent_workspaces() {
    let harness = setup_workspace().await;
    let wrong_cwd_child = harness
        .agents
        .create(
            SessionHeader {
                version: SESSION_FORMAT_VERSION,
                id: SessionId::from("wrong-cwd-child"),
                created_at: 0,
                cwd: Some("/wrong-root".into()),
                parent_session: Some(SessionId::from("parent")),
                seed_length: None,
                origin: Some(SessionOrigin::Subagent),
                delegation_depth: None,
                agent_mode: Some(AgentModeId::standard()),
            },
            options(),
            cancellation(),
        )
        .await
        .unwrap();
    harness
        .registry
        .recognize_session("wrong-cwd-child")
        .unwrap();
    harness
        .registry
        .attach_session(&harness.workspace_id, "wrong-cwd-child", None)
        .unwrap();
    wrong_cwd_child.dispose().await.unwrap();
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let mut resume = request("wrong-cwd-child");
    resume.resume = true;
    assert!(matches!(
        parent.start(resume, cancellation()).await,
        Err(SubagentError::ResumeWorkspaceMismatch)
    ));
    assert_eq!(harness.provider.calls.load(Ordering::Acquire), 0);

    let foreign = harness.root.dir("foreign");
    let foreign_id = harness
        .registry
        .create(&foreign, None)
        .unwrap()
        .workspace
        .workspace_id;
    let child_header = SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: SessionId::from("foreign-child"),
        created_at: 0,
        cwd: Some(
            harness
                .workspace
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        ),
        parent_session: Some(SessionId::from("parent")),
        seed_length: None,
        origin: Some(SessionOrigin::Subagent),
        delegation_depth: None,
        agent_mode: Some(AgentModeId::standard()),
    };
    let foreign_child = harness
        .agents
        .create(child_header, options(), cancellation())
        .await
        .unwrap();
    harness.registry.recognize_session("foreign-child").unwrap();
    harness
        .registry
        .attach_session(&foreign_id, "foreign-child", None)
        .unwrap();
    foreign_child.dispose().await.unwrap();
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let mut resume = request("foreign-child");
    resume.resume = true;
    assert!(matches!(
        parent.start(resume, cancellation()).await,
        Err(SubagentError::ResumeWorkspaceMismatch)
    ));
    assert_eq!(harness.provider.calls.load(Ordering::Acquire), 0);

    let removed = setup_workspace().await;
    let parent = removed.service.attach(removed.parent.clone()).unwrap();
    removed
        .registry
        .delete(&removed.workspace_id, None)
        .unwrap();
    assert!(matches!(
        parent.start(request("removed-child"), cancellation()).await,
        Err(SubagentError::Workspace(_))
    ));
    assert_eq!(removed.provider.calls.load(Ordering::Acquire), 0);
}

struct DeleteWorkspaceProvider {
    native: NativeSubagentProvider,
    registry: WorkspaceRegistry,
    workspace_id: String,
    calls: AtomicUsize,
}

#[async_trait]
impl SubagentProvider for DeleteWorkspaceProvider {
    fn capabilities(&self) -> BTreeSet<String> {
        self.native.capabilities()
    }

    async fn start(
        &self,
        request: tessivum::subagent::ProviderStart,
        cancellation: CancellationToken,
    ) -> Result<AgentHandle, SubagentError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let agent = self.native.start(request, cancellation).await?;
        self.registry.delete(&self.workspace_id, None).unwrap();
        Ok(agent)
    }
}

#[tokio::test]
async fn workspace_attach_failure_disposes_and_leaves_child_for_repair() {
    let harness = setup_workspace().await;
    let provider = Arc::new(DeleteWorkspaceProvider {
        native: NativeSubagentProvider::new(harness.agents.clone(), ["scout".into()]),
        registry: harness.registry.clone(),
        workspace_id: harness.workspace_id.clone(),
        calls: AtomicUsize::new(0),
    });
    let service = SubagentService::new_with_workspace_registry(
        harness.agents.clone(),
        harness.sessions.clone(),
        harness.persistence.clone(),
        harness.registry.clone(),
    );
    std::mem::forget(service.register("deleting", provider.clone()).unwrap());
    let parent = service.attach(harness.parent.clone()).unwrap();
    let mut request = request("repair-child");
    request.provider = "deleting".into();
    assert!(matches!(
        parent.start(request, cancellation()).await,
        Err(SubagentError::Workspace(WorkspaceError::StaleLease))
    ));
    assert_eq!(provider.calls.load(Ordering::Acquire), 1);
    assert!(harness
        .agents
        .get(&SessionId::from("repair-child"))
        .is_none());
    assert_eq!(
        harness
            .persistence
            .load(&SessionId::from("repair-child"), cancellation())
            .await
            .unwrap()
            .unwrap()
            .cwd,
        Some(
            harness
                .workspace
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        )
    );
    assert!(harness
        .registry
        .workspace_for_session("repair-child")
        .is_none());
    assert!(!harness
        .parent
        .session()
        .events()
        .iter()
        .any(is_accepted_child_lifecycle_event));

    let replacement = harness
        .registry
        .create(&harness.workspace, None)
        .unwrap()
        .workspace
        .workspace_id;
    harness.registry.recognize_session("repair-child").unwrap();
    harness
        .registry
        .attach_session(&replacement, "repair-child", None)
        .unwrap();
    assert_eq!(
        harness
            .registry
            .workspace_for_session("repair-child")
            .unwrap()
            .workspace_id,
        replacement
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
    assert!(harness
        .agents
        .get(&SessionId::from("delivery-fails"))
        .is_none());
    assert!(!harness
        .parent
        .session()
        .events()
        .iter()
        .any(is_accepted_child_lifecycle_event));
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
                meta: json!({"name": "failing-workflow"}),
                args: Value::Null,
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, WorkflowRunStatus::Error);
    let events = harness.parent.session().events();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        ["tool-workflow/run-start", "tool-workflow/run-end"]
    );
    assert_eq!(
        events[0].data,
        json!({"runId": result.run_id.as_str(), "name": "failing-workflow"})
    );
    assert_eq!(
        events[1].data,
        json!({"runId": result.run_id.as_str(), "stopReason": "error"})
    );
    assert!(events
        .iter()
        .all(|event| event.ignorable.is_none() && event.validate().is_ok()));
}

struct RecordingEngine;

#[async_trait]
impl WorkflowEngine for RecordingEngine {
    async fn run(
        &self,
        context: WorkflowContext,
        _: WorkflowRequest,
        _: CancellationToken,
    ) -> Result<Value, TessivumError> {
        context.phase_start("Research", Value::Null).await;
        let activation = context
            .start_agent(request("workflow-child"))
            .await
            .unwrap();
        let child_result = activation.run().await.unwrap();
        context.end_agent(&activation, &child_result).await;
        context.phase_end("Research", Value::Null).await;
        Ok(json!({"answer": "complete"}))
    }
}

#[tokio::test]
async fn workflow_records_canonical_member_lifecycle_that_reloads() {
    let harness = setup().await;
    let workflow = WorkflowRuntime::new(
        harness.sessions,
        harness.service,
        Arc::new(RecordingEngine),
        1,
    )
    .unwrap();
    let result = workflow
        .attach(harness.parent.clone())
        .unwrap()
        .run(
            WorkflowRequest {
                script: Value::Null,
                meta: json!({"name": "research"}),
                args: Value::Null,
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, WorkflowRunStatus::Completed);
    assert_eq!(result.value, Some(json!({"answer": "complete"})));

    let events = harness.parent.session().events();
    let records = events
        .iter()
        .filter(|event| event.event_type.starts_with("tool-workflow/"))
        .collect::<Vec<_>>();
    assert_eq!(
        records
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        [
            "tool-workflow/run-start",
            "tool-workflow/agent-start",
            "tool-workflow/agent-end",
            "tool-workflow/run-end",
        ]
    );
    assert_eq!(
        records[0].data,
        json!({"runId": result.run_id.as_str(), "name": "research"})
    );
    assert_eq!(
        records[1].data,
        json!({
            "runId": result.run_id.as_str(),
            "seq": 1,
            "label": "scout",
            "phase": "Research",
            "childId": "workflow-child",
        })
    );
    assert_eq!(
        records[2].data,
        json!({"runId": result.run_id.as_str(), "seq": 1, "outcome": "completed"})
    );
    assert_eq!(
        records[3].data,
        json!({"runId": result.run_id.as_str(), "stopReason": "completed"})
    );
    assert!(records
        .iter()
        .all(|event| event.ignorable.is_none() && event.validate().is_ok()));

    let reloaded = SessionStore::new(harness.persistence.clone())
        .restore(
            &SessionId::from("parent"),
            RestoreMode::Metadata,
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(reloaded.events(), events);
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
async fn workflow_rejects_missing_canonical_name_before_recording() {
    let harness = setup().await;
    let workflow = WorkflowRuntime::new(
        harness.sessions,
        harness.service,
        Arc::new(CompleteEngine),
        1,
    )
    .unwrap();
    assert!(matches!(
        workflow
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
            .await,
        Err(WorkflowError::InvalidWorkflowName)
    ));
    assert!(harness.parent.session().events().is_empty());
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
                meta: json!({"name": "workflow"}),
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
                    meta: json!({"name": "workflow"}),
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
        Ok(())
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
    event_type: &'static str,
    child_id: Option<&'static str>,
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

    async fn create_seeded(
        &self,
        header: &SessionHeader,
        events: &[SessionEvent],
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.inner.create_seeded(header, events, cancellation).await
    }

    async fn append(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        if event.event_type == self.event_type
            && self.child_id.is_none_or(|child_id| {
                event
                    .data
                    .get("child")
                    .and_then(|child| child.get("childSessionId"))
                    .and_then(Value::as_str)
                    == Some(child_id)
            })
        {
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

#[tokio::test]
async fn child_admission_sequences_behind_a_persisting_root_write() {
    let write_started = Arc::new(Notify::new());
    let release_write = Arc::new(Notify::new());
    let persistence = Arc::new(GatedStartPersistence {
        inner: MemorySessionPersistence::new(),
        event_type: "test/root-write",
        child_id: None,
        started: write_started.clone(),
        release: release_write.clone(),
    });
    let harness = setup_with(persistence, Arc::new(Factory)).await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let root = harness.parent.session();
    let root_write = tokio::spawn({
        let root = root.clone();
        async move {
            root.append(
                SessionEvent {
                    event_type: "test/root-write".into(),
                    seq: root.next_seq().unwrap(),
                    time: 0,
                    data: Value::Null,
                    ignorable: Some(true),
                    source_event_seqs: None,
                    surface_op: None,
                },
                cancellation(),
            )
            .await
        }
    });
    write_started.notified().await;

    let mut starting = Box::pin(parent.start(request("sequenced-child"), cancellation()));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut starting)
            .await
            .is_err()
    );
    release_write.notify_one();

    root_write.await.unwrap().unwrap();
    let (_, child) = starting.await.unwrap();
    let events = root.events();
    assert!(events
        .iter()
        .enumerate()
        .all(|(seq, event)| event.seq == seq as u64));
    assert_eq!(
        events
            .iter()
            .filter(|event| is_accepted_child_lifecycle_event(event))
            .count(),
        2
    );

    child.dispose().await.unwrap();
    parent.dispose().await;
    harness.parent.dispose().await.unwrap();
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
        event_type: "subagent/contained-start",
        child_id: Some("late-child"),
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
                    meta: json!({"name": "workflow"}),
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
    assert_eq!(running.await.unwrap().status, WorkflowRunStatus::Cancelled);
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

    async fn create_seeded(
        &self,
        header: &SessionHeader,
        events: &[SessionEvent],
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.inner.create_seeded(header, events, cancellation).await
    }

    async fn append(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        if event.event_type == "tool-workflow/agent-start"
            && !self.failed.swap(true, Ordering::AcqRel)
        {
            return Err(SessionError::Cancelled);
        }
        if event.event_type == "tool-workflow/run-end" {
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
        let activation = context.start_agent(request("member-child")).await.unwrap();
        let result = activation.run().await.unwrap();
        context.end_agent(&activation, &result).await;
        Ok(Value::Null)
    }
}

#[tokio::test]
async fn workflow_stops_recording_after_member_write_failure() {
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
                meta: json!({"name": "workflow"}),
                args: Value::Null,
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, WorkflowRunStatus::Completed);
    assert_eq!(persistence.run_end_attempts.load(Ordering::Acquire), 0);
    assert_eq!(
        harness
            .parent
            .session()
            .events()
            .into_iter()
            .filter(|event| event.event_type.starts_with("tool-workflow/"))
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        ["tool-workflow/run-start"]
    );
}

#[tokio::test]
async fn native_workflow_rejects_unsupported_scripts_before_starting_children() {
    let harness = setup().await;
    let engine = NativeWorkflowEngine::from_recording(Some(
        r#"{"type":"tool/call","data":{"callId":"workflow-call","name":"workflow"}}
{"type":"tool/result","data":{"message":{"source":{"kind":"tool","callId":"workflow-call"},"content":[{"type":"tool-result","toolCallId":"workflow-call","content":[{"type":"text","text":"workflow \"recorded\" completed (1 agent).\nReturn value:\n{\"reply\":\"DURABLE_REPLY\"}"}],"isError":false}]}}}"#,
    ))
    .unwrap();
    let workflow = WorkflowRuntime::new(
        harness.sessions.clone(),
        harness.service.clone(),
        Arc::new(engine),
        2,
    )
    .unwrap();
    for script in [
        "// const reply = await agent('must not run')\nphase('Run')\nconst reply = await agent('also must not run')\nreturn { reply }",
        "if (false) { const reply = await agent('must not run') }\nreturn { reply }",
        "phase('Run')\nconst reply = await agent(args.prompt)\nreturn { reply }",
        "phase('Run')\nconst reply = await agent('must not run')\nconst unused = await agent('must not run')\nreturn { reply }",
        "phase('Run)\nconst reply = await agent('must not run')\nreturn { reply }",
    ] {
        let result = workflow
            .attach(harness.parent.clone())
            .unwrap()
            .run(
                WorkflowRequest {
                    script: json!(script),
                    meta: json!({"name": "rejected"}),
                    args: Value::Null,
                },
                cancellation(),
            )
            .await
            .unwrap();
        assert_eq!(result.status, WorkflowRunStatus::Error);
    }
    assert_eq!(harness.provider.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn native_workflow_replays_durable_result_bindings_exactly() {
    let harness = setup().await;
    let engine = NativeWorkflowEngine::from_recording(Some(
        r#"{"type":"tool/call","data":{"callId":"workflow-call","name":"workflow"}}
{"type":"tool/result","data":{"message":{"source":{"kind":"tool","callId":"workflow-call"},"content":[{"type":"tool-result","toolCallId":"workflow-call","content":[{"type":"text","text":"workflow \"recorded\" completed (1 agent).\nReturn value:\n{\"reply\":\"DURABLE_REPLY\"}"}],"isError":false}]}}}"#,
    ))
    .unwrap();
    let workflow =
        WorkflowRuntime::new(harness.sessions, harness.service, Arc::new(engine), 1).unwrap();
    let result = workflow
        .attach(harness.parent)
        .unwrap()
        .run(
            WorkflowRequest {
                script: json!(
                    "phase('Run')\nconst reply = await agent('Reply with exactly the word PROMPT_WORD and nothing else.')\nreturn { reply }"
                ),
                meta: json!({"name": "recorded"}),
                args: Value::Null,
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, WorkflowRunStatus::Completed);
    assert_eq!(result.value, Some(json!({"reply": "DURABLE_REPLY"})));
    assert_eq!(harness.provider.calls.load(Ordering::Acquire), 1);
}

#[test]
fn native_workflow_rejects_incomplete_recordings() {
    assert!(NativeWorkflowEngine::from_recording(Some(
        r#"{"type":"tool/call","data":{"callId":"workflow-call","name":"workflow"}}"#,
    ))
    .is_err());
}

#[tokio::test]
async fn recursive_start_rejects_depth_five_before_provider_side_effects() {
    let harness = setup().await;
    let mut parent_agent = harness.parent.clone();
    let mut parents = Vec::new();
    let mut activations = Vec::new();
    for depth in 1..=4 {
        let parent = harness.service.attach(parent_agent).unwrap();
        let child_id = format!("depth-{depth}");
        let (_, activation) = parent
            .start(request(&child_id), cancellation())
            .await
            .unwrap();
        parent_agent = Arc::new(harness.agents.get(&SessionId::from(child_id)).unwrap());
        parents.push(parent);
        activations.push(activation);
    }

    let deepest = harness.service.attach(parent_agent).unwrap();
    let calls = harness.provider.calls.load(Ordering::Acquire);
    assert!(matches!(
        deepest.start(request("depth-5"), cancellation()).await,
        Err(SubagentError::DelegationDepthLimit { limit: 4 })
    ));
    assert_eq!(harness.provider.calls.load(Ordering::Acquire), calls);

    harness.parent.dispose().await.unwrap();
    drop((parents, activations));
}

#[tokio::test]
async fn concurrent_tree_admissions_fail_fast_and_recover_dropped_or_disposed_slots() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let sessions = SessionStore::new(Arc::clone(&persistence));
    let agents = AgentRegistry::new(sessions.clone());
    std::mem::forget(agents.register_factory(Arc::new(Factory)).unwrap());
    let provider = Arc::new(GatedProvider {
        native: NativeSubagentProvider::new(agents.clone(), ["scout".into()]),
        calls: AtomicUsize::new(0),
        open: AtomicBool::new(false),
        release: Notify::new(),
    });
    let service = SubagentService::new(agents.clone(), sessions.clone(), Arc::clone(&persistence));
    std::mem::forget(service.register("native", provider.clone()).unwrap());
    let second_service = SubagentService::new(agents.clone(), sessions, persistence);
    std::mem::forget(second_service.register("native", provider.clone()).unwrap());
    let parent_agent = Arc::new(
        agents
            .create(header("concurrent-parent", None), options(), cancellation())
            .await
            .unwrap(),
    );
    let parent = service.attach(parent_agent.clone()).unwrap();
    let second_parent = second_service.attach(parent_agent.clone()).unwrap();
    let mut starts = Vec::new();
    for index in 0..16 {
        let parent = if index % 2 == 0 {
            parent.clone()
        } else {
            second_parent.clone()
        };
        starts.push(tokio::spawn(async move {
            parent
                .start(request(&format!("concurrent-{index}")), cancellation())
                .await
        }));
    }
    while provider.calls.load(Ordering::Acquire) != 16 {
        tokio::task::yield_now().await;
    }

    assert!(matches!(
        parent
            .start(request("concurrent-overflow"), cancellation())
            .await,
        Err(SubagentError::TreeConcurrencyLimit { limit: 16 })
    ));
    assert_eq!(provider.calls.load(Ordering::Acquire), 16);
    let aborted = starts.pop().unwrap();
    aborted.abort();
    assert!(matches!(aborted.await, Err(error) if error.is_cancelled()));
    let recovered_parent = parent.clone();
    starts.push(tokio::spawn(async move {
        recovered_parent
            .start(request("concurrent-recovered-pending"), cancellation())
            .await
    }));
    while provider.calls.load(Ordering::Acquire) != 17 {
        tokio::task::yield_now().await;
    }
    provider.open.store(true, Ordering::Release);
    provider.release.notify_waiters();

    let mut activations = Vec::new();
    for start in starts {
        activations.push(start.await.unwrap().unwrap().1);
    }
    activations.pop().unwrap().dispose().await.unwrap();
    let (_, replacement) = parent
        .start(request("concurrent-replacement"), cancellation())
        .await
        .unwrap();
    assert_eq!(provider.calls.load(Ordering::Acquire), 18);
    replacement.dispose().await.unwrap();
    parent.dispose().await;
    second_parent.dispose().await;
    parent_agent.dispose().await.unwrap();
}

#[tokio::test]
async fn cumulative_tree_quota_survives_disposal_and_reattached_capabilities() {
    let harness = setup().await;
    let first = harness.service.attach(harness.parent.clone()).unwrap();
    let second_service = SubagentService::new(
        harness.agents.clone(),
        harness.sessions.clone(),
        harness.persistence.clone(),
    );
    std::mem::forget(
        second_service
            .register("native", harness.provider.clone())
            .unwrap(),
    );
    let second = second_service.attach(harness.parent.clone()).unwrap();
    for index in 0..128 {
        let parent = if index % 2 == 0 { &first } else { &second };
        let (_, activation) = parent
            .start(request(&format!("quota-{index}")), cancellation())
            .await
            .unwrap();
        activation.dispose().await.unwrap();
        if index == 0 {
            assert!(parent
                .start(request("quota-0"), cancellation())
                .await
                .is_err());
        }
    }

    let resumed_service = SubagentService::new(
        harness.agents.clone(),
        harness.sessions.clone(),
        harness.persistence.clone(),
    );
    std::mem::forget(
        resumed_service
            .register("native", harness.provider.clone())
            .unwrap(),
    );
    let resumed = resumed_service.attach(harness.parent.clone()).unwrap();
    assert!(matches!(
        resumed
            .start(request("quota-overflow"), cancellation())
            .await,
        Err(SubagentError::TreeCreationLimit { limit: 128 })
    ));
    assert_eq!(harness.provider.calls.load(Ordering::Acquire), 129);
    harness.parent.dispose().await.unwrap();
}

#[tokio::test]
async fn dropped_start_disposes_the_provider_agent_and_rolls_back_admission() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let persistence = Arc::new(GatedStartPersistence {
        inner: MemorySessionPersistence::new(),
        event_type: "subagent/contained-start",
        child_id: Some("dropped-start"),
        started: started.clone(),
        release,
    });
    let disposals = Arc::new(AtomicUsize::new(0));
    let harness = setup_with(
        persistence,
        Arc::new(DisposeCountingFactory {
            disposals: disposals.clone(),
        }),
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let start_parent = parent.clone();
    let starting = tokio::spawn(async move {
        start_parent
            .start(request("dropped-start"), cancellation())
            .await
    });
    started.notified().await;
    starting.abort();
    while disposals.load(Ordering::Acquire) == 0 {
        tokio::task::yield_now().await;
    }

    assert!(harness
        .agents
        .get(&SessionId::from("dropped-start"))
        .is_none());
    parent.dispose().await;
    harness.parent.dispose().await.unwrap();
}

#[tokio::test]
async fn dropped_foreground_wait_disposes_only_its_activation_and_background_sibling_survives() {
    let idle_started = Arc::new(Notify::new());
    let release_idle = Arc::new(Notify::new());
    let harness = setup_with(
        Arc::new(MemorySessionPersistence::new()),
        Arc::new(BlockingIdleFactory {
            idle_started: idle_started.clone(),
            release_idle,
        }),
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let (_, foreground) = parent
        .start(request("dropped-foreground"), cancellation())
        .await
        .unwrap();
    let (_, background) = parent
        .start(request("background-sibling"), cancellation())
        .await
        .unwrap();
    let waiting = tokio::spawn({
        let foreground = foreground.clone();
        async move { foreground.wait_for_idle().await }
    });
    idle_started.notified().await;
    waiting.abort();
    while harness
        .agents
        .get(&SessionId::from("dropped-foreground"))
        .is_some()
    {
        tokio::task::yield_now().await;
    }

    assert!(harness
        .agents
        .get(&SessionId::from("background-sibling"))
        .is_some_and(|agent| !agent.is_disposed()));
    background.dispose().await.unwrap();
    assert!(harness
        .agents
        .get(&SessionId::from("background-sibling"))
        .is_none());
    parent.dispose().await;
    harness.parent.dispose().await.unwrap();
}

struct FailOnceDisposeRuntime {
    fail_once: bool,
    attempts: Arc<AtomicUsize>,
    first_failure_gate: Option<(Arc<Notify>, Arc<Notify>)>,
}

#[async_trait]
impl AgentRuntime for FailOnceDisposeRuntime {
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
        if self.fail_once {
            if self.attempts.load(Ordering::Acquire) == 0 {
                if let Some((started, release)) = &self.first_failure_gate {
                    started.notify_one();
                    release.notified().await;
                }
            }
            if self.attempts.fetch_add(1, Ordering::AcqRel) == 0 {
                return Err(AgentError::Runtime("first disposal failed".into()));
            }
        }
        Ok(())
    }
}

struct FailOnceDisposeFactory {
    attempts: Arc<AtomicUsize>,
    first_failure_gate: Option<(Arc<Notify>, Arc<Notify>)>,
}

#[async_trait]
impl AgentFactory for FailOnceDisposeFactory {
    async fn create(
        &self,
        session: Arc<tessivum::session::Session>,
        _: AgentOptions,
        _: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        Ok(Arc::new(FailOnceDisposeRuntime {
            fail_once: session.id().as_str() != "parent",
            attempts: self.attempts.clone(),
            first_failure_gate: self.first_failure_gate.clone(),
        }))
    }
}

#[tokio::test]
async fn disposal_failure_is_observable_and_retry_keeps_the_tree_slot_until_success() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let harness = setup_with(
        Arc::new(MemorySessionPersistence::new()),
        Arc::new(FailOnceDisposeFactory {
            attempts: attempts.clone(),
            first_failure_gate: None,
        }),
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let (_, child) = parent
        .start(request("retry-dispose"), cancellation())
        .await
        .unwrap();

    let failed = child.dispose().await.unwrap();
    assert_eq!(failed.status, SubagentRunStatus::Error);
    assert_eq!(failed.error.unwrap().code, "AGENT_DISPOSE_FAILED");
    assert!(harness
        .agents
        .get(&SessionId::from("retry-dispose"))
        .is_some());

    let retried = child.dispose().await.unwrap();
    assert_eq!(retried.status, SubagentRunStatus::Cancelled);
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    assert!(harness
        .agents
        .get(&SessionId::from("retry-dispose"))
        .is_none());
    parent.dispose().await;
    harness.parent.dispose().await.unwrap();
}

#[tokio::test]
async fn dropped_child_cleanup_retries_without_erasing_its_first_failure() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let failure_started = Arc::new(Notify::new());
    let release_failure = Arc::new(Notify::new());
    let harness = setup_with(
        Arc::new(MemorySessionPersistence::new()),
        Arc::new(FailOnceDisposeFactory {
            attempts: attempts.clone(),
            first_failure_gate: Some((failure_started.clone(), release_failure.clone())),
        }),
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let child = parent
        .start(request("dropped-retry-dispose"), cancellation())
        .await
        .unwrap()
        .1;

    let dropped_child = child.clone();
    let disposing = tokio::spawn(async move { dropped_child.dispose().await });
    failure_started.notified().await;
    disposing.abort();
    assert!(matches!(disposing.await, Err(error) if error.is_cancelled()));
    failure_started.notified().await;
    let mut observing = Box::pin(child.dispose());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut observing)
            .await
            .is_err()
    );
    release_failure.notify_one();

    let failure = observing.await.unwrap();
    assert_eq!(failure.status, SubagentRunStatus::Error);
    assert_eq!(failure.error.unwrap().code, "AGENT_DISPOSE_FAILED");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while harness
            .agents
            .get(&SessionId::from("dropped-retry-dispose"))
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("drop-owned cleanup retries without another disposal call");
    assert_eq!(
        child.dispose().await.unwrap().status,
        SubagentRunStatus::Cancelled
    );
    assert_eq!(attempts.load(Ordering::Acquire), 2);
    parent.dispose().await;
    harness.parent.dispose().await.unwrap();
}

#[tokio::test]
async fn parent_cleanup_waiters_share_failure_and_second_dispose_retries_retained_child() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let failure_started = Arc::new(Notify::new());
    let release_failure = Arc::new(Notify::new());
    let harness = setup_with(
        Arc::new(MemorySessionPersistence::new()),
        Arc::new(FailOnceDisposeFactory {
            attempts: attempts.clone(),
            first_failure_gate: Some((failure_started.clone(), release_failure.clone())),
        }),
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    parent
        .start(request("parent-retry-dispose"), cancellation())
        .await
        .unwrap();

    let first_parent = parent.clone();
    let first = tokio::spawn(async move { first_parent.dispose().await });
    failure_started.notified().await;
    let second_parent = parent.clone();
    let (joining_tx, joining_rx) = oneshot::channel();
    let second = tokio::spawn(async move {
        joining_tx.send(()).unwrap();
        second_parent.dispose().await
    });
    joining_rx.await.unwrap();
    assert!(!second.is_finished());
    first.abort();
    assert!(matches!(first.await, Err(error) if error.is_cancelled()));
    release_failure.notify_one();

    let failed = second.await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].status, SubagentRunStatus::Error);
    assert_eq!(
        failed[0].error.as_ref().unwrap().code,
        "AGENT_DISPOSE_FAILED"
    );
    assert!(harness
        .agents
        .get(&SessionId::from("parent-retry-dispose"))
        .is_some());
    assert_eq!(attempts.load(Ordering::Acquire), 1);

    let retry_parent = harness.service.attach(harness.parent.clone()).unwrap();
    let mut peers = Vec::new();
    for index in 0..15 {
        peers.push(
            retry_parent
                .start(
                    request(&format!("parent-retry-peer-{index}")),
                    cancellation(),
                )
                .await
                .unwrap()
                .1,
        );
    }
    assert!(matches!(
        retry_parent
            .start(request("parent-retry-overflow"), cancellation())
            .await,
        Err(SubagentError::TreeConcurrencyLimit { limit: 16 })
    ));

    let retried = parent.dispose().await;
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0].status, SubagentRunStatus::Cancelled);
    assert!(harness
        .agents
        .get(&SessionId::from("parent-retry-dispose"))
        .is_none());
    let replacement = retry_parent
        .start(request("parent-retry-replacement"), cancellation())
        .await
        .unwrap()
        .1;
    replacement.dispose().await.unwrap();
    for peer in peers {
        peer.dispose().await.unwrap();
    }
    retry_parent.dispose().await;
    harness.parent.dispose().await.unwrap();
}

struct BlockingDisposeRuntime {
    blocked: bool,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl AgentRuntime for BlockingDisposeRuntime {
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
        if self.blocked {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

struct BlockingDisposeFactory {
    blocked_id: &'static str,
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl AgentFactory for BlockingDisposeFactory {
    async fn create(
        &self,
        session: Arc<tessivum::session::Session>,
        _: AgentOptions,
        _: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        Ok(Arc::new(BlockingDisposeRuntime {
            blocked: session.id().as_str() == self.blocked_id,
            started: self.started.clone(),
            release: self.release.clone(),
        }))
    }
}

#[tokio::test]
async fn dropped_disposal_future_keeps_its_slot_until_the_same_agent_is_cleaned_up() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let harness = setup_with(
        Arc::new(MemorySessionPersistence::new()),
        Arc::new(BlockingDisposeFactory {
            blocked_id: "blocked-dispose",
            started: started.clone(),
            release: release.clone(),
        }),
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let (_, blocked) = parent
        .start(request("blocked-dispose"), cancellation())
        .await
        .unwrap();
    let disposing = tokio::spawn(async move { blocked.dispose().await });
    started.notified().await;
    disposing.abort();
    assert!(matches!(disposing.await, Err(error) if error.is_cancelled()));
    started.notified().await;

    let mut children = Vec::new();
    for index in 0..15 {
        children.push(
            parent
                .start(request(&format!("blocked-peer-{index}")), cancellation())
                .await
                .unwrap()
                .1,
        );
    }
    assert!(matches!(
        parent
            .start(request("blocked-overflow"), cancellation())
            .await,
        Err(SubagentError::TreeConcurrencyLimit { limit: 16 })
    ));

    release.notify_waiters();
    while harness
        .agents
        .get(&SessionId::from("blocked-dispose"))
        .is_some()
    {
        tokio::task::yield_now().await;
    }
    let replacement = parent
        .start(request("blocked-replacement"), cancellation())
        .await
        .unwrap()
        .1;
    replacement.dispose().await.unwrap();
    for child in children {
        child.dispose().await.unwrap();
    }
    parent.dispose().await;
    harness.parent.dispose().await.unwrap();
}

#[tokio::test]
async fn cancelled_start_keeps_its_slot_until_provider_cleanup_finishes() {
    let start_persisting = Arc::new(Notify::new());
    let release_persistence = Arc::new(Notify::new());
    let persistence = Arc::new(GatedStartPersistence {
        inner: MemorySessionPersistence::new(),
        event_type: "subagent/contained-start",
        child_id: Some("cancelled-start"),
        started: start_persisting.clone(),
        release: release_persistence,
    });
    let dispose_started = Arc::new(Notify::new());
    let release_dispose = Arc::new(Notify::new());
    let harness = setup_with(
        persistence,
        Arc::new(BlockingDisposeFactory {
            blocked_id: "cancelled-start",
            started: dispose_started.clone(),
            release: release_dispose.clone(),
        }),
    )
    .await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let starting = tokio::spawn({
        let parent = parent.clone();
        async move {
            parent
                .start(request("cancelled-start"), cancellation())
                .await
        }
    });
    start_persisting.notified().await;
    starting.abort();
    assert!(matches!(starting.await, Err(error) if error.is_cancelled()));
    dispose_started.notified().await;

    let mut children = Vec::new();
    for index in 0..15 {
        children.push(
            parent
                .start(request(&format!("cancelled-peer-{index}")), cancellation())
                .await
                .unwrap()
                .1,
        );
    }
    assert!(matches!(
        parent
            .start(request("cancelled-overflow"), cancellation())
            .await,
        Err(SubagentError::TreeConcurrencyLimit { limit: 16 })
    ));

    release_dispose.notify_waiters();
    while harness
        .agents
        .get(&SessionId::from("cancelled-start"))
        .is_some()
    {
        tokio::task::yield_now().await;
    }
    let replacement = parent
        .start(request("cancelled-replacement"), cancellation())
        .await
        .unwrap()
        .1;
    replacement.dispose().await.unwrap();
    for child in children {
        child.dispose().await.unwrap();
    }
    parent.dispose().await;
    harness.parent.dispose().await.unwrap();
}

#[tokio::test]
async fn cancelled_native_factory_setups_hold_all_slots_and_parent_admissions() {
    let factory = Arc::new(GatedNativeFactory {
        gated_id: None,
        open: AtomicBool::new(false),
        starts: AtomicUsize::new(0),
        release: Notify::new(),
    });
    let harness = setup_with(Arc::new(MemorySessionPersistence::new()), factory.clone()).await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let second_service = SubagentService::new(
        harness.agents.clone(),
        harness.sessions.clone(),
        harness.persistence.clone(),
    );
    std::mem::forget(
        second_service
            .register("native", harness.provider.clone())
            .unwrap(),
    );
    let retry_parent = second_service.attach(harness.parent.clone()).unwrap();

    let mut cancellations = Vec::new();
    let mut starts = Vec::new();
    for index in 0..16 {
        let parent = parent.clone();
        let cancellation = cancellation();
        cancellations.push(cancellation.clone());
        starts.push(tokio::spawn(async move {
            parent
                .start(request(&format!("native-pending-{index}")), cancellation)
                .await
        }));
    }
    while factory.starts.load(Ordering::Acquire) != 16 {
        tokio::task::yield_now().await;
    }
    for cancellation in cancellations {
        cancellation.cancel();
    }

    assert!(matches!(
        retry_parent
            .start(request("native-pending-overflow"), cancellation())
            .await,
        Err(SubagentError::TreeConcurrencyLimit { limit: 16 })
    ));
    assert_eq!(factory.starts.load(Ordering::Acquire), 16);
    let disposing = tokio::spawn({
        let parent = parent.clone();
        async move { parent.dispose().await }
    });
    tokio::task::yield_now().await;
    assert!(!disposing.is_finished());

    factory.open.store(true, Ordering::Release);
    factory.release.notify_waiters();
    for start in starts {
        assert!(matches!(
            start.await.unwrap(),
            Err(SubagentError::CancelledBeforeAcceptance)
        ));
    }
    disposing.await.unwrap();

    let mut retry = request("native-pending-0");
    retry.resume = true;
    let replacement = retry_parent.start(retry, cancellation()).await.unwrap().1;
    replacement.dispose().await.unwrap();
    retry_parent.dispose().await;
    harness.parent.dispose().await.unwrap();
}

#[tokio::test]
async fn cancelled_cold_resume_holds_its_slot_until_native_factory_cleanup() {
    let factory = Arc::new(GatedNativeFactory {
        gated_id: Some("cold-resume-slot"),
        open: AtomicBool::new(true),
        starts: AtomicUsize::new(0),
        release: Notify::new(),
    });
    let harness = setup_with(Arc::new(MemorySessionPersistence::new()), factory.clone()).await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    let mut child_request = request("cold-resume-slot");
    child_request.mode = SubagentMode::Continuable;
    parent
        .start(child_request, cancellation())
        .await
        .unwrap()
        .1
        .dispose()
        .await
        .unwrap();

    factory.open.store(false, Ordering::Release);
    let prompt_cancellation = cancellation();
    let prompting = tokio::spawn({
        let service = harness.service.clone();
        let prompt_cancellation = prompt_cancellation.clone();
        async move {
            service
                .prompt(
                    tessivum::subagent::SubagentPromptRequest {
                        parent_session_id: SessionId::from("parent"),
                        child_session_id: SessionId::from("cold-resume-slot"),
                        mode: SubagentMode::Continuable,
                        content: vec![ContentBlock::Text {
                            text: "cancelled".into(),
                        }],
                        client_time_zone: None,
                    },
                    prompt_cancellation,
                )
                .await
        }
    });
    while factory.starts.load(Ordering::Acquire) != 1 {
        tokio::task::yield_now().await;
    }
    prompt_cancellation.cancel();

    let mut peers = Vec::new();
    for index in 0..15 {
        peers.push(
            parent
                .start(
                    request(&format!("cold-resume-peer-{index}")),
                    cancellation(),
                )
                .await
                .unwrap()
                .1,
        );
    }
    assert!(matches!(
        parent
            .start(request("cold-resume-overflow"), cancellation())
            .await,
        Err(SubagentError::TreeConcurrencyLimit { limit: 16 })
    ));
    assert!(!prompting.is_finished());

    factory.open.store(true, Ordering::Release);
    factory.release.notify_waiters();
    assert!(matches!(
        prompting.await.unwrap(),
        Err(SubagentError::Cancelled)
    ));
    harness
        .service
        .prompt(
            tessivum::subagent::SubagentPromptRequest {
                parent_session_id: SessionId::from("parent"),
                child_session_id: SessionId::from("cold-resume-slot"),
                mode: SubagentMode::Continuable,
                content: vec![ContentBlock::Text {
                    text: "retry".into(),
                }],
                client_time_zone: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    while harness
        .agents
        .get(&SessionId::from("cold-resume-slot"))
        .is_some()
    {
        tokio::task::yield_now().await;
    }
    for peer in peers {
        peer.dispose().await.unwrap();
    }
    parent.dispose().await;
    harness.parent.dispose().await.unwrap();
}

#[tokio::test]
async fn root_creation_total_survives_deleted_retired_descendant_logs() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let harness = setup_with(persistence.clone(), Arc::new(Factory)).await;
    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    for index in 0..2 {
        let id = format!("deleted-quota-{index}");
        let child = parent.start(request(&id), cancellation()).await.unwrap().1;
        child.dispose().await.unwrap();
        persistence
            .delete(&SessionId::from(id), cancellation())
            .await
            .unwrap();
    }
    parent.dispose().await;
    harness.parent.dispose().await.unwrap();

    let persistence: Arc<dyn SessionPersistence> = persistence;
    let sessions = SessionStore::new(Arc::clone(&persistence));
    let agents = AgentRegistry::new(sessions.clone());
    std::mem::forget(agents.register_factory(Arc::new(Factory)).unwrap());
    let provider = Arc::new(CountingProvider {
        native: NativeSubagentProvider::new(agents.clone(), ["scout".into()]),
        calls: AtomicUsize::new(0),
    });
    let service = SubagentService::new(agents.clone(), sessions, persistence);
    std::mem::forget(service.register("native", provider).unwrap());
    let parent_agent = Arc::new(
        agents
            .resume(SessionId::from("parent"), options(), cancellation())
            .await
            .unwrap(),
    );
    let parent = service.attach(parent_agent.clone()).unwrap();
    for index in 0..126 {
        parent
            .start(
                request(&format!("post-delete-quota-{index}")),
                cancellation(),
            )
            .await
            .unwrap()
            .1
            .dispose()
            .await
            .unwrap();
    }
    assert!(matches!(
        parent
            .start(request("post-delete-overflow"), cancellation())
            .await,
        Err(SubagentError::TreeCreationLimit { limit: 128 })
    ));
    parent.dispose().await;
    parent_agent.dispose().await.unwrap();
}

#[tokio::test]
async fn seeded_new_root_does_not_inherit_the_source_tree_ledger() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let mut copied_ledger = Vec::with_capacity(129);
    for seq in 0..=128 {
        copied_ledger.push(SessionEvent {
            event_type: "subagent/tree-creation".into(),
            seq,
            time: 0,
            data: if seq == 0 {
                json!({"baseline": 0})
            } else {
                json!({"accepted": true})
            },
            ignorable: Some(true),
            source_event_seqs: None,
            surface_op: None,
        });
    }
    let mut root_header = header("seeded-new-root", None);
    root_header.seed_length = Some(copied_ledger.len() as u64);
    persistence
        .create_seeded(&root_header, &copied_ledger, cancellation())
        .await
        .unwrap();

    let sessions = SessionStore::new(Arc::clone(&persistence));
    let agents = AgentRegistry::new(sessions.clone());
    std::mem::forget(agents.register_factory(Arc::new(Factory)).unwrap());
    let provider = Arc::new(CountingProvider {
        native: NativeSubagentProvider::new(agents.clone(), ["scout".into()]),
        calls: AtomicUsize::new(0),
    });
    let service = SubagentService::new(agents.clone(), sessions, persistence);
    std::mem::forget(service.register("native", provider.clone()).unwrap());
    let parent_agent = Arc::new(
        agents
            .resume(root_header.id, options(), cancellation())
            .await
            .unwrap(),
    );
    let parent = service.attach(parent_agent.clone()).unwrap();
    let child = parent
        .start(request("seeded-root-first-child"), cancellation())
        .await
        .unwrap()
        .1;
    assert_eq!(provider.calls.load(Ordering::Acquire), 1);
    child.dispose().await.unwrap();
    parent.dispose().await;
    parent_agent.dispose().await.unwrap();
}

#[tokio::test]
async fn legacy_reconstruction_ignores_contained_starts_copied_into_seed_history() {
    let harness = setup().await;
    let root = harness.parent.session();
    let legacy_start = SessionEvent {
        event_type: "subagent/contained-start".into(),
        seq: root.next_seq().unwrap(),
        time: 0,
        data: Value::Null,
        ignorable: Some(true),
        source_event_seqs: None,
        surface_op: None,
    };
    root.append(legacy_start.clone(), cancellation())
        .await
        .unwrap();
    let mut seeded_header = header("seeded-history", Some("parent"));
    seeded_header.seed_length = Some(1);
    seeded_header.origin = Some(SessionOrigin::Subagent);
    seeded_header.delegation_depth = Some(1);
    harness
        .sessions
        .create_seeded(seeded_header, vec![legacy_start], cancellation())
        .await
        .unwrap();

    let parent = harness.service.attach(harness.parent.clone()).unwrap();
    for index in 0..127 {
        parent
            .start(request(&format!("seed-quota-{index}")), cancellation())
            .await
            .unwrap()
            .1
            .dispose()
            .await
            .unwrap();
    }
    assert!(matches!(
        parent.start(request("seed-overflow"), cancellation()).await,
        Err(SubagentError::TreeCreationLimit { limit: 128 })
    ));
    parent.dispose().await;
    harness.parent.dispose().await.unwrap();
}

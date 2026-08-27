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
    let (_, explicit_child) = parent.start(explicit_request, cancellation()).await.unwrap();
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
    assert!(harness.parent.session().events().is_empty());

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

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tessivum::tools::{
    ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRestrictions, ToolRunContext,
    ToolRuntime,
};
use tessivum::{
    agent::{
        AgentError, AgentFactory, AgentHandle, AgentOptions, AgentRegistry, AgentRuntime,
        AgentStatus, Inbox,
    },
    approval::{
        ApprovalAnswerer, ApprovalAsked, ApprovalDecision, ApprovalError, ApprovalOutcome,
        ApprovalPolicy, ApprovalRequest, ApprovalService, HostApprovalRegistry,
    },
    goal::{GoalError, GoalPhase, GoalRef, GoalService, GoalSnapshot},
    planning::{PlanMode, PlanningService},
    session::{
        MemorySessionPersistence, Session, SessionError, SessionInspection, SessionPersistence,
        SessionStore,
    },
    SessionEvent, SessionHeader, SessionId, TessivumError, TodoItem, TodoStatus, ToolCallId,
};
use tessivum_core::{CancellationToken, ContextHandle};
use tokio::sync::Notify;

fn cancellation() -> CancellationToken {
    ContextHandle::root().scope().cancellation()
}

#[derive(Default)]
struct Runtime;

#[async_trait]
impl AgentRuntime for Runtime {
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
        _session: Arc<Session>,
        _options: AgentOptions,
        _inbox: Inbox,
        _cancellation: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        Ok(Arc::new(Runtime))
    }
}

fn header(id: &str) -> SessionHeader {
    serde_json::from_value(json!({
        "version": 0, "id": id, "createdAt": 0, "cwd": "/", "delegationDepth": 0
    }))
    .unwrap()
}

fn options() -> AgentOptions {
    serde_json::from_value(json!({"provider": "test", "model": "test"})).unwrap()
}

async fn append_turn_end(session: &Session, turn: u64) {
    session
        .append(
            SessionEvent {
                event_type: "turn/end".into(),
                seq: session.next_seq().unwrap(),
                time: 0,
                data: json!({"turn": turn}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
}

struct AskedFailingPersistence {
    inner: MemorySessionPersistence,
    fail_asked: AtomicBool,
    block_policy: AtomicBool,
    policy_entered: Arc<Notify>,
    policy_release: Arc<Notify>,
}

#[async_trait]
impl SessionPersistence for AskedFailingPersistence {
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
        if event.event_type == "approval/asked" && self.fail_asked.load(Ordering::Acquire) {
            return Err(SessionError::NotFound(session_id.clone()));
        }
        if event.event_type == "approval/policy" && self.block_policy.load(Ordering::Acquire) {
            self.policy_entered.notify_one();
            self.policy_release.notified().await;
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

async fn agent(id: &str) -> (AgentHandle, AgentRegistry) {
    let store = SessionStore::new(Arc::new(MemorySessionPersistence::default()));
    let registry = AgentRegistry::new(store);
    let _factory = registry.register_factory(Arc::new(Factory)).unwrap();
    let handle = registry
        .create(header(id), options(), cancellation())
        .await
        .unwrap();
    (handle, registry)
}

async fn append_turn_start(session: &Session, turn: u64) {
    session
        .append(
            SessionEvent {
                event_type: "turn/start".into(),
                seq: session.next_seq().unwrap(),
                time: 0,
                data: json!({"turn": turn}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
}

async fn append_user(session: &Session, text: &str) -> u64 {
    let seq = session.next_seq().unwrap();
    session
        .append(
            SessionEvent {
                event_type: "user/message".into(),
                seq,
                time: 0,
                data: json!({
                    "content": [{"type": "text", "text": text}],
                    "source": {"kind": "user"},
                    "role": "user",
                    "id": session.id(),
                }),
                ignorable: None,
                source_event_seqs: None,
                surface_op: Some(tessivum::SurfaceOp::Append),
            },
            cancellation(),
        )
        .await
        .unwrap();
    seq
}

fn goal(revision: u64, phase: GoalPhase, tombstone: bool) -> GoalSnapshot {
    GoalSnapshot {
        reference: GoalRef {
            id: "ship".into(),
            revision,
        },
        phase,
        title: "Ship durable goals".into(),
        tombstone,
    }
}

#[tokio::test]
async fn goals_are_cas_transitioned_tombstoned_and_round_bounded() {
    let (agent, _registry) = agent("goal-session").await;
    let session = agent.session();
    let service = GoalService::new(agent).unwrap();
    let first = service
        .write(None, goal(1, GoalPhase::Active, false), cancellation())
        .await
        .unwrap();
    assert!(matches!(
        service
            .write(None, goal(1, GoalPhase::Active, false), cancellation())
            .await,
        Err(GoalError::Stale)
    ));

    let activation = service
        .activate(first.reference.clone(), 1, cancellation())
        .await
        .unwrap();
    append_turn_start(&session, 1).await;
    let first_user = append_user(&session, "continue").await;
    assert_eq!(
        activation
            .admit_user_round(first_user, cancellation())
            .await
            .unwrap(),
        1
    );
    let second_user = append_user(&session, "must not replay").await;
    assert!(matches!(
        activation
            .admit_user_round(second_user, cancellation())
            .await,
        Err(GoalError::RoundCap)
    ));
    activation.disarm().await.unwrap();

    let complete = service
        .write(
            Some(first.reference),
            goal(2, GoalPhase::Complete, false),
            cancellation(),
        )
        .await
        .unwrap();
    let tombstone = service
        .write(
            Some(complete.reference),
            goal(3, GoalPhase::Complete, true),
            cancellation(),
        )
        .await
        .unwrap();
    assert!(tombstone.tombstone);
    assert!(matches!(
        service
            .write(
                Some(tombstone.reference),
                goal(4, GoalPhase::Complete, false),
                cancellation()
            )
            .await,
        Err(GoalError::Tombstoned)
    ));
    assert!(session
        .events()
        .iter()
        .any(|event| event.event_type == "goal/change"));
}

#[tokio::test]
async fn planning_is_an_observable_mode_with_frozen_whole_todos() {
    let (agent, _registry) = agent("plan-session").await;
    let session = agent.session();
    let planning = PlanningService::new(agent).unwrap();
    planning
        .set_mode(PlanMode::Plan, cancellation())
        .await
        .unwrap();
    let todos = vec![TodoItem {
        content: "verify wire shape".into(),
        status: TodoStatus::InProgress,
    }];
    planning
        .write_todos(todos.clone(), cancellation())
        .await
        .unwrap();
    assert_eq!(planning.mode().await, PlanMode::Plan);
    assert_eq!(planning.todos().await, todos);
    let todo = session
        .events()
        .into_iter()
        .find(|event| event.event_type == "todo/write")
        .unwrap();
    assert_eq!(
        todo.data,
        json!({"todos": [{"content": "verify wire shape", "status": "in_progress"}]})
    );
    assert!(session
        .events()
        .iter()
        .any(|event| event.event_type == "plan/change"));
}

struct CountingAnswerer(AtomicUsize);

#[async_trait]
impl ApprovalAnswerer for CountingAnswerer {
    async fn answer(
        &self,
        _: ApprovalAsked,
        _: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Some(true))
    }
}

struct DetailsObserver(serde_json::Value);

#[async_trait]
impl ApprovalAnswerer for DetailsObserver {
    async fn answer(
        &self,
        asked: ApprovalAsked,
        _: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        assert_eq!(asked.request.details, self.0);
        Ok(None)
    }
}

struct CountingTool(AtomicUsize);

#[async_trait]
impl ToolHandler for CountingTool {
    async fn run(&self, _: ToolRunContext, _: serde_json::Value) -> ToolHandlerResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::new(Vec::new(), false, serde_json::Value::Null))
    }
}

struct ThrowingAnswerer;

#[async_trait]
impl ApprovalAnswerer for ThrowingAnswerer {
    async fn answer(
        &self,
        _: ApprovalAsked,
        _: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        Err(TessivumError::new(
            "ANSWERER_FAILED",
            "answerer failed",
            "test",
            json!(null),
        ))
    }
}

struct WaitingAnswerer {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl ApprovalAnswerer for WaitingAnswerer {
    async fn answer(
        &self,
        _: ApprovalAsked,
        _: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(Some(true))
    }
}

struct PanickingAnswerer;

#[async_trait]
impl ApprovalAnswerer for PanickingAnswerer {
    async fn answer(
        &self,
        _: ApprovalAsked,
        _: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        panic!("answerer panic");
    }
}

struct GateHoldingAnswerer {
    approvals: ApprovalService,
    policy_entered: Arc<Notify>,
    ready: Arc<Notify>,
}

#[async_trait]
impl ApprovalAnswerer for GateHoldingAnswerer {
    async fn answer(
        &self,
        _: ApprovalAsked,
        cancellation: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        let approvals = self.approvals.clone();
        let _policy = tokio::spawn(async move {
            let _ = approvals
                .set_policy(ApprovalPolicy::Never, cancellation)
                .await;
        });
        self.policy_entered.notified().await;
        self.ready.notify_one();
        Ok(Some(true))
    }
}

#[tokio::test]
async fn never_does_not_bypass_answerers_and_events_are_auditable() {
    let (agent, _registry) = agent("approval-never").await;
    let owner = agent.authority();
    let session = agent.session();
    append_turn_start(&session, 1).await;
    let approvals = ApprovalService::new(agent).unwrap();
    let calls = Arc::new(CountingAnswerer(AtomicUsize::new(0)));
    let _answerer = approvals.register_answerer(&owner, calls.clone()).unwrap();
    approvals
        .set_policy(ApprovalPolicy::Never, cancellation())
        .await
        .unwrap();
    assert_eq!(
        approvals
            .approve(
                ApprovalRequest {
                    action: "danger".into(),
                    details: json!({})
                },
                cancellation()
            )
            .await,
        ApprovalOutcome::Rejected
    );
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    assert!(session
        .events()
        .iter()
        .any(|event| event.event_type == "approval/asked"));
    assert!(session
        .events()
        .iter()
        .any(|event| event.event_type == "approval/decided"));
}

#[tokio::test]
async fn absent_throwing_and_late_answers_fail_closed() {
    let (agent, _registry) = agent("approval-fail-closed").await;
    let owner = agent.authority();
    let session = agent.session();
    append_turn_start(&session, 1).await;
    let approvals = ApprovalService::new(agent).unwrap();
    let request = ApprovalRequest {
        action: "write".into(),
        details: json!({}),
    };
    assert_eq!(
        approvals.approve(request.clone(), cancellation()).await,
        ApprovalOutcome::Unavailable
    );

    let throwing = approvals
        .register_answerer(&owner, Arc::new(ThrowingAnswerer))
        .unwrap();
    assert_eq!(
        approvals.approve(request.clone(), cancellation()).await,
        ApprovalOutcome::Unavailable
    );
    throwing.close();

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let _waiting = approvals
        .register_answerer(
            &owner,
            Arc::new(WaitingAnswerer {
                entered: entered.clone(),
                release: release.clone(),
            }),
        )
        .unwrap();
    let abort = cancellation();
    let task = tokio::spawn({
        let approvals = approvals.clone();
        let request = request.clone();
        let abort = abort.clone();
        async move { approvals.approve(request, abort).await }
    });
    entered.notified().await;
    abort.cancel();
    release.notify_waiters();
    assert_eq!(task.await.unwrap(), ApprovalOutcome::Cancelled);
}

struct ReentrantAnswerer {
    approvals: ApprovalService,
}

#[async_trait]
impl ApprovalAnswerer for ReentrantAnswerer {
    async fn answer(
        &self,
        _: ApprovalAsked,
        cancellation: CancellationToken,
    ) -> Result<Option<bool>, TessivumError> {
        self.approvals
            .set_policy(ApprovalPolicy::Never, cancellation.clone())
            .await
            .unwrap();
        assert_eq!(
            self.approvals
                .approve(
                    ApprovalRequest {
                        action: "nested".into(),
                        details: json!({})
                    },
                    cancellation
                )
                .await,
            ApprovalOutcome::Rejected,
        );
        Ok(Some(true))
    }
}

#[tokio::test]
async fn one_shot_override_is_consumed_across_restart() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let first_store = SessionStore::new(persistence.clone());
    let first_registry = AgentRegistry::new(first_store);
    let _first_factory = first_registry.register_factory(Arc::new(Factory)).unwrap();
    let first = first_registry
        .create(header("approval-restart"), options(), cancellation())
        .await
        .unwrap();
    let first_session = first.session();
    append_turn_start(&first_session, 1).await;
    let approvals = ApprovalService::new(first).unwrap();
    approvals
        .override_next_step(ApprovalPolicy::Allow, cancellation())
        .await
        .unwrap();
    assert_eq!(
        approvals
            .approve(
                ApprovalRequest {
                    action: "first".into(),
                    details: json!({})
                },
                cancellation()
            )
            .await,
        ApprovalOutcome::AllowedOnce,
    );
    append_turn_end(&first_session, 1).await;
    drop(approvals);
    drop(first_registry);

    let second_store = SessionStore::new(persistence);
    let second_registry = AgentRegistry::new(second_store);
    let _second_factory = second_registry.register_factory(Arc::new(Factory)).unwrap();
    let second = second_registry
        .resume(
            SessionId::from("approval-restart"),
            options(),
            cancellation(),
        )
        .await
        .unwrap();
    let second_session = second.session();
    append_turn_start(&second_session, 2).await;
    let restarted = ApprovalService::new(second).unwrap();
    assert_eq!(
        restarted
            .approve(
                ApprovalRequest {
                    action: "second".into(),
                    details: json!({})
                },
                cancellation()
            )
            .await,
        ApprovalOutcome::Unavailable,
    );
    let policies = second_session
        .events()
        .into_iter()
        .filter_map(|event| {
            (event.event_type == "approval/asked").then(|| {
                serde_json::from_value::<tessivum::approval::ApprovalAsked>(event.data)
                    .unwrap()
                    .policy
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(policies, vec![ApprovalPolicy::Allow, ApprovalPolicy::Ask]);
}

#[tokio::test]
async fn failed_asked_append_retains_the_one_shot_override() {
    let persistence = Arc::new(AskedFailingPersistence {
        inner: MemorySessionPersistence::new(),
        fail_asked: AtomicBool::new(true),
        block_policy: AtomicBool::new(false),
        policy_entered: Arc::new(Notify::new()),
        policy_release: Arc::new(Notify::new()),
    });
    let registry = AgentRegistry::new(SessionStore::new(persistence.clone()));
    let _factory = registry.register_factory(Arc::new(Factory)).unwrap();
    let agent = registry
        .create(header("approval-asked-failure"), options(), cancellation())
        .await
        .unwrap();
    let session = agent.session();
    append_turn_start(&session, 1).await;
    let approvals = ApprovalService::new(agent).unwrap();
    approvals
        .override_next_step(ApprovalPolicy::Allow, cancellation())
        .await
        .unwrap();
    assert_eq!(
        approvals
            .approve(
                ApprovalRequest {
                    action: "first".into(),
                    details: json!({})
                },
                cancellation()
            )
            .await,
        ApprovalOutcome::Rejected,
    );
    persistence.fail_asked.store(false, Ordering::Release);
    assert_eq!(
        approvals
            .approve(
                ApprovalRequest {
                    action: "second".into(),
                    details: json!({})
                },
                cancellation()
            )
            .await,
        ApprovalOutcome::AllowedOnce,
    );
}

#[tokio::test]
async fn answerer_reentry_does_not_hold_the_write_gate() {
    let (agent, _registry) = agent("approval-reentry").await;
    let owner = agent.authority();
    let session = agent.session();
    append_turn_start(&session, 1).await;
    let approvals = ApprovalService::new(agent).unwrap();
    let _registration = approvals
        .register_answerer(
            &owner,
            Arc::new(ReentrantAnswerer {
                approvals: approvals.clone(),
            }),
        )
        .unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(1),
            approvals.approve(
                ApprovalRequest {
                    action: "outer".into(),
                    details: json!({})
                },
                cancellation()
            ),
        )
        .await
        .unwrap(),
        ApprovalOutcome::AllowedOnce,
    );
}

#[tokio::test]
async fn ending_the_turn_while_an_answerer_waits_records_a_denial() {
    let (agent, _registry) = agent("approval-ended-turn").await;
    let owner = agent.authority();
    let session = agent.session();
    append_turn_start(&session, 1).await;
    let approvals = ApprovalService::new(agent).unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let _waiting = approvals
        .register_answerer(
            &owner,
            Arc::new(WaitingAnswerer {
                entered: entered.clone(),
                release: release.clone(),
            }),
        )
        .unwrap();
    let task = tokio::spawn({
        let approvals = approvals.clone();
        async move {
            approvals
                .approve(
                    ApprovalRequest {
                        action: "wait".into(),
                        details: json!({}),
                    },
                    cancellation(),
                )
                .await
        }
    });
    entered.notified().await;
    append_turn_end(&session, 1).await;
    release.notify_waiters();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap(),
        ApprovalOutcome::Rejected
    );
    let decision = session
        .events()
        .into_iter()
        .rev()
        .find(|event| event.event_type == "approval/decided")
        .unwrap();
    assert_eq!(
        serde_json::from_value::<tessivum::approval::ApprovalDecision>(decision.data)
            .unwrap()
            .outcome,
        ApprovalOutcome::Rejected,
    );
}

#[tokio::test]
async fn stale_owner_generation_cannot_register_an_answerer() {
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry.register_factory(Arc::new(Factory)).unwrap();
    let first = registry
        .create(header("approval-stale-owner"), options(), cancellation())
        .await
        .unwrap();
    let stale_owner = first.authority();
    first.dispose().await.unwrap();
    let replacement = registry
        .resume(
            SessionId::from("approval-stale-owner"),
            options(),
            cancellation(),
        )
        .await
        .unwrap();
    let approvals = ApprovalService::new(replacement).unwrap();
    assert!(matches!(
        approvals.register_answerer(
            &stale_owner,
            Arc::new(CountingAnswerer(AtomicUsize::new(0)))
        ),
        Err(ApprovalError::NotLive)
    ));
}

#[tokio::test]
async fn a_different_live_agent_with_the_same_session_id_cannot_register() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let primary_registry = AgentRegistry::new(SessionStore::new(persistence.clone()));
    let _primary_factory = primary_registry
        .register_factory(Arc::new(Factory))
        .unwrap();
    let primary = primary_registry
        .create(header("approval-exact-owner"), options(), cancellation())
        .await
        .unwrap();
    let approvals = ApprovalService::new(primary).unwrap();

    let foreign_registry = AgentRegistry::new(SessionStore::new(persistence));
    let _foreign_factory = foreign_registry
        .register_factory(Arc::new(Factory))
        .unwrap();
    let foreign = foreign_registry
        .resume(
            SessionId::from("approval-exact-owner"),
            options(),
            cancellation(),
        )
        .await
        .unwrap();
    assert!(matches!(
        approvals.register_answerer(
            &foreign.authority(),
            Arc::new(CountingAnswerer(AtomicUsize::new(0)))
        ),
        Err(ApprovalError::NotLive)
    ));
}

#[tokio::test]
async fn panicking_callbacks_fail_closed_and_record_decisions() {
    let (agent, _registry) = agent("approval-panics").await;
    let owner = agent.authority();
    let session = agent.session();
    append_turn_start(&session, 1).await;
    let approvals = ApprovalService::new(agent).unwrap();
    let hook = approvals
        .register_hook(
            &owner,
            Arc::new(|_: &ApprovalRequest| -> Option<String> { panic!("hook panic") }),
        )
        .unwrap();
    assert_eq!(
        approvals
            .approve(
                ApprovalRequest {
                    action: "hook".into(),
                    details: json!({})
                },
                cancellation()
            )
            .await,
        ApprovalOutcome::Rejected,
    );
    hook.close();
    let _answerer = approvals
        .register_answerer(&owner, Arc::new(PanickingAnswerer))
        .unwrap();
    assert_eq!(
        approvals
            .approve(
                ApprovalRequest {
                    action: "answerer".into(),
                    details: json!({})
                },
                cancellation()
            )
            .await,
        ApprovalOutcome::Unavailable,
    );
    let outcomes = session
        .events()
        .into_iter()
        .filter_map(|event| {
            (event.event_type == "approval/decided").then(|| {
                serde_json::from_value::<tessivum::approval::ApprovalDecision>(event.data)
                    .unwrap()
                    .outcome
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes,
        vec![ApprovalOutcome::Rejected, ApprovalOutcome::Unavailable]
    );
}

#[tokio::test]
async fn cancellation_waiting_for_finalization_is_recorded() {
    let policy_entered = Arc::new(Notify::new());
    let policy_release = Arc::new(Notify::new());
    let ready = Arc::new(Notify::new());
    let persistence = Arc::new(AskedFailingPersistence {
        inner: MemorySessionPersistence::new(),
        fail_asked: AtomicBool::new(false),
        block_policy: AtomicBool::new(true),
        policy_entered: policy_entered.clone(),
        policy_release: policy_release.clone(),
    });
    let registry = AgentRegistry::new(SessionStore::new(persistence));
    let _factory = registry.register_factory(Arc::new(Factory)).unwrap();
    let agent = registry
        .create(header("approval-final-cancel"), options(), cancellation())
        .await
        .unwrap();
    let owner = agent.authority();
    let session = agent.session();
    append_turn_start(&session, 1).await;
    let approvals = ApprovalService::new(agent).unwrap();
    let _answerer = approvals
        .register_answerer(
            &owner,
            Arc::new(GateHoldingAnswerer {
                approvals: approvals.clone(),
                policy_entered,
                ready: ready.clone(),
            }),
        )
        .unwrap();
    let abort = cancellation();
    let task = tokio::spawn({
        let approvals = approvals.clone();
        let abort = abort.clone();
        async move {
            approvals
                .approve(
                    ApprovalRequest {
                        action: "cancel".into(),
                        details: json!({}),
                    },
                    abort,
                )
                .await
        }
    });
    ready.notified().await;
    tokio::task::yield_now().await;
    abort.cancel();
    policy_release.notify_waiters();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap(),
        ApprovalOutcome::Cancelled
    );
    let decision = session
        .events()
        .into_iter()
        .rev()
        .find(|event| event.event_type == "approval/decided")
        .unwrap();
    assert_eq!(
        serde_json::from_value::<tessivum::approval::ApprovalDecision>(decision.data)
            .unwrap()
            .outcome,
        ApprovalOutcome::Cancelled,
    );
}

#[tokio::test]
async fn host_registry_routes_exact_generations_and_audits_tool_calls() {
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry.register_factory(Arc::new(Factory)).unwrap();
    let session_id = SessionId::from("approval-host-registry");
    let first = registry
        .create(header(session_id.as_str()), options(), cancellation())
        .await
        .unwrap();
    let session = first.session();
    append_turn_start(&session, 1).await;
    let authority = first.authority();
    let approvals = ApprovalService::new(registry.get(&session_id).unwrap()).unwrap();
    let host_approvals = HostApprovalRegistry::new();
    let stale_slot = host_approvals.install(&authority, approvals).unwrap();
    let duplicate = ApprovalService::new(registry.get(&session_id).unwrap()).unwrap();
    assert!(host_approvals.install(&authority, duplicate).is_err());

    let tools = ToolRuntime::new();
    tools.set_approval(Some(Arc::new(host_approvals.clone())));
    let asked_tools = tools.scoped(ToolRestrictions::new().ask("danger")).unwrap();
    let calls = Arc::new(CountingTool(AtomicUsize::new(0)));
    let _tool = tools
        .register(ToolDefinition::new(
            "danger",
            "requires approval",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
            calls.clone(),
        ))
        .unwrap();

    let unavailable = host_approvals
        .register_answerer(&session_id, Arc::new(ThrowingAnswerer))
        .unwrap();
    let denied = asked_tools
        .execute(
            ToolRunContext {
                session: session_id.clone(),
                call: ToolCallId::from("no-answer"),
                cancellation: cancellation(),
            },
            "danger",
            json!({"path":"/tmp/a"}),
        )
        .await;
    assert!(denied.is_error);
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    unavailable.close();

    let _answerer = host_approvals
        .register_answerer(&session_id, Arc::new(CountingAnswerer(AtomicUsize::new(0))))
        .unwrap();
    let allowed = asked_tools
        .execute(
            ToolRunContext {
                session: session_id.clone(),
                call: ToolCallId::from("allowed-once"),
                cancellation: cancellation(),
            },
            "danger",
            json!({"path":"/tmp/b"}),
        )
        .await;
    assert!(!allowed.is_error);
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);

    let asked = session
        .events()
        .into_iter()
        .filter_map(|event| {
            (event.event_type == "approval/asked")
                .then(|| serde_json::from_value::<ApprovalAsked>(event.data).unwrap())
        })
        .collect::<Vec<_>>();
    let decided = session
        .events()
        .into_iter()
        .filter_map(|event| {
            (event.event_type == "approval/decided")
                .then(|| serde_json::from_value::<ApprovalDecision>(event.data).unwrap())
        })
        .collect::<Vec<_>>();
    assert_eq!(asked.len(), 2);
    assert_eq!(decided.len(), 2);
    let audit_types = session
        .events()
        .into_iter()
        .filter(|event| event.event_type.starts_with("approval/"))
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert_eq!(
        audit_types.iter().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "approval/asked",
            "approval/decided",
            "approval/asked",
            "approval/decided",
        ]
    );
    assert_eq!(asked[0].tool_name, "danger");
    assert_eq!(asked[0].policy, ApprovalPolicy::Ask);
    assert_eq!(
        asked[0].request,
        ApprovalRequest {
            action: "danger".into(),
            details: json!(null),
        }
    );
    assert_eq!(
        asked[0].call_id.as_ref().map(ToolCallId::as_str),
        Some("no-answer")
    );
    assert_eq!(asked[0].reason, None);
    assert_eq!(asked[0].session_id, session_id);
    assert!(!asked[0].approval_id.as_str().is_empty());
    assert_eq!(asked[0].approval_id, decided[0].approval_id);
    assert_ne!(asked[0].approval_id, asked[1].approval_id);
    assert_eq!(
        asked[1].call_id.as_ref().map(ToolCallId::as_str),
        Some("allowed-once")
    );
    assert_eq!(asked[1].approval_id, decided[1].approval_id);
    assert_eq!(decided[0].outcome, ApprovalOutcome::Unavailable);
    assert_eq!(decided[1].outcome, ApprovalOutcome::AllowedOnce);

    first.dispose().await.unwrap();
    assert!(host_approvals.lookup(&session_id).is_none());
    let replacement = registry
        .resume(session_id.clone(), options(), cancellation())
        .await
        .unwrap();
    let replacement_authority = replacement.authority();
    let blocked_replacement = ApprovalService::new(registry.get(&session_id).unwrap()).unwrap();
    assert!(host_approvals
        .install(&replacement_authority, blocked_replacement)
        .is_err());
    assert!(stale_slot.close());
    let replacement_service = ApprovalService::new(registry.get(&session_id).unwrap()).unwrap();
    let replacement_slot = host_approvals
        .install(&replacement_authority, replacement_service)
        .unwrap();
    assert!(!stale_slot.close());
    assert!(host_approvals.lookup(&session_id).is_some());
    drop(replacement_slot);
    replacement.dispose().await.unwrap();
}

#[test]
fn approval_asked_redacts_details_and_replays_legacy_history() {
    let secret = "approval-secret-not-durable";
    let asked = ApprovalAsked {
        approval_id: tessivum::approval::ApprovalId::new("asked-id"),
        session_id: SessionId::from("approval-serialize"),
        turn: 7,
        policy: ApprovalPolicy::Ask,
        request: ApprovalRequest {
            action: "write".into(),
            details: json!({"token": secret}),
        },
        tool_name: "write".into(),
        call_id: Some(ToolCallId::from("call-id")),
        reason: Some("policy".into()),
    };

    let durable = serde_json::to_value(&asked).unwrap();
    assert_eq!(durable["approvalId"], json!("asked-id"));
    assert_eq!(durable["sessionId"], json!("approval-serialize"));
    assert_eq!(durable["toolName"], json!("write"));
    assert_eq!(durable["callId"], json!("call-id"));
    assert_eq!(durable["reason"], json!("policy"));
    assert_eq!(durable["request"], json!({"action": "write"}));
    assert!(!durable.to_string().contains(secret));

    let legacy: ApprovalAsked = serde_json::from_value(json!({
        "approvalId": "asked-id",
        "sessionId": "approval-serialize",
        "turn": 7,
        "policy": "ask",
        "request": {"action": "write", "details": {"token": secret}},
        "toolName": "write",
        "callId": "call-id",
        "reason": "policy",
    }))
    .unwrap();
    assert_eq!(legacy.request.details, json!({"token": secret}));
}

#[tokio::test]
async fn browser_pending_approvals_are_first_wins_and_durably_resolved() {
    let (agent, _registry) = agent("approval-browser-pending").await;
    let session = agent.session();
    let session_id = session.id();
    append_turn_start(&session, 1).await;
    let authority = agent.authority();
    let approvals = ApprovalService::new(agent).unwrap();
    let browser = HostApprovalRegistry::new();
    let _slot = browser.install(&authority, approvals.clone()).unwrap();
    let secret_details = json!({"private": "approval-secret-not-durable"});
    let _details = browser
        .register_answerer(&session_id, Arc::new(DetailsObserver(secret_details.clone())))
        .unwrap();
    let mut notices = browser.subscribe();

    let first = tokio::spawn({
        let approvals = approvals.clone();
        async move {
            approvals
                .approve(
                    ApprovalRequest {
                        action: "identical".into(),
                        details: secret_details,
                    },
                    cancellation(),
                )
                .await
        }
    });
    let asked = loop {
        if let Some(asked) = session.events().into_iter().find_map(|event| {
            (event.event_type == "approval/asked")
                .then(|| serde_json::from_value::<ApprovalAsked>(event.data).unwrap())
        }) {
            break asked;
        }
        tokio::task::yield_now().await;
    };
    let durable_asked = session
        .events()
        .into_iter()
        .find(|event| event.event_type == "approval/asked")
        .unwrap()
        .data;
    assert_eq!(durable_asked["request"], json!({"action": "identical"}));
    assert!(!durable_asked
        .to_string()
        .contains("approval-secret-not-durable"));
    assert_eq!(asked.request.details, json!(null));
    browser.observe_asked(&asked);
    let requested = browser.snapshots().pop().unwrap();
    assert_eq!(requested.session_id, session_id);
    assert_eq!(requested.approval_id, asked.approval_id);
    assert_eq!(requested.tool_name, "identical");
    assert!(requested.call_id.is_none());
    assert!(requested.reason.is_none());
    let notice = notices.recv().await.unwrap();
    assert!(
        matches!(notice, tessivum::approval::ApprovalNotification::Requested(value) if value.rpc_id == requested.rpc_id)
    );

    assert!(
        !browser
            .respond(
                &requested.rpc_id,
                &SessionId::from("wrong-session"),
                &requested.approval_id,
                ApprovalOutcome::AllowedOnce,
            )
            .accepted
    );
    assert!(
        browser
            .respond(
                &requested.rpc_id,
                &requested.session_id,
                &requested.approval_id,
                ApprovalOutcome::AllowedOnce,
            )
            .accepted
    );
    assert!(
        !browser
            .respond(
                &requested.rpc_id,
                &requested.session_id,
                &requested.approval_id,
                ApprovalOutcome::Rejected,
            )
            .accepted
    );
    assert_eq!(first.await.unwrap(), ApprovalOutcome::AllowedOnce);
    let decision = session
        .events()
        .into_iter()
        .rev()
        .find_map(|event| {
            (event.event_type == "approval/decided")
                .then(|| serde_json::from_value::<ApprovalDecision>(event.data).unwrap())
        })
        .unwrap();
    assert_eq!(decision.approval_id, requested.approval_id);
    browser.observe_decided(&session_id, &decision);
    assert!(browser.snapshots().is_empty());
    assert!(
        matches!(notices.recv().await.unwrap(), tessivum::approval::ApprovalNotification::Resolved(value) if value.approval_id == requested.approval_id && value.outcome == ApprovalOutcome::AllowedOnce)
    );

    let second = tokio::spawn({
        let approvals = approvals.clone();
        async move {
            approvals
                .approve(
                    ApprovalRequest {
                        action: "identical".into(),
                        details: json!({"private": "approval-secret-not-durable"}),
                    },
                    cancellation(),
                )
                .await
        }
    });
    let rejected_asked = loop {
        if let Some(asked) = session.events().into_iter().rev().find_map(|event| {
            (event.event_type == "approval/asked")
                .then(|| serde_json::from_value::<ApprovalAsked>(event.data).unwrap())
                .filter(|asked| asked.approval_id != requested.approval_id)
        }) {
            break asked;
        }
        tokio::task::yield_now().await;
    };
    browser.observe_asked(&rejected_asked);
    let rejected = browser.snapshots().pop().unwrap();
    assert_ne!(rejected.approval_id, requested.approval_id);
    assert!(
        browser
            .respond(
                &rejected.rpc_id,
                &rejected.session_id,
                &rejected.approval_id,
                ApprovalOutcome::Rejected,
            )
            .accepted
    );
    assert_eq!(second.await.unwrap(), ApprovalOutcome::Rejected);
}

#[tokio::test]
async fn pending_authority_caps_times_out_and_cancels_fail_closed() {
    let (agent, _registry) = agent("approval-pending-cap").await;
    let session = agent.session();
    let session_id = session.id();
    append_turn_start(&session, 1).await;
    let authority = agent.authority();
    let approvals = ApprovalService::new(agent).unwrap();
    let browser = HostApprovalRegistry::with_limits(1, Duration::from_millis(50));
    let _slot = browser.install(&authority, approvals.clone()).unwrap();

    let first = tokio::spawn({
        let approvals = approvals.clone();
        async move {
            approvals
                .approve(
                    ApprovalRequest {
                        action: "one".into(),
                        details: json!({}),
                    },
                    cancellation(),
                )
                .await
        }
    });
    let first_asked = loop {
        if let Some(asked) = session.events().into_iter().find_map(|event| {
            (event.event_type == "approval/asked")
                .then(|| serde_json::from_value::<ApprovalAsked>(event.data).unwrap())
        }) {
            break asked;
        }
        tokio::task::yield_now().await;
    };
    browser.observe_asked(&first_asked);
    let stable_rpc_id = browser.snapshots().pop().unwrap().rpc_id;
    assert_eq!(browser.snapshots().pop().unwrap().rpc_id, stable_rpc_id);
    assert_eq!(
        approvals
            .approve(
                ApprovalRequest {
                    action: "two".into(),
                    details: json!({})
                },
                cancellation()
            )
            .await,
        ApprovalOutcome::Unavailable
    );
    let first_outcome = tokio::time::timeout(Duration::from_secs(1), first)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_outcome, ApprovalOutcome::Unavailable);
    let first_decision = session
        .events()
        .into_iter()
        .rev()
        .find_map(|event| {
            (event.event_type == "approval/decided")
                .then(|| serde_json::from_value::<ApprovalDecision>(event.data).unwrap())
                .filter(|decision| decision.approval_id == first_asked.approval_id)
        })
        .unwrap();
    browser.observe_decided(&session_id, &first_decision);

    let cancelled = tokio::spawn({
        let approvals = approvals.clone();
        async move {
            approvals
                .approve(
                    ApprovalRequest {
                        action: "three".into(),
                        details: json!({}),
                    },
                    cancellation(),
                )
                .await
        }
    });
    let cancelled_asked = loop {
        if let Some(asked) = session.events().into_iter().rev().find_map(|event| {
            (event.event_type == "approval/asked")
                .then(|| serde_json::from_value::<ApprovalAsked>(event.data).unwrap())
                .filter(|asked| asked.tool_name == "three")
        }) {
            break asked;
        }
        tokio::task::yield_now().await;
    };
    browser.observe_asked(&cancelled_asked);
    let pending = browser.snapshots().pop().unwrap();
    browser.cancel_session(&session_id);
    assert!(
        !browser
            .respond(
                &pending.rpc_id,
                &pending.session_id,
                &pending.approval_id,
                ApprovalOutcome::AllowedOnce,
            )
            .accepted
    );
    assert_eq!(cancelled.await.unwrap(), ApprovalOutcome::Cancelled);
}

#[tokio::test]
async fn delayed_turn_end_cannot_cancel_later_pending_approval() {
    let (agent, _registry) = agent("approval-delayed-turn-end").await;
    let session = agent.session();
    let session_id = session.id();
    let authority = agent.authority();
    let approvals = ApprovalService::new(agent).unwrap();
    let browser = HostApprovalRegistry::new();
    let _slot = browser.install(&authority, approvals.clone()).unwrap();

    append_turn_start(&session, 1).await;
    let first = tokio::spawn({
        let approvals = approvals.clone();
        async move {
            approvals
                .approve(
                    ApprovalRequest {
                        action: "first".into(),
                        details: json!({}),
                    },
                    cancellation(),
                )
                .await
        }
    });
    let first_asked = loop {
        if let Some(asked) = session.events().into_iter().find_map(|event| {
            (event.event_type == "approval/asked")
                .then(|| serde_json::from_value::<ApprovalAsked>(event.data).unwrap())
        }) {
            break asked;
        }
        tokio::task::yield_now().await;
    };
    browser.observe_asked(&first_asked);
    let first_pending = browser
        .snapshots()
        .into_iter()
        .find(|requested| requested.approval_id == first_asked.approval_id)
        .unwrap();

    append_turn_end(&session, 1).await;
    append_turn_start(&session, 2).await;
    let second = tokio::spawn({
        let approvals = approvals.clone();
        async move {
            approvals
                .approve(
                    ApprovalRequest {
                        action: "second".into(),
                        details: json!({}),
                    },
                    cancellation(),
                )
                .await
        }
    });
    let second_asked = loop {
        if let Some(asked) = session.events().into_iter().rev().find_map(|event| {
            (event.event_type == "approval/asked")
                .then(|| serde_json::from_value::<ApprovalAsked>(event.data).unwrap())
                .filter(|asked| asked.approval_id != first_asked.approval_id)
        }) {
            break asked;
        }
        tokio::task::yield_now().await;
    };
    browser.observe_asked(&second_asked);
    let second_pending = browser
        .snapshots()
        .into_iter()
        .find(|requested| requested.approval_id == second_asked.approval_id)
        .unwrap();

    browser.cancel_turn(&session_id, 1);
    assert!(
        !browser
            .respond(
                &first_pending.rpc_id,
                &first_pending.session_id,
                &first_pending.approval_id,
                ApprovalOutcome::AllowedOnce,
            )
            .accepted
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .unwrap()
            .unwrap(),
        ApprovalOutcome::Cancelled
    );
    assert!(
        browser
            .respond(
                &second_pending.rpc_id,
                &second_pending.session_id,
                &second_pending.approval_id,
                ApprovalOutcome::AllowedOnce,
            )
            .accepted
    );
    assert_eq!(second.await.unwrap(), ApprovalOutcome::AllowedOnce);
}

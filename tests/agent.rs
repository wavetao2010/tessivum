use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tessivum::{
    agent::{
        agents_service_key, AgentCancelCause, AgentError, AgentFactory, AgentOptions,
        AgentRegistry, AgentRuntime, AgentStatus, Inbox, InboxTarget, InboxUpdate,
        InboxUpdateResult,
    },
    agent_mode::AgentModeId,
    protocol::{
        Message, SessionEvent, SessionHeader, SessionId, SurfaceOp, SESSION_FORMAT_VERSION,
    },
    session::{MemorySessionPersistence, Session, SessionError, SessionPersistence, SessionStore},
};
use tessivum_core::{CancellationToken, ContextHandle};
use tokio::sync::Notify;

fn cancellation() -> CancellationToken {
    ContextHandle::root().scope().cancellation()
}

fn header(id: &str) -> SessionHeader {
    SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: SessionId::from(id),
        created_at: 0,
        cwd: None,
        parent_session: None,
        seed_length: None,
        origin: None,
        delegation_depth: None,
        agent_mode: None,
    }
}

fn options() -> AgentOptions {
    AgentOptions {
        provider: "fake".into(),
        model: "deterministic".into(),
        reasoning_effort: None,
        max_tokens: Some(32),
    }
}

fn message(id: &str) -> Message {
    serde_json::from_value(json!({
        "id": id,
        "role": "user",
        "content": [{"type": "text", "text": id}],
        "source": {"kind": "user"},
    }))
    .unwrap()
}

fn inbox_event(session: &Session, event_type: &str, data: serde_json::Value) -> SessionEvent {
    SessionEvent {
        event_type: event_type.into(),
        seq: session.next_seq().unwrap(),
        time: 0,
        data,
        ignorable: Some(true),
        source_event_seqs: None,
        surface_op: None,
    }
}

#[derive(Default)]
struct FakeRuntime {
    wakes: AtomicUsize,
    idle_calls: AtomicUsize,
    disposals: AtomicUsize,
    blocked_idle: AtomicBool,
    fail_dispose: AtomicBool,
    idle_started: Notify,
    release_idle: Notify,
}

#[async_trait]
impl AgentRuntime for FakeRuntime {
    fn status(&self) -> AgentStatus {
        AgentStatus::Idle
    }

    async fn wake(&self) -> Result<(), AgentError> {
        self.wakes.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    async fn when_idle(&self) -> Result<(), AgentError> {
        self.idle_calls.fetch_add(1, Ordering::AcqRel);
        self.idle_started.notify_one();
        if self.blocked_idle.swap(false, Ordering::AcqRel) {
            self.release_idle.notified().await;
        }
        Ok(())
    }

    async fn dispose(&self) -> Result<(), AgentError> {
        self.disposals.fetch_add(1, Ordering::AcqRel);
        if self.fail_dispose.load(Ordering::Acquire) {
            return Err(AgentError::Runtime("fixture dispose failed".into()));
        }
        Ok(())
    }
}

#[derive(Default)]
struct FakeFactory {
    fail: AtomicBool,
    block_next_idle: AtomicBool,
    sessions: Mutex<Vec<SessionId>>,
    runtimes: Mutex<Vec<Arc<FakeRuntime>>>,
}

impl FakeFactory {
    fn runtime(&self, index: usize) -> Arc<FakeRuntime> {
        self.runtimes.lock().unwrap()[index].clone()
    }
}

#[async_trait]
impl AgentFactory for FakeFactory {
    async fn create(
        &self,
        session: Arc<Session>,
        _options: AgentOptions,
        _inbox: Inbox,
        _cancellation: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        self.sessions.lock().unwrap().push(session.id());
        if self.fail.load(Ordering::Acquire) {
            return Err(AgentError::Runtime("setup failed".into()));
        }
        let runtime = Arc::new(FakeRuntime {
            blocked_idle: AtomicBool::new(self.block_next_idle.swap(false, Ordering::AcqRel)),
            ..Default::default()
        });
        self.runtimes.lock().unwrap().push(Arc::clone(&runtime));
        Ok(runtime)
    }
}

fn registry() -> (AgentRegistry, Arc<FakeFactory>) {
    let registry = AgentRegistry::new(tessivum::session::SessionStore::new(Arc::new(
        MemorySessionPersistence::new(),
    )));
    let factory = Arc::new(FakeFactory::default());
    (registry, factory)
}

#[tokio::test]
async fn setup_failure_never_publishes_and_reuses_the_same_session_identity() {
    let (registry, factory) = registry();
    let _factory = registry.register_factory(factory.clone()).unwrap();
    factory.fail.store(true, Ordering::Release);

    assert!(matches!(
        registry
            .create_or_resume(header("rollback"), options(), cancellation())
            .await,
        Err(AgentError::Runtime(_))
    ));
    assert!(registry.get(&SessionId::from("rollback")).is_none());

    factory.fail.store(false, Ordering::Release);
    let handle = registry
        .create_or_resume(header("rollback"), options(), cancellation())
        .await
        .unwrap();
    assert_eq!(handle.id(), handle.session().id());
    assert_eq!(
        factory.sessions.lock().unwrap().as_slice(),
        &[SessionId::from("rollback"), SessionId::from("rollback")]
    );
    handle.dispose().await.unwrap();
}

#[tokio::test]
async fn handle_exposes_typed_session_agent_mode() {
    let (registry, factory) = registry();
    let _factory = registry.register_factory(factory).unwrap();
    let mut mode_header = header("mode");
    mode_header.agent_mode = Some(AgentModeId::minimal());

    let handle = registry
        .create(mode_header, options(), cancellation())
        .await
        .unwrap();
    assert_eq!(handle.agent_mode(), Some(AgentModeId::minimal()));
    handle.dispose().await.unwrap();
}

#[tokio::test]
async fn factory_and_live_session_ids_are_unique() {
    let (registry, factory) = registry();
    let _factory = registry.register_factory(factory.clone()).unwrap();
    assert!(matches!(
        registry.register_factory(Arc::new(FakeFactory::default())),
        Err(AgentError::DuplicateFactory)
    ));

    let handle = registry
        .create_or_resume(header("one"), options(), cancellation())
        .await
        .unwrap();
    assert!(matches!(
        registry
            .create_or_resume(header("one"), options(), cancellation())
            .await,
        Err(AgentError::Session(SessionError::DuplicateLive(id))) if id == SessionId::from("one")
    ));
    handle.dispose().await.unwrap();
}

#[tokio::test]
async fn targeted_inbox_is_fifo_and_only_wakeable_delivery_wakes() {
    let (registry, factory) = registry();
    let _factory = registry.register_factory(factory.clone()).unwrap();
    let handle = registry
        .create_or_resume(header("queue"), options(), cancellation())
        .await
        .unwrap();
    let runtime = factory.runtime(0);
    let inbox = handle.inbox();
    let wake_revision = inbox.wake_revision();
    let waiter = tokio::spawn({
        let inbox = inbox.clone();
        async move { inbox.wait_for_wake(wake_revision).await }
    });

    handle.inject(message("inject")).await.unwrap();
    handle.followup(message("followup-1")).await.unwrap();
    handle.followup(message("followup-2")).await.unwrap();
    handle.steer(message("steer")).await.unwrap();

    assert!(waiter.await.unwrap() > wake_revision);
    assert_eq!(runtime.wakes.load(Ordering::Acquire), 3);
    assert_eq!(inbox.take_pre_step().unwrap().id.as_str(), "inject");
    assert_eq!(inbox.take_next_step().unwrap().id.as_str(), "steer");
    assert_eq!(inbox.take_next_turn().unwrap().id.as_str(), "followup-1");
    assert_eq!(inbox.take_next_turn().unwrap().id.as_str(), "followup-2");
    handle.dispose().await.unwrap();
}

#[test]
fn step_inbox_preserves_arrival_order_across_steer_inject_and_splice() {
    let inbox = Inbox::new();
    let steer = message("steer-first");
    let inject = message("inject-second");
    let followup = message("followup-third");
    inbox.steer(steer).unwrap();
    inbox.inject(inject).unwrap();
    inbox.followup(followup.clone()).unwrap();
    assert!(matches!(
        inbox.update(&followup.id, InboxUpdate::Steer),
        InboxUpdateResult::Updated {
            target: InboxTarget::Steer,
            ..
        }
    ));

    assert_eq!(
        inbox
            .take_step_batch()
            .into_iter()
            .map(|message| message.id)
            .collect::<Vec<_>>(),
        vec![
            tessivum::protocol::MessageId::from("steer-first"),
            tessivum::protocol::MessageId::from("inject-second"),
            tessivum::protocol::MessageId::from("followup-third"),
        ]
    );
    assert!(inbox.is_empty());
}

#[test]
fn inbox_updates_duplicate_bodies_by_id_without_reordering_fifo() {
    let inbox = Inbox::new();
    let mut first = message("first");
    let mut second = message("second");
    let mut third = message("third");
    for queued in [&mut first, &mut second, &mut third] {
        queued.content = vec![tessivum::protocol::ContentBlock::Text {
            text: "same body".into(),
        }];
        inbox.followup(queued.clone()).unwrap();
    }

    assert!(matches!(
        inbox.update(
            &second.id,
            InboxUpdate::Edit {
                content: vec![tessivum::protocol::ContentBlock::Text {
                    text: "edited".into(),
                }],
            },
        ),
        InboxUpdateResult::Updated {
            target: InboxTarget::Followup,
            ..
        }
    ));
    assert!(matches!(
        inbox.update(&first.id, InboxUpdate::Remove),
        InboxUpdateResult::Updated {
            target: InboxTarget::Followup,
            ..
        }
    ));
    assert!(matches!(
        inbox.update(&second.id, InboxUpdate::Steer),
        InboxUpdateResult::Updated {
            target: InboxTarget::Steer,
            ..
        }
    ));

    assert_eq!(inbox.take_next_turn().unwrap().id, third.id);
    let steered = inbox.take_next_step().unwrap();
    assert_eq!(steered.id, second.id);
    assert_eq!(
        steered.content,
        vec![tessivum::protocol::ContentBlock::Text {
            text: "edited".into(),
        }]
    );
    assert_eq!(
        inbox.update(&first.id, InboxUpdate::Remove),
        InboxUpdateResult::NotPending
    );
}

#[tokio::test]
async fn resume_replays_only_unclaimed_next_turn_inbox_mutations() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let first_registry = AgentRegistry::new(SessionStore::new(persistence.clone()));
    let factory = Arc::new(FakeFactory::default());
    let _factory = first_registry.register_factory(factory.clone()).unwrap();
    let first = first_registry
        .create(header("durable-inbox"), options(), cancellation())
        .await
        .unwrap();
    let session = first.session();
    let first_message = message("first");
    let original_edited = message("edited");
    let mut edited = original_edited.clone();
    edited.content = vec![tessivum::protocol::ContentBlock::Text {
        text: "已编辑".into(),
    }];
    let edited_id = edited.id.clone();
    let edited_content = edited.content.clone();
    let steered = message("steered");
    let claimed = message("claimed");

    for queued in [&first_message, &original_edited, &steered, &claimed] {
        session
            .append(
                inbox_event(
                    &session,
                    "agent/inbox/enqueued",
                    json!({"target": "next-turn", "message": queued}),
                ),
                cancellation(),
            )
            .await
            .unwrap();
    }
    session
        .append(
            inbox_event(
                &session,
                "agent/inbox/spliced",
                json!({"target": "next-turn", "itemId": "edited", "action": "edit", "message": edited}),
            ),
            cancellation(),
        )
        .await
        .unwrap();
    session
        .append(
            inbox_event(
                &session,
                "agent/inbox/spliced",
                json!({"target": "next-turn", "itemId": "first", "action": "remove", "message": first_message}),
            ),
            cancellation(),
        )
        .await
        .unwrap();
    session
        .append(
            inbox_event(
                &session,
                "agent/inbox/spliced",
                json!({"target": "next-step", "itemId": "steered", "action": "steer", "message": steered}),
            ),
            cancellation(),
        )
        .await
        .unwrap();
    session
        .append(
            SessionEvent {
                event_type: "user/message".into(),
                seq: session.next_seq().unwrap(),
                time: 0,
                data: serde_json::to_value(&claimed).unwrap(),
                ignorable: None,
                source_event_seqs: None,
                surface_op: Some(SurfaceOp::Append),
            },
            cancellation(),
        )
        .await
        .unwrap();
    first.dispose().await.unwrap();

    let resumed_registry = AgentRegistry::new(SessionStore::new(persistence));
    let _factory = resumed_registry.register_factory(factory).unwrap();
    let resumed = resumed_registry
        .resume(SessionId::from("durable-inbox"), options(), cancellation())
        .await
        .unwrap();

    assert_eq!(
        resumed
            .inbox()
            .pending()
            .into_iter()
            .map(|(target, message)| (target, message.id, message.content))
            .collect::<Vec<_>>(),
        vec![(InboxTarget::Followup, edited_id, edited_content)]
    );
    resumed.dispose().await.unwrap();
}

#[tokio::test]
async fn resume_rejects_out_of_order_durable_inbox_mutations() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let id = SessionId::from("corrupt-durable-inbox");
    persistence
        .create(&header(id.as_str()), cancellation())
        .await
        .unwrap();
    persistence
        .append(
            &id,
            &SessionEvent {
                event_type: "agent/inbox/spliced".into(),
                seq: 0,
                time: 0,
                data: json!({
                    "target": "next-turn",
                    "itemId": "missing",
                    "action": "remove",
                    "message": message("missing"),
                }),
                ignorable: Some(true),
                source_event_seqs: None,
                surface_op: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(persistence));
    let _factory = registry
        .register_factory(Arc::new(FakeFactory::default()))
        .unwrap();

    assert!(matches!(
        registry.resume(id, options(), cancellation()).await,
        Err(AgentError::Session(
            SessionError::InboxMutationOutOfOrder { .. }
        ))
    ));
}

#[tokio::test]
async fn idle_cancellation_is_a_noop_and_followups_remain_accepted() {
    let (registry, factory) = registry();
    let _factory = registry.register_factory(factory).unwrap();
    let handle = registry
        .create_or_resume(header("idle-cancel"), options(), cancellation())
        .await
        .unwrap();

    assert!(!handle.cancel(AgentCancelCause::User, false));
    assert!(handle.cancel_options().is_none());
    handle.followup(message("after-cancel")).await.unwrap();
    assert_eq!(
        handle.inbox().take_next_turn().unwrap().id.as_str(),
        "after-cancel"
    );

    handle.dispose().await.unwrap();
}

#[tokio::test]
async fn when_idle_follows_a_replacement_wakeup_to_quiescence() {
    let (registry, factory) = registry();
    factory.block_next_idle.store(true, Ordering::Release);
    let _factory = registry.register_factory(factory.clone()).unwrap();
    let handle = Arc::new(
        registry
            .create_or_resume(header("idle"), options(), cancellation())
            .await
            .unwrap(),
    );
    let runtime = factory.runtime(0);
    let waiting = tokio::spawn({
        let handle = Arc::clone(&handle);
        async move { handle.when_idle().await }
    });
    tokio::time::timeout(Duration::from_secs(1), runtime.idle_started.notified())
        .await
        .unwrap();
    handle.followup(message("replacement")).await.unwrap();
    runtime.release_idle.notify_one();
    waiting.await.unwrap().unwrap();
    assert_eq!(runtime.idle_calls.load(Ordering::Acquire), 2);
    handle.dispose().await.unwrap();
}

#[tokio::test]
async fn stale_dispose_cannot_remove_a_later_generation() {
    let (registry, factory) = registry();
    let _factory = registry.register_factory(factory.clone()).unwrap();
    let first = registry
        .create_or_resume(header("generation"), options(), cancellation())
        .await
        .unwrap();
    let first_runtime = factory.runtime(0);
    first.dispose().await.unwrap();
    let second = registry
        .create_or_resume(header("generation"), options(), cancellation())
        .await
        .unwrap();
    let second_runtime = factory.runtime(1);

    first.dispose().await.unwrap();
    assert!(registry.get(&SessionId::from("generation")).is_some());
    assert_eq!(first_runtime.disposals.load(Ordering::Acquire), 1);
    assert_eq!(second_runtime.disposals.load(Ordering::Acquire), 0);
    second.dispose().await.unwrap();
}

#[tokio::test]
async fn failed_dispose_retains_the_live_generation_until_retry_succeeds() {
    let (registry, factory) = registry();
    let _factory = registry.register_factory(factory.clone()).unwrap();
    let handle = registry
        .create_or_resume(header("dispose-retry"), options(), cancellation())
        .await
        .unwrap();
    let runtime = factory.runtime(0);
    runtime.fail_dispose.store(true, Ordering::Release);

    assert!(handle.dispose().await.is_err());
    assert!(registry.get(&SessionId::from("dispose-retry")).is_some());
    assert_eq!(runtime.disposals.load(Ordering::Acquire), 1);

    runtime.fail_dispose.store(false, Ordering::Release);
    handle.dispose().await.unwrap();
    assert!(registry.get(&SessionId::from("dispose-retry")).is_none());
    assert_eq!(runtime.disposals.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn authority_rejects_a_replaced_agent_generation() {
    let (registry, factory) = registry();
    let _factory = registry.register_factory(factory).unwrap();
    let first = registry
        .create_or_resume(header("authority"), options(), cancellation())
        .await
        .unwrap();
    let stale = first.authority();
    assert!(stale.is_live());

    first.dispose().await.unwrap();
    let second = registry
        .create_or_resume(header("authority"), options(), cancellation())
        .await
        .unwrap();
    assert!(!stale.is_live());
    assert!(second.authority().is_live());
    second.dispose().await.unwrap();
}

#[tokio::test]
async fn registry_shutdown_waits_for_each_runtime_disposal() {
    let (registry, factory) = registry();
    let _factory = registry.register_factory(factory.clone()).unwrap();
    let _a = registry
        .create_or_resume(header("a"), options(), cancellation())
        .await
        .unwrap();
    let _b = registry
        .create_or_resume(header("b"), options(), cancellation())
        .await
        .unwrap();

    registry.shutdown().await.unwrap();
    assert!(registry.list().is_empty());
    assert_eq!(factory.runtime(0).disposals.load(Ordering::Acquire), 1);
    assert_eq!(factory.runtime(1).disposals.load(Ordering::Acquire), 1);
}

#[test]
fn registry_publishes_the_versioned_service_to_context() {
    let (registry, _) = registry();
    let context = ContextHandle::root();
    let provider = registry.publish(&context).unwrap();

    assert!(provider.is_current());
    assert_eq!(agents_service_key().diagnostic_key(), "harness.agents@1");
    assert!(context
        .get::<AgentRegistry>(&agents_service_key())
        .unwrap()
        .is_some());
}

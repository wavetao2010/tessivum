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
        AgentRegistry, AgentRuntime, AgentStatus, Inbox,
    },
    protocol::{Message, SessionHeader, SessionId, SESSION_FORMAT_VERSION},
    session::{MemorySessionPersistence, Session},
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
        agent_preset: None,
    }
}

fn options() -> AgentOptions {
    AgentOptions {
        provider: "fake".into(),
        model: "deterministic".into(),
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

#[derive(Default)]
struct FakeRuntime {
    wakes: AtomicUsize,
    idle_calls: AtomicUsize,
    disposals: AtomicUsize,
    blocked_idle: AtomicBool,
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
        Err(AgentError::DuplicateLive(id)) if id == SessionId::from("one")
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

#[tokio::test]
async fn cancellation_is_first_wins_and_keep_inbox_is_sticky() {
    let (registry, factory) = registry();
    let _factory = registry.register_factory(factory).unwrap();
    let kept = registry
        .create_or_resume(header("kept"), options(), cancellation())
        .await
        .unwrap();
    kept.followup(message("keep")).await.unwrap();
    assert!(kept.cancel(AgentCancelCause::User, true));
    assert!(!kept.cancel(AgentCancelCause::Parent, false));
    assert_eq!(kept.inbox().take_next_turn().unwrap().id.as_str(), "keep");
    assert_eq!(kept.cancel_options().unwrap().cause, AgentCancelCause::User);

    let cleared = registry
        .create_or_resume(header("cleared"), options(), cancellation())
        .await
        .unwrap();
    cleared.followup(message("clear")).await.unwrap();
    assert!(cleared.cancel(AgentCancelCause::Parent, false));
    assert!(cleared.inbox().is_empty());
    kept.dispose().await.unwrap();
    cleared.dispose().await.unwrap();
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

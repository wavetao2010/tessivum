use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tessivum::{
    agent::{
        AgentError, AgentFactory, AgentHandle, AgentOptions, AgentRegistry, AgentRuntime,
        AgentStatus, Inbox,
    },
    jobs::{JobError, JobOwner, JobStart, JobStatus, LocalJobRegistry},
    protocol::{SessionHeader, SessionId, SESSION_FORMAT_VERSION},
    session::{MemorySessionPersistence, Session, SessionStore},
    tools::{ToolRunContext, ToolRuntime},
    ToolCallId,
};
use tessivum_core::{CancellationToken, ContextHandle};

struct JobsAgentRuntime;

#[async_trait]
impl AgentRuntime for JobsAgentRuntime {
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

struct JobsAgentFactory;

#[async_trait]
impl AgentFactory for JobsAgentFactory {
    async fn create(
        &self,
        _session: Arc<Session>,
        _options: AgentOptions,
        _inbox: Inbox,
        _cancellation: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        Ok(Arc::new(JobsAgentRuntime))
    }
}

struct AttachedOwner {
    controller: ContextHandle,
    _agents: AgentRegistry,
    agent: AgentHandle,
    owner: JobOwner,
}

fn agent_options() -> AgentOptions {
    AgentOptions {
        provider: "test".into(),
        model: "test".into(),
        reasoning_effort: None,
        max_tokens: None,
    }
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

fn cancellation() -> CancellationToken {
    ContextHandle::root().scope().cancellation()
}

async fn owner(registry: &LocalJobRegistry, name: &str) -> AttachedOwner {
    let agents = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = agents.register_factory(Arc::new(JobsAgentFactory)).unwrap();
    let agent = agents
        .create_or_resume(header(name), agent_options(), cancellation())
        .await
        .unwrap();
    let controller = ContextHandle::root();
    let owner = registry
        .attach_owner(&agent.authority(), &controller)
        .expect("live agent attaches to a live controller");
    AttachedOwner {
        controller,
        _agents: agents,
        agent,
        owner,
    }
}

fn pending(kind: &str) -> JobStart {
    JobStart::new(kind, "pending", 64, |control| async move {
        control.cancelled().await;
        Ok(json!("released"))
    })
}

#[tokio::test]
async fn owners_are_isolated_for_observation_and_killing() {
    let registry = LocalJobRegistry::new();
    let a = owner(&registry, "owner-a").await;
    let b = owner(&registry, "owner-b").await;
    let job = a.owner.start(pending("work")).unwrap();

    assert!(matches!(b.owner.get(&job.id), Err(JobError::NotFound)));
    assert!(matches!(b.owner.kill(&job.id), Err(JobError::NotFound)));
    assert!(b.owner.list().unwrap().is_empty());

    a.owner.kill(&job.id).unwrap();
    assert_eq!(
        a.owner
            .wait(&job.id, Some(Duration::from_secs(1)), None)
            .await
            .unwrap()
            .status,
        JobStatus::Killed
    );
}

#[tokio::test]
async fn output_cursor_reports_tail_loss_and_advances() {
    let registry = LocalJobRegistry::new();
    let attached = owner(&registry, "owner").await;
    let job = attached
        .owner
        .start(JobStart::new("log", "tail", 3, |control| async move {
            assert!(control.write_text("abcdef"));
            Ok(json!(true))
        }))
        .unwrap();
    attached
        .owner
        .wait(&job.id, Some(Duration::from_secs(1)), None)
        .await
        .unwrap();

    let page = attached.owner.read(&job.id, 0, 2).unwrap();
    assert_eq!(page.lost, 3);
    assert_eq!(page.bytes, b"de");
    assert_eq!(page.next_cursor, 5);
    let page = attached.owner.read(&job.id, page.next_cursor, 2).unwrap();
    assert_eq!(page.bytes, b"f");
    assert_eq!(page.next_cursor, 6);
}

#[tokio::test]
async fn kill_transitions_to_stopping_then_killed_after_release() {
    let registry = LocalJobRegistry::new();
    let attached = owner(&registry, "owner").await;
    let job = attached.owner.start(pending("kill")).unwrap();

    assert_eq!(
        attached.owner.kill(&job.id).unwrap().status,
        JobStatus::Stopping
    );
    assert_eq!(
        attached
            .owner
            .wait(&job.id, Some(Duration::from_secs(1)), None)
            .await
            .unwrap()
            .status,
        JobStatus::Killed
    );
}

#[tokio::test]
async fn first_settlement_wins_when_a_hook_attempts_two() {
    let registry = LocalJobRegistry::new();
    let attached = owner(&registry, "owner").await;
    let job = attached
        .owner
        .start(JobStart::new("settle", "first", 64, |control| async move {
            assert!(control.complete(json!({"winner": "complete"})));
            assert!(!control.fail("late failure"));
            Err("returned failure loses".into())
        }))
        .unwrap();

    let done = attached
        .owner
        .wait(&job.id, Some(Duration::from_secs(1)), None)
        .await
        .unwrap();
    assert_eq!(done.status, JobStatus::Completed);
    assert_eq!(done.result, Some(json!({"winner": "complete"})));
    assert!(attached.owner.completion_notice(&job.id).unwrap().is_some());
    assert!(attached.owner.report(&job.id).unwrap());
    assert!(attached.owner.completion_notice(&job.id).unwrap().is_none());
}

#[tokio::test]
async fn wait_timeout_or_cancellation_does_not_cancel_the_job() {
    let registry = LocalJobRegistry::new();
    let attached = owner(&registry, "owner").await;
    let job = attached.owner.start(pending("wait")).unwrap();

    assert!(matches!(
        attached
            .owner
            .wait(&job.id, Some(Duration::from_millis(1)), None)
            .await,
        Err(JobError::TimedOut)
    ));
    let cancellation = cancellation();
    cancellation.cancel();
    assert!(matches!(
        attached.owner.wait(&job.id, None, Some(cancellation)).await,
        Err(JobError::Cancelled)
    ));
    assert_eq!(
        attached.owner.get(&job.id).unwrap().status,
        JobStatus::Running
    );

    attached.owner.kill(&job.id).unwrap();
    attached
        .owner
        .wait(&job.id, Some(Duration::from_secs(1)), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn owner_disposal_cancels_and_awaits_resource_release() {
    let registry = LocalJobRegistry::new();
    let attached = owner(&registry, "owner").await;
    let released = Arc::new(AtomicBool::new(false));
    let released_by_hook = Arc::clone(&released);
    attached
        .owner
        .start(JobStart::new("owner", "dispose", 64, move |control| {
            let released = Arc::clone(&released_by_hook);
            async move {
                control.cancelled().await;
                released.store(true, Ordering::Release);
                Ok(json!(null))
            }
        }))
        .unwrap();

    attached.controller.scope().dispose().await.unwrap();
    assert!(released.load(Ordering::Acquire));
}

#[tokio::test]
async fn stale_agent_authority_cannot_preattach_jobs() {
    let registry = LocalJobRegistry::new();
    let attached = owner(&registry, "stale-agent").await;
    let stale = attached.agent.authority();
    attached.agent.dispose().await.unwrap();
    assert!(!stale.is_live());
    let target = LocalJobRegistry::new();
    assert!(matches!(
        target.attach_owner(&stale, &ContextHandle::root()),
        Err(JobError::OwnerNotAttached)
    ));
}

#[tokio::test]
async fn misrouted_tool_context_cannot_select_another_owner_capability() {
    let registry = LocalJobRegistry::new();
    let victim = owner(&registry, "victim").await;
    let attacker = owner(&registry, "attacker").await;
    let victim_job = victim.owner.start(pending("victim")).unwrap();

    let runtime = ToolRuntime::new();
    let _tools = attacker.owner.install_tools(&runtime).unwrap();
    let output = runtime
        .execute(
            ToolRunContext {
                session: victim.agent.id(),
                call: ToolCallId::from("misrouted-owner"),
                cancellation: cancellation(),
            },
            "jobs.list",
            json!({}),
        )
        .await;
    assert!(output.is_error);
    assert_eq!(output.meta["code"], "JOB_NOT_FOUND");

    victim.owner.kill(&victim_job.id).unwrap();
    victim
        .owner
        .wait(&victim_job.id, Some(Duration::from_secs(1)), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn stale_owner_cleanup_cannot_remove_a_reattached_generation() {
    let registry = LocalJobRegistry::new();
    let first = owner(&registry, "reused").await;
    let stale = first.owner.clone();
    first.owner.start(pending("first")).unwrap();
    first.owner.dispose().await;

    let replacement = owner(&registry, "reused").await;
    let replacement_job = replacement.owner.start(pending("replacement")).unwrap();
    stale.dispose().await;
    assert_eq!(
        replacement.owner.get(&replacement_job.id).unwrap().status,
        JobStatus::Running
    );

    replacement.owner.kill(&replacement_job.id).unwrap();
    replacement
        .owner
        .wait(&replacement_job.id, Some(Duration::from_secs(1)), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn hook_reentrant_owner_and_runtime_disposal_never_wait_for_itself() {
    let registry = LocalJobRegistry::new();
    let attached = owner(&registry, "reentrant").await;
    let owner_in_hook = attached.owner.clone();
    let registry_in_hook = registry.clone();
    let returned = Arc::new(AtomicBool::new(false));
    let returned_in_hook = Arc::clone(&returned);
    attached
        .owner
        .start(JobStart::new("reentrant", "dispose", 64, move |_control| {
            let owner = owner_in_hook.clone();
            let registry = registry_in_hook.clone();
            let returned = Arc::clone(&returned_in_hook);
            async move {
                owner.dispose().await;
                registry.dispose().await;
                returned.store(true, Ordering::Release);
                Ok(json!(null))
            }
        }))
        .unwrap();

    tokio::time::timeout(Duration::from_secs(1), registry.dispose())
        .await
        .expect("external disposal joins the hook's shared completion");
    assert!(returned.load(Ordering::Acquire));
    tokio::time::timeout(
        Duration::from_secs(1),
        attached.controller.scope().dispose(),
    )
    .await
    .expect("stale scope cleanup returns")
    .expect("stale scope cleanup succeeds");
}

#[tokio::test]
async fn multibyte_terminal_error_truncates_on_a_character_boundary_and_notifies() {
    let registry = LocalJobRegistry::new();
    let attached = owner(&registry, "unicode").await;
    let notified = Arc::new(AtomicBool::new(false));
    let notified_by_observer = Arc::clone(&notified);
    let _observer = registry.on_done(move |_| notified_by_observer.store(true, Ordering::Release));
    let error = format!("{}é!", "a".repeat(16_383));
    let job = attached
        .owner
        .start(JobStart::new("unicode", "boundary", 64, move |_control| {
            let error = error.clone();
            async move { Err(error) }
        }))
        .unwrap();

    let done = attached
        .owner
        .wait(&job.id, Some(Duration::from_secs(1)), None)
        .await
        .unwrap();
    assert_eq!(done.status, JobStatus::Failed);
    assert_eq!(done.error, Some("a".repeat(16_383)));
    assert!(notified.load(Ordering::Acquire));
}

#[tokio::test]
async fn tool_runtime_exposes_bounded_list_read_kill_and_wait_schemas() {
    let registry = LocalJobRegistry::new();
    let attached = owner(&registry, "tools").await;
    let runtime = ToolRuntime::new();
    let tools = attached.owner.install_tools(&runtime).unwrap();
    assert_eq!(tools.len(), 4);
    assert_eq!(
        runtime
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>(),
        vec!["jobs.kill", "jobs.list", "jobs.read", "jobs.wait"]
    );
}

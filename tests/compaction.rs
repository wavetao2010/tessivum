use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::Poll,
};

use async_trait::async_trait;
use futures_util::stream;
use serde_json::json;
use tessivum::{
    compaction::{
        CompactionConfig, CompactionError, CompactionOutcome, CompactionRange, CompactionService,
        CompactionTrigger, ToolResultPruneOutcome,
    },
    llm::{LlmAdapter, LlmProviderRegistration, LlmRuntime, LlmStream},
    protocol::{
        ContentBlock, FinishReason, GeneratePurpose, GenerateRequest, MessageRole, SessionEvent,
        SessionHeader, SessionId, SurfaceOp, ToolCallId, SESSION_FORMAT_VERSION,
    },
    session::{
        MemorySessionPersistence, Session, SessionError, SessionInspection, SessionPersistence,
        SessionStore,
    },
    TessivumError,
};
use tessivum_core::{CancellationToken, ContextHandle};
use tokio::sync::Notify;

#[derive(Clone)]
struct StaticAdapter {
    chunks: Vec<tessivum::StreamChunk>,
    requests: Arc<Mutex<Vec<GenerateRequest>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmAdapter for StaticAdapter {
    async fn generate(
        &self,
        request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.requests).push(request);
        Ok(Box::pin(stream::iter(
            self.chunks.clone().into_iter().map(Ok),
        )))
    }
}

#[derive(Clone)]
struct InterleavingAdapter {
    chunks: Vec<tessivum::StreamChunk>,
    session: Arc<Session>,
}

#[async_trait]
impl LlmAdapter for InterleavingAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        user(self.session.as_ref(), "concurrent", "newer surface").await;
        Ok(Box::pin(stream::iter(
            self.chunks.clone().into_iter().map(Ok),
        )))
    }
}

struct FailingAdapter;

#[async_trait]
impl LlmAdapter for FailingAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        Err(TessivumError::new(
            "SUMMARY_DOWN",
            "summary provider failed",
            "llm",
            serde_json::Value::Null,
        ))
    }
}

struct PendingAdapter;

#[async_trait]
impl LlmAdapter for PendingAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        Ok(Box::pin(stream::pending()))
    }
}

struct BlockingAppendPersistence {
    inner: MemorySessionPersistence,
    blocked_seq: u64,
    blocked: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl BlockingAppendPersistence {
    fn new(blocked_seq: u64) -> Self {
        Self {
            inner: MemorySessionPersistence::new(),
            blocked_seq,
            blocked: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        }
    }
}

#[async_trait]
impl SessionPersistence for BlockingAppendPersistence {
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
        if event.seq == self.blocked_seq && !self.blocked.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
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

fn cancellation() -> CancellationToken {
    ContextHandle::root().scope().cancellation()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn header(id: &str, seed_length: Option<u64>) -> SessionHeader {
    SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: SessionId::from(id),
        created_at: 0,
        cwd: None,
        parent_session: None,
        seed_length,
        origin: None,
        delegation_depth: None,
        agent_mode: None,
    }
}

async fn session(id: &str) -> Arc<Session> {
    SessionStore::new(Arc::new(MemorySessionPersistence::new()))
        .create(header(id, None), cancellation())
        .await
        .unwrap()
}

async fn append(
    session: &Session,
    event_type: &str,
    data: serde_json::Value,
    sources: Option<Vec<u64>>,
    operation: SurfaceOp,
) -> u64 {
    let seq = session.next_seq().unwrap();
    session
        .append(
            SessionEvent {
                event_type: event_type.into(),
                seq,
                time: 0,
                data,
                ignorable: None,
                source_event_seqs: sources,
                surface_op: Some(operation),
            },
            cancellation(),
        )
        .await
        .unwrap();
    seq
}

async fn user(session: &Session, id: &str, text: &str) -> u64 {
    append(
        session,
        "user/message",
        json!({
            "id": id,
            "role": "user",
            "content": [{"type": "text", "text": text}],
            "source": {"kind": "user"},
        }),
        None,
        SurfaceOp::Append,
    )
    .await
}

async fn assistant_call(session: &Session, call: &str) -> u64 {
    append(
        session,
        "assistant/message",
        json!({
            "message": {
                "id": format!("call-{call}"),
                "role": "assistant",
                "content": [{
                    "type": "tool-call",
                    "id": call,
                    "name": "probe",
                    "arguments": "{}",
                }],
                "source": {"kind": "model", "provider": "test", "model": "test"},
            },
        }),
        Some(Vec::new()),
        SurfaceOp::Append,
    )
    .await
}

async fn tool_result(session: &Session, call: &str, text: &str, source: u64) -> u64 {
    append(
        session,
        "tool/result",
        json!({
            "message": {
                "id": format!("result-{call}"),
                "role": "user",
                "content": [{
                    "type": "tool-result",
                    "toolCallId": call,
                    "content": [{"type": "text", "text": text}],
                }],
                "source": {"kind": "tool", "callId": call},
            },
        }),
        Some(vec![source]),
        SurfaceOp::Append,
    )
    .await
}

fn text_stream(text: &str) -> Vec<tessivum::StreamChunk> {
    vec![
        tessivum::StreamChunk::BlockStart {
            index: 0,
            block_type: "text".into(),
        },
        tessivum::StreamChunk::TextDelta {
            index: 0,
            text: text.into(),
        },
        tessivum::StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text { text: text.into() },
        },
        tessivum::StreamChunk::Finish {
            reason: FinishReason::Stop,
            replay_state: None,
        },
    ]
}

fn service(adapter: Arc<dyn LlmAdapter>) -> (CompactionService, LlmProviderRegistration) {
    let runtime = LlmRuntime::new();
    let registration = runtime.register("test", adapter).unwrap();
    (
        CompactionService::new(
            runtime,
            CompactionConfig {
                provider: "test".into(),
                model: "test".into(),
                max_tokens: Some(128),
                system: None,
                ..CompactionConfig::default()
            },
        )
        .unwrap(),
        registration,
    )
}

#[tokio::test]
async fn compact_now_is_a_standalone_noop_without_events_or_model_call() {
    let session = session("noop").await;
    user(&session, "one", "only one").await;
    let calls = Arc::new(AtomicUsize::new(0));
    let (service, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream("never called"),
        requests: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::clone(&calls),
    }));

    assert_eq!(
        service.compact_now(&session, cancellation()).await.unwrap(),
        CompactionOutcome::Noop {
            trigger: CompactionTrigger::Manual,
            token_estimate: 0,
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(session.events().len(), 1);
}

#[tokio::test]
async fn first_compaction_wins_the_per_session_lock() {
    let session = session("busy").await;
    user(&session, "one", "first").await;
    user(&session, "two", "second").await;
    let (service, _registration) = service(Arc::new(PendingAdapter));
    let first_cancellation = cancellation();
    let first_service = service.clone();
    let first_session = Arc::clone(&session);
    let first = tokio::spawn(async move {
        first_service
            .compact_region(
                &first_session,
                CompactionRange { start: 0, end: 1 },
                first_cancellation,
            )
            .await
    });
    tokio::task::yield_now().await;

    assert!(matches!(
        service
            .compact_region(
                &session,
                CompactionRange { start: 0, end: 1 },
                cancellation()
            )
            .await,
        Err(CompactionError::Busy { .. })
    ));
    // The task-owned token is unavailable after moving it; cancelling the
    // session-independent root token used by the service is not needed here.
    first.abort();
}

#[tokio::test]
async fn regions_are_inclusive_current_surface_bounds() {
    let session = session("bounds").await;
    user(&session, "one", "first").await;
    user(&session, "two", "second").await;
    let (service, _registration) = service(Arc::new(FailingAdapter));

    for range in [
        CompactionRange { start: 1, end: 0 },
        CompactionRange { start: 0, end: 2 },
    ] {
        assert!(matches!(
            service.compact_region(&session, range, cancellation()).await,
            Err(CompactionError::Invalid(error)) if error.code == "INVALID_COMPACTION_REGION"
        ));
    }
    assert_eq!(session.events().len(), 2);
}

#[tokio::test]
async fn tool_pairs_must_be_ordered_and_wholly_selected() {
    let session = session("pairs").await;
    let call = assistant_call(&session, "call-a").await;
    tool_result(&session, "call-a", "done", call).await;
    let (service, _registration) = service(Arc::new(FailingAdapter));

    assert!(matches!(
        service
            .compact_region(&session, CompactionRange { start: 0, end: 0 }, cancellation())
            .await,
        Err(CompactionError::Invalid(error)) if error.code == "UNBALANCED_TOOL_PAIR"
    ));
    assert_eq!(session.events().len(), 2);
}

#[tokio::test]
async fn successful_compaction_replaces_only_after_durable_summary() {
    let session = session("success").await;
    let first = user(&session, "one", "first").await;
    let second = user(&session, "two", "second").await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (service, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream("both messages summarized"),
        requests: Arc::clone(&requests),
        calls: Arc::new(AtomicUsize::new(0)),
    }));

    let result = service
        .compact_region(
            &session,
            CompactionRange { start: 0, end: 1 },
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(result.shadowed_event_seqs, vec![first, second]);
    assert_eq!(result.event_seqs.start, 2);
    assert_eq!(result.event_seqs.summary, 3);
    assert_eq!(result.event_seqs.replacement, 4);
    assert_eq!(result.event_seqs.end, 5);
    let events = session.events();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "user/message",
            "user/message",
            "compaction/start",
            "compaction/summary",
            "user/message",
            "compaction/end",
        ]
    );
    assert_eq!(
        events[4].surface_op,
        Some(SurfaceOp::Replace { start: 0, end: 2 })
    );
    assert_eq!(events[4].source_event_seqs, Some(vec![2, 3, first, second]));
    assert_eq!(session.surface().len(), 1);
    assert_eq!(session.derive_messages()[0].role, MessageRole::User);
    assert_eq!(
        lock(&requests)[0].purpose,
        Some(GeneratePurpose::Compaction)
    );
}

#[tokio::test]
async fn compaction_rejects_a_surface_changed_during_summary() {
    let session = session("stale-compaction").await;
    let first = user(&session, "one", "first").await;
    let second = user(&session, "two", "second").await;
    let (service, _registration) = service(Arc::new(InterleavingAdapter {
        chunks: text_stream("stale summary"),
        session: Arc::clone(&session),
    }));

    let error = service
        .compact_region(
            &session,
            CompactionRange { start: 0, end: 1 },
            cancellation(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        CompactionError::Session(SessionError::StaleSurface { expected, actual })
            if expected == vec![first, second] && actual == vec![first, second, 3]
    ));
    let events = session.events();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "user/message",
            "user/message",
            "compaction/start",
            "user/message",
            "compaction/summary",
            "compaction/end",
        ]
    );
    assert_eq!(
        events[5].data["error"],
        "surface changed before conditional append"
    );
    assert_eq!(
        session
            .surface()
            .iter()
            .map(|entry| entry.event_seq)
            .collect::<Vec<_>>(),
        vec![first, second, 3]
    );
    assert_eq!(session.derive_messages()[2].id.as_str(), "concurrent");
}

#[tokio::test]
async fn summary_failure_records_failed_end_without_changing_surface() {
    let session = session("failure").await;
    user(&session, "one", "first").await;
    user(&session, "two", "second").await;
    let before = session.derive_messages();
    let (service, _registration) = service(Arc::new(FailingAdapter));

    assert!(matches!(
        service
            .compact_region(&session, CompactionRange { start: 0, end: 1 }, cancellation())
            .await,
        Err(CompactionError::Llm(error)) if error.code == "SUMMARY_DOWN"
    ));
    assert_eq!(session.derive_messages(), before);
    let events = session.events();
    assert_eq!(events[2].event_type, "compaction/start");
    assert_eq!(events[3].event_type, "compaction/end");
    assert_eq!(
        events[3].data["error"],
        "LLM summarization failed: SUMMARY_DOWN: summary provider failed"
    );
}

#[tokio::test]
async fn cancellation_records_cancelled_end_without_changing_surface() {
    let session = session("cancel").await;
    user(&session, "one", "first").await;
    user(&session, "two", "second").await;
    let before = session.derive_messages();
    let (service, _registration) = service(Arc::new(PendingAdapter));
    let token = cancellation();
    let task_service = service.clone();
    let task_session = Arc::clone(&session);
    let task_token = token.clone();
    let task = tokio::spawn(async move {
        task_service
            .compact_region(
                &task_session,
                CompactionRange { start: 0, end: 1 },
                task_token,
            )
            .await
    });
    tokio::task::yield_now().await;
    token.cancel();

    assert!(matches!(
        task.await.unwrap(),
        Err(CompactionError::Cancelled)
    ));
    assert_eq!(session.derive_messages(), before);
    let events = session.events();
    assert_eq!(events[2].event_type, "compaction/start");
    assert_eq!(events[3].event_type, "compaction/end");
    assert_eq!(events[3].data["error"], "compaction was cancelled");
}

#[tokio::test]
async fn cancellation_during_summary_append_records_a_cancelled_end() {
    let persistence = Arc::new(BlockingAppendPersistence::new(3));
    let store = SessionStore::new(persistence.clone());
    let session = store
        .create(header("cancel-summary-append", None), cancellation())
        .await
        .unwrap();
    user(&session, "one", "first").await;
    user(&session, "two", "second").await;
    let before = session.derive_messages();
    let (service, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream("summary"),
        requests: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let token = cancellation();
    let task_service = service.clone();
    let task_session = Arc::clone(&session);
    let task_token = token.clone();
    let task = tokio::spawn(async move {
        task_service
            .compact_region(
                &task_session,
                CompactionRange { start: 0, end: 1 },
                task_token,
            )
            .await
    });
    persistence.entered.notified().await;
    token.cancel();
    persistence.release.notify_one();

    assert!(matches!(
        task.await.unwrap(),
        Err(CompactionError::Cancelled)
    ));
    assert_eq!(session.derive_messages(), before);
    let events = session.events();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "user/message",
            "user/message",
            "compaction/start",
            "compaction/end"
        ]
    );
    assert_eq!(events[3].data["error"], "compaction was cancelled");
}

#[tokio::test]
async fn cancellation_during_replacement_append_records_a_cancelled_end() {
    let persistence = Arc::new(BlockingAppendPersistence::new(4));
    let store = SessionStore::new(persistence.clone());
    let session = store
        .create(header("cancel-replacement-append", None), cancellation())
        .await
        .unwrap();
    user(&session, "one", "first").await;
    user(&session, "two", "second").await;
    let before = session.derive_messages();
    let (service, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream("summary"),
        requests: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let token = cancellation();
    let task_service = service.clone();
    let task_session = Arc::clone(&session);
    let task_token = token.clone();
    let task = tokio::spawn(async move {
        task_service
            .compact_region(
                &task_session,
                CompactionRange { start: 0, end: 1 },
                task_token,
            )
            .await
    });
    persistence.entered.notified().await;
    token.cancel();
    persistence.release.notify_one();

    assert!(matches!(
        task.await.unwrap(),
        Err(CompactionError::Cancelled)
    ));
    assert_eq!(session.derive_messages(), before);
    let events = session.events();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "user/message",
            "user/message",
            "compaction/start",
            "compaction/summary",
            "compaction/end",
        ]
    );
    assert_eq!(events[4].data["error"], "compaction was cancelled");
}

#[tokio::test]
async fn an_orphaned_seed_does_not_block_live_automatic_compaction() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let store = SessionStore::new(Arc::clone(&persistence));
    let seeded = SessionEvent {
        event_type: "tool/result".into(),
        seq: 0,
        time: 0,
        data: json!({
            "message": {
                "id": "orphan",
                "role": "user",
                "content": [{
                    "type": "tool-result",
                    "toolCallId": "lost",
                    "content": [{"type": "text", "text": "old"}],
                }],
                "source": {"kind": "tool", "callId": "lost"},
            },
        }),
        ignorable: None,
        source_event_seqs: None,
        surface_op: Some(SurfaceOp::Append),
    };
    let session = store
        .create_seeded(header("seed", Some(1)), vec![seeded], cancellation())
        .await
        .unwrap();
    user(&session, "one", "first").await;
    user(&session, "two", "second").await;
    let (service, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream("live only"),
        requests: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    }));

    assert!(matches!(
        service.compact_now(&session, cancellation()).await.unwrap(),
        CompactionOutcome::Compacted(_)
    ));
    assert_eq!(session.surface().len(), 2);
}

#[tokio::test]
async fn tool_result_pruning_uses_unicode_codepoints_and_a_durable_replacement() {
    let session = session("prune").await;
    user(&session, "one", "start").await;
    let call = assistant_call(&session, "call-a").await;
    let source = tool_result(&session, "call-a", "aé🙂b", call).await;
    let (service, _registration) = service(Arc::new(FailingAdapter));

    let outcome = service
        .prune_tool_result(&session, source, 3, cancellation())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        ToolResultPruneOutcome::Pruned(ref result)
            if result.original_codepoints == 4 && result.retained_codepoints == 3
    ));
    let events = session.events();
    assert_eq!(events[3].event_type, "tool/result");
    assert_eq!(events[3].source_event_seqs, Some(vec![source]));
    assert_eq!(
        events[3].surface_op,
        Some(SurfaceOp::Replace { start: 2, end: 3 })
    );
    let message = &session.derive_messages()[2];
    assert_eq!(
        message.content,
        vec![ContentBlock::ToolResult {
            tool_call_id: ToolCallId::from("call-a"),
            content: vec![ContentBlock::Text {
                text: "aé…".into()
            }],
            is_error: None,
        }]
    );
}

#[tokio::test]
async fn pruning_rejects_a_source_replaced_before_its_commit() {
    let persistence = Arc::new(BlockingAppendPersistence::new(3));
    let store = SessionStore::new(persistence.clone());
    let session = store
        .create(header("stale-prune", None), cancellation())
        .await
        .unwrap();
    user(&session, "one", "start").await;
    let call = assistant_call(&session, "call-a").await;
    let source = tool_result(&session, "call-a", "original", call).await;
    let (service, _registration) = service(Arc::new(FailingAdapter));

    let concurrent_session = Arc::clone(&session);
    let concurrent = tokio::spawn(async move {
        append(
            concurrent_session.as_ref(),
            "tool/result",
            json!({
                "message": {
                    "id": "newer-result",
                    "role": "user",
                    "content": [{
                        "type": "tool-result",
                        "toolCallId": "call-a",
                        "content": [{"type": "text", "text": "newer"}],
                    }],
                    "source": {"kind": "tool", "callId": "call-a"},
                },
            }),
            Some(vec![source]),
            SurfaceOp::Replace { start: 2, end: 3 },
        )
        .await
    });
    persistence.entered.notified().await;

    let mut prune = Box::pin(service.prune_tool_result(&session, source, 3, cancellation()));
    assert!(matches!(futures_util::poll!(prune.as_mut()), Poll::Pending));
    persistence.release.notify_one();
    assert_eq!(concurrent.await.unwrap(), 3);

    let error = prune.await.unwrap_err();
    assert!(matches!(
        error,
        CompactionError::Session(SessionError::StaleSurface { expected, actual })
            if expected == vec![0, call, source] && actual == vec![0, call, 3]
    ));
    let events = session.events();
    assert_eq!(events.len(), 4);
    assert_eq!(events[3].source_event_seqs, Some(vec![source]));
    assert_eq!(
        events[3].surface_op,
        Some(SurfaceOp::Replace { start: 2, end: 3 })
    );
    assert_eq!(
        session
            .surface()
            .iter()
            .map(|entry| entry.event_seq)
            .collect::<Vec<_>>(),
        vec![0, call, 3]
    );
    assert_eq!(session.derive_messages()[2].id.as_str(), "newer-result");
}

#[tokio::test]
async fn replay_reconstructs_the_durable_compaction_surface() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let writer = SessionStore::new(Arc::clone(&persistence));
    let session = writer
        .create(header("replay", None), cancellation())
        .await
        .unwrap();
    user(&session, "one", "first").await;
    user(&session, "two", "second").await;
    let (service, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream("replayed summary"),
        requests: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    service
        .compact_region(
            &session,
            CompactionRange { start: 0, end: 1 },
            cancellation(),
        )
        .await
        .unwrap();
    let expected = session.derive_messages();

    let reader = SessionStore::new(persistence);
    let restored = reader
        .restore(
            &SessionId::from("replay"),
            tessivum::session::RestoreMode::Cold,
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(restored.derive_messages(), expected);
}

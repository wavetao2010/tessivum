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

struct FailSecondSummary {
    inner: StaticAdapter,
    failed: AtomicBool,
}

#[async_trait]
impl LlmAdapter for FailSecondSummary {
    async fn generate(
        &self,
        request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        if self.inner.calls.load(Ordering::SeqCst) == 1 && !self.failed.swap(true, Ordering::SeqCst)
        {
            return Err(TessivumError::new(
                "SUMMARY_DOWN",
                "second summary failed",
                "llm",
                serde_json::Value::Null,
            ));
        }
        self.inner.generate(request, cancellation).await
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
    fail_replacement: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl BlockingAppendPersistence {
    fn new(blocked_seq: u64) -> Self {
        Self {
            inner: MemorySessionPersistence::new(),
            blocked_seq,
            blocked: AtomicBool::new(false),
            fail_replacement: AtomicBool::new(false),
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
        if matches!(event.surface_op, Some(SurfaceOp::Replace { .. }))
            && self.fail_replacement.swap(false, Ordering::SeqCst)
        {
            return Err(SessionError::Protocol(TessivumError::new(
                "INJECTED_APPEND_FAILURE",
                "replacement write failed",
                "persistence",
                serde_json::Value::Null,
            )));
        }
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
    service_with_config(adapter, CompactionConfig::default())
}

fn service_with_config(
    adapter: Arc<dyn LlmAdapter>,
    overrides: CompactionConfig,
) -> (CompactionService, LlmProviderRegistration) {
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
                ..overrides
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

    assert!(matches!(
        service.compact_now(&session, cancellation()).await.unwrap(),
        CompactionOutcome::Noop { .. }
    ));
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
    let first = user(&session, "one", &"first ".repeat(64)).await;
    let second = user(&session, "two", &"second ".repeat(64)).await;
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
    let first = user(&session, "one", &"first ".repeat(64)).await;
    let second = user(&session, "two", &"second ".repeat(64)).await;
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
    user(&session, "one", &"first ".repeat(64)).await;
    user(&session, "two", &"second ".repeat(64)).await;
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
    user(&session, "one", &"first ".repeat(64)).await;
    user(&session, "two", &"second ".repeat(64)).await;
    let (service, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream("live only"),
        requests: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    }));

    assert!(matches!(
        service.compact_now(&session, cancellation()).await.unwrap(),
        CompactionOutcome::Compacted(_)
    ));
    assert_eq!(session.surface()[0].message.id.as_str(), "orphan");
    assert_eq!(session.derive_messages().last().unwrap().id.as_str(), "two");
}

#[tokio::test]
async fn tool_result_pruning_uses_unicode_codepoints_and_a_durable_replacement() {
    let session = session("prune").await;
    user(&session, "one", "start").await;
    let call = assistant_call(&session, "call-a").await;
    let original = "aé🙂b\"\\\n".repeat(200);
    let source = tool_result(&session, "call-a", &original, call).await;
    let (service, _registration) = service(Arc::new(FailingAdapter));

    let outcome = service
        .prune_tool_result(&session, source, 400, cancellation())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        ToolResultPruneOutcome::Pruned(ref result)
            if result.original_codepoints == original.chars().count() as u64
    ));
    let events = session.events();
    assert_eq!(events[3].event_type, "tool/result");
    assert_eq!(events[3].source_event_seqs, Some(vec![source]));
    assert_eq!(
        events[3].surface_op,
        Some(SurfaceOp::Replace { start: 2, end: 3 })
    );
    let message = &session.derive_messages()[2];
    let serialized = serde_json::to_string(message).unwrap();
    assert!(serialized.chars().count() <= 400);
    assert!(
        serialized.len() > 400,
        "the budget is Unicode codepoints, not bytes"
    );
    let [ContentBlock::ToolResult {
        tool_call_id,
        content,
        is_error,
    }] = message.content.as_slice()
    else {
        panic!("tool result must remain paired")
    };
    assert_eq!(tool_call_id.as_str(), "call-a");
    assert_eq!(*is_error, None);
    let [ContentBlock::Text { text }] = content.as_slice() else {
        panic!("text result expected")
    };
    assert!(text.contains(&format!("event {source}")));
    assert_eq!(
        events[source as usize].data["message"]["content"][0]["content"][0]["text"],
        original
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
    let source = tool_result(&session, "call-a", &"original".repeat(200), call).await;
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

    let mut prune = Box::pin(service.prune_tool_result(&session, source, 400, cancellation()));
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
    user(&session, "one", &"first ".repeat(64)).await;
    user(&session, "two", &"second ".repeat(64)).await;
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

#[tokio::test]
async fn request_recovery_compacts_old_history_and_keeps_latest_user_verbatim() {
    let session = session("request-recovery").await;
    for index in 0..6 {
        user(&session, &format!("old-{index}"), &"x".repeat(256)).await;
    }
    let latest = session.derive_messages().last().cloned().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (service, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream("short summary"),
        requests: Arc::clone(&requests),
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let request = GenerateRequest {
        provider: "test".into(),
        model: "test".into(),
        reasoning_effort: None,
        messages: session.derive_messages(),
        system: None,
        tools: None,
        temperature: None,
        max_tokens: Some(16),
        stop: None,
        session_id: Some(session.id()),
        purpose: None,
    };
    let outcome = service
        .compact_for_request(
            &session,
            CompactionTrigger::ContextOverflow,
            &request,
            Some(2_000),
            cancellation(),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, CompactionOutcome::Compacted(_)));
    let messages = session.derive_messages();
    assert_eq!(messages.last(), Some(&latest));
    assert!(messages.len() < 6);
    assert!(!lock(&requests).is_empty());
}

#[tokio::test]
async fn equal_or_larger_summary_is_rejected_without_surface_replacement() {
    let session = session("no-progress").await;
    let first = user(&session, "one", &"a".repeat(256)).await;
    let second = user(&session, "two", &"b".repeat(256)).await;
    let before = session.derive_messages();
    let (service, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream(&"z".repeat(2_000)),
        requests: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let error = service
        .compact_region(
            &session,
            CompactionRange { start: 0, end: 1 },
            cancellation(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, CompactionError::Invalid(value) if value.code == "COMPACTION_NO_PROGRESS")
    );
    assert_eq!(session.derive_messages(), before);
    assert_eq!(
        session
            .surface()
            .iter()
            .map(|entry| entry.event_seq)
            .collect::<Vec<_>>(),
        vec![first, second]
    );
}

#[tokio::test]
async fn request_recovery_commits_multiple_bounded_chunks() {
    let session = session("request-recovery-chunks").await;
    for index in 0..12 {
        user(&session, &format!("chunk-{index}"), &"history ".repeat(32)).await;
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let (service, _registration) = service_with_config(
        Arc::new(StaticAdapter {
            chunks: text_stream("small"),
            requests: Arc::clone(&requests),
            calls: Arc::clone(&calls),
        }),
        CompactionConfig {
            max_surface_messages: 4,
            max_input_codepoints: 4_096,
            max_summary_codepoints: 128,
            ..CompactionConfig::default()
        },
    );
    let request = GenerateRequest {
        provider: "test".into(),
        model: "test".into(),
        reasoning_effort: None,
        messages: session.derive_messages(),
        system: None,
        tools: None,
        temperature: None,
        max_tokens: Some(16),
        stop: None,
        session_id: Some(session.id()),
        purpose: None,
    };
    assert!(matches!(
        service
            .compact_for_request(
                &session,
                CompactionTrigger::ContextOverflow,
                &request,
                None,
                cancellation()
            )
            .await
            .unwrap(),
        CompactionOutcome::Compacted(_)
    ));
    assert!(calls.load(Ordering::SeqCst) > 1);
    assert!(lock(&requests)
        .iter()
        .all(|request| request.messages.len() <= 4));
    assert!(session.surface().len() < 12);
}

#[tokio::test]
async fn oversized_live_history_recovers_after_restart_without_replaying_originals() {
    for unicode in [false, true] {
        let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
        let store = SessionStore::new(persistence.clone());
        let original = store
            .create(header("long-history", None), cancellation())
            .await
            .unwrap();
        let count = if unicode { 12 } else { 515 };
        let text = if unicode {
            "约束🙂\"\\\n".repeat(1_100)
        } else {
            "old".into()
        };
        for index in 0..count {
            user(&original, &format!("history-{index}"), &text).await;
        }
        user(&original, "current", "Preserve this current task exactly.").await;
        let latest = original.derive_messages().last().cloned().unwrap();
        let raw = original.events();
        let reader = SessionStore::new(persistence);
        let recovered = reader
            .restore(
                &original.id(),
                tessivum::session::RestoreMode::Cold,
                cancellation(),
            )
            .await
            .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (compactor, _registration) = service(Arc::new(StaticAdapter {
            chunks: text_stream("Constraint A must remain in force."),
            requests: requests.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        assert!(matches!(
            compactor
                .compact_now(&recovered, cancellation())
                .await
                .unwrap(),
            CompactionOutcome::Compacted(_)
        ));
        assert_eq!(&recovered.events()[..raw.len()], raw.as_slice());
        assert_eq!(recovered.derive_messages().last(), Some(&latest));
        assert!(recovered.surface().len() <= 384);
        assert!(
            recovered
                .derive_messages()
                .iter()
                .map(|message| serde_json::to_string(message).unwrap().chars().count())
                .sum::<usize>()
                <= 49_152
        );
        let first_calls = lock(&requests).len();
        for index in 0..30 {
            user(
                &recovered,
                &format!("new-{index}"),
                &"new context ".repeat(200),
            )
            .await;
        }
        compactor
            .compact_now(&recovered, cancellation())
            .await
            .unwrap();
        let calls = lock(&requests);
        assert!(calls.iter().all(|request| request.messages.len() <= 512
            && request
                .messages
                .iter()
                .map(|message| serde_json::to_string(message).unwrap().chars().count())
                .sum::<usize>()
                <= 65_536));
        assert!(calls[first_calls..].iter().any(|request| request.messages.iter().any(|message|
            message.content.iter().any(|block| matches!(block, ContentBlock::Text { text } if text.contains("Constraint A"))))));
        let mut original_ids = std::collections::BTreeSet::new();
        for message in calls
            .iter()
            .flat_map(|request| &request.messages)
            .filter(|message| message.id.as_str().starts_with("history-"))
        {
            assert!(
                original_ids.insert(message.id.clone()),
                "original history must not be summarized twice"
            );
        }
    }
}

#[tokio::test]
async fn summary_batch_limits_do_not_reject_history_that_fits_the_main_model() {
    let store = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let parent = store
        .create(header("large-parent", None), cancellation())
        .await
        .unwrap();
    for index in 0..520 {
        user(&parent, &format!("history-{index}"), &"history ".repeat(16)).await;
    }
    let seed = parent.events();
    let child = store
        .create_seeded(
            header("large-child", Some(seed.len() as u64)),
            seed,
            cancellation(),
        )
        .await
        .unwrap();
    let (compactor, _registration) = service(Arc::new(FailingAdapter));
    for session in [&parent, &child] {
        user(
            session,
            "current",
            "Continue without modifying the existing history.",
        )
        .await;
        let before = session.events();
        let request = GenerateRequest {
            provider: "test".into(),
            model: "large-main".into(),
            reasoning_effort: None,
            messages: session.derive_messages(),
            system: None,
            tools: None,
            temperature: None,
            max_tokens: Some(128),
            stop: None,
            session_id: Some(session.id()),
            purpose: None,
        };
        assert!(matches!(
            compactor
                .compact_for_request(
                    session,
                    CompactionTrigger::Pressure,
                    &request,
                    Some(256_000),
                    cancellation()
                )
                .await
                .unwrap(),
            CompactionOutcome::Noop { .. }
        ));
        assert_eq!(session.events(), before);
    }
}

#[tokio::test]
async fn model_windows_are_separate_and_protected_input_cannot_fake_recovery() {
    let session = session("model-budgets").await;
    for index in 0..20 {
        user(&session, &format!("old-{index}"), &"历史".repeat(150)).await;
    }
    user(&session, "current", "keep this request").await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (compactor, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream("Earlier constraints retained."),
        requests: requests.clone(),
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let compactor = compactor.with_context_window_resolver(Arc::new(|_, model| {
        if model == "test" {
            Some(4_096)
        } else {
            Some(200_000)
        }
    }));
    let mut request = GenerateRequest {
        provider: "test".into(),
        model: "large-main".into(),
        reasoning_effort: None,
        messages: session.derive_messages(),
        system: None,
        tools: None,
        temperature: None,
        max_tokens: Some(128),
        stop: None,
        session_id: Some(session.id()),
        purpose: None,
    };
    compactor
        .compact_for_request(
            &session,
            CompactionTrigger::ContextOverflow,
            &request,
            Some(200_000),
            cancellation(),
        )
        .await
        .unwrap();
    assert!(lock(&requests)
        .iter()
        .all(|request| serde_json::to_string(request).unwrap().len() + 128 <= 4_096));
    let before = session.derive_messages();
    request.messages = before.clone();
    request.model = "tiny-main".into();
    request.max_tokens = Some(128);
    let error = compactor
        .compact_for_request(
            &session,
            CompactionTrigger::Pressure,
            &request,
            Some(64),
            cancellation(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "COMPACTION_PROTECTED_INPUT_TOO_LARGE");
    assert!(session
        .derive_messages()
        .iter()
        .any(|message| message.id.as_str() == "current"));
}

#[tokio::test]
async fn partial_recovery_survives_failure_and_cold_restart() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let writer = SessionStore::new(persistence.clone());
    let session = writer
        .create(header("partial-recovery", None), cancellation())
        .await
        .unwrap();
    for index in 0..12 {
        user(&session, &format!("old-{index}"), &"history ".repeat(64)).await;
    }
    let (compactor, _registration) = service_with_config(
        Arc::new(FailSecondSummary {
            inner: StaticAdapter {
                chunks: text_stream("A survives."),
                requests: Arc::new(Mutex::new(Vec::new())),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            failed: AtomicBool::new(false),
        }),
        CompactionConfig {
            max_surface_messages: 4,
            ..CompactionConfig::default()
        },
    );
    assert_eq!(
        compactor
            .compact_now(&session, cancellation())
            .await
            .unwrap_err()
            .code(),
        "SUMMARY_DOWN"
    );
    let partial = session.derive_messages();
    assert!(partial.len() < 12 && partial.len() > 3);
    let reader = SessionStore::new(persistence);
    let restored = reader
        .restore(
            &session.id(),
            tessivum::session::RestoreMode::Cold,
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(restored.derive_messages(), partial);
    compactor
        .compact_now(&restored, cancellation())
        .await
        .unwrap();
    assert!(restored.surface().len() <= 3);
    assert_eq!(
        restored.derive_messages().last().unwrap().id.as_str(),
        "old-11"
    );
    assert!(restored
        .events()
        .iter()
        .any(|event| event.event_type == "compaction/end" && event.data.get("error").is_some()));
}

#[tokio::test]
async fn long_turn_recovery_keeps_current_request_parallel_pairs_and_pending_calls() {
    let session = session("parallel-turn-recovery").await;
    user(
        &session,
        "current",
        "Keep this original task throughout the long turn.",
    )
    .await;
    let current = session.derive_messages()[0].clone();
    for index in 0..8 {
        let a = format!("a-{index}");
        let b = format!("b-{index}");
        let call = append(
            &session,
            "assistant/message",
            json!({"message": {
                "id": format!("parallel-{index}"), "role": "assistant",
                "content": [
                    {"type":"tool-call","id":a,"name":"probe","arguments":"{}"},
                    {"type":"tool-call","id":b,"name":"probe","arguments":"{}"}
                ], "source":{"kind":"model","provider":"test","model":"test"}
            }}),
            None,
            SurfaceOp::Append,
        )
        .await;
        tool_result(&session, &b, &"result B ".repeat(70), call).await;
        tool_result(&session, &a, &"result A ".repeat(70), call).await;
    }
    assistant_call(&session, "pending").await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (compactor, _registration) = service_with_config(
        Arc::new(StaticAdapter {
            chunks: text_stream("Completed tools summarized."),
            requests: requests.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        CompactionConfig {
            max_surface_messages: 8,
            max_input_codepoints: 6_000,
            ..CompactionConfig::default()
        },
    );
    compactor
        .compact_now(&session, cancellation())
        .await
        .unwrap();
    let messages = session.derive_messages();
    assert_eq!(messages.first(), Some(&current));
    assert!(
        matches!(&messages.last().unwrap().content[0], ContentBlock::ToolCall { id, .. } if id == &ToolCallId::from("pending"))
    );
    assert!(messages.len() <= 6);
    for request in lock(&requests).iter() {
        let mut calls = std::collections::BTreeSet::new();
        let mut results = std::collections::BTreeSet::new();
        for block in request.messages.iter().flat_map(|message| &message.content) {
            match block {
                ContentBlock::ToolCall { id, .. } => {
                    calls.insert(id.clone());
                }
                ContentBlock::ToolResult { tool_call_id, .. } => {
                    results.insert(tool_call_id.clone());
                }
                _ => {}
            }
        }
        assert_eq!(calls, results);
        assert!(!calls.contains(&ToolCallId::from("pending")));
    }
}

#[tokio::test]
async fn automatic_recovery_prunes_a_giant_recent_text_result_without_losing_raw_data() {
    let session = session("giant-result").await;
    user(&session, "current", "inspect the result").await;
    let call = assistant_call(&session, "giant").await;
    let original = "巨大🙂\"\\\n".repeat(20_000);
    let source = tool_result(&session, "giant", &original, call).await;
    let (compactor, _registration) = service(Arc::new(FailingAdapter));
    assert!(matches!(
        compactor
            .compact_now(&session, cancellation())
            .await
            .unwrap(),
        CompactionOutcome::Pruned(_)
    ));
    let messages = session.derive_messages();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].id.as_str(), "current");
    assert!(serde_json::to_string(&messages[2]).unwrap().chars().count() <= 16_384);
    assert_eq!(
        session.events()[source as usize].data["message"]["content"][0]["content"][0]["text"],
        original
    );
}

#[tokio::test]
async fn failed_replacement_write_keeps_history_replayable_and_retryable() {
    let persistence = Arc::new(BlockingAppendPersistence::new(u64::MAX));
    let store = SessionStore::new(persistence.clone());
    let session = store
        .create(header("failed-replacement", None), cancellation())
        .await
        .unwrap();
    user(&session, "old", &"durable history ".repeat(256)).await;
    user(&session, "current", "retain current request").await;
    let before = session.derive_messages();
    let (compactor, _registration) = service(Arc::new(StaticAdapter {
        chunks: text_stream("Durable constraints retained."),
        requests: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    persistence.fail_replacement.store(true, Ordering::SeqCst);
    assert_eq!(
        compactor
            .compact_now(&session, cancellation())
            .await
            .unwrap_err()
            .code(),
        "INJECTED_APPEND_FAILURE"
    );
    assert_eq!(session.derive_messages(), before);
    let recovered = SessionStore::new(persistence)
        .restore(
            &session.id(),
            tessivum::session::RestoreMode::Cold,
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(recovered.derive_messages(), before);
    assert!(matches!(
        compactor
            .compact_now(&recovered, cancellation())
            .await
            .unwrap(),
        CompactionOutcome::Compacted(_)
    ));
    assert_eq!(recovered.derive_messages().last(), before.last());
}

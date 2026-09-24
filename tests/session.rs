use std::{fs, sync::Arc};

use serde_json::json;
use tessivum::persistence_jsonl::JsonlSessionPersistence;
use tessivum::{
    protocol::{SessionEvent, SessionHeader, SessionId, SurfaceOp, SESSION_FORMAT_VERSION},
    session::{
        session_service_key, MemorySessionPersistence, RestoreMode, SessionError,
        SessionPersistence, SessionStore,
    },
};
use tessivum_core::ContextHandle;
use uuid::Uuid;

fn cancellation() -> tessivum_core::CancellationToken {
    ContextHandle::root().scope().cancellation()
}

fn allocator_rss_bytes() -> Option<u64> {
    #[cfg(unix)]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        // ru_maxrss is bytes on macOS and KiB on Linux.
        let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if status == 0 {
            let usage = unsafe { usage.assume_init() };
            let scale = if cfg!(target_os = "linux") { 1024 } else { 1 };
            return u64::try_from(usage.ru_maxrss).ok().map(|rss| rss * scale);
        }
    }
    None
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

fn event(
    event_type: &str,
    seq: u64,
    data: serde_json::Value,
    source_event_seqs: Option<Vec<u64>>,
    surface_op: Option<SurfaceOp>,
) -> SessionEvent {
    SessionEvent {
        event_type: event_type.into(),
        seq,
        time: 0,
        data,
        ignorable: None,
        source_event_seqs,
        surface_op,
    }
}

fn user_event(seq: u64, id: &str, text: &str, surface_op: SurfaceOp) -> SessionEvent {
    event(
        "user/message",
        seq,
        json!({
            "id": id,
            "role": "user",
            "content": [{"type": "text", "text": text}],
            "source": {"kind": "user"},
        }),
        None,
        Some(surface_op),
    )
}

#[tokio::test]
async fn user_messages_require_direct_payloads() {
    let store = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let session = store
        .create(header("direct", None), cancellation())
        .await
        .unwrap();
    let direct = json!({
        "id": "message-1",
        "role": "user",
        "content": [{"type": "text", "text": "hello"}],
        "source": {"kind": "user"},
    });
    session
        .append(
            event(
                "user/message",
                0,
                direct.clone(),
                None,
                Some(SurfaceOp::Append),
            ),
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(session.events().unwrap()[0].data, direct);
    assert_eq!(session.derive_messages()[0].id.as_str(), "message-1");

    let wrapped = event(
        "user/message",
        1,
        json!({"message": direct}),
        None,
        Some(SurfaceOp::Append),
    );
    assert!(matches!(
        session.append(wrapped, cancellation()).await,
        Err(SessionError::InvalidSurfaceMessage)
    ));
    let wrong_role = event(
        "user/message",
        1,
        json!({
            "id": "message-2",
            "role": "assistant",
            "content": [{"type": "text", "text": "wrong"}],
            "source": {"kind": "user"},
        }),
        None,
        Some(SurfaceOp::Append),
    );
    assert!(matches!(
        session.append(wrong_role, cancellation()).await,
        Err(SessionError::InvalidSurfaceRole)
    ));
}

#[tokio::test]
async fn create_append_and_derive_messages() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let store = SessionStore::new(persistence);
    let session = store
        .create(header("create", None), cancellation())
        .await
        .unwrap();
    assert!(matches!(
        store.create(header("create", None), cancellation()).await,
        Err(SessionError::DuplicateLive(_))
    ));

    session
        .append(
            user_event(0, "message-1", "hello", SurfaceOp::Append),
            cancellation(),
        )
        .await
        .unwrap();

    assert_eq!(session.header().id.as_str(), "create");
    assert_eq!(session.next_seq().unwrap(), 1);
    assert_eq!(session.events().unwrap().len(), 1);
    assert_eq!(session.derive_messages()[0].id.as_str(), "message-1");
}

#[tokio::test]
async fn gaps_do_not_admit_or_persist_events() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let store = SessionStore::new(Arc::clone(&persistence));
    let session = store
        .create(header("gaps", None), cancellation())
        .await
        .unwrap();

    let error = session
        .append(
            user_event(1, "message-1", "hello", SurfaceOp::Append),
            cancellation(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, SessionError::SequenceGap { .. }));
    assert!(session.events().unwrap().is_empty());
    assert_eq!(
        persistence
            .inspect(&SessionId::from("gaps"), cancellation())
            .await
            .unwrap()
            .unwrap()
            .event_count,
        0
    );
}

#[tokio::test]
async fn seed_prefix_is_separate_from_live_events() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let store = SessionStore::new(persistence);
    let seed = vec![user_event(0, "seed-message", "seed", SurfaceOp::Append)];
    let session = store
        .create_seeded(header("seed", Some(1)), seed, cancellation())
        .await
        .unwrap();

    session
        .append(
            event("session/end-seed", 1, json!({}), None, None),
            cancellation(),
        )
        .await
        .unwrap();
    session
        .append(
            user_event(2, "live-message", "live", SurfaceOp::Append),
            cancellation(),
        )
        .await
        .unwrap();

    assert_eq!(session.seed_events().unwrap().len(), 1);
    assert_eq!(session.live_events().unwrap().len(), 2);
    assert_eq!(session.derive_messages().len(), 2);
}

#[tokio::test]
async fn unknown_required_events_reject_but_ignorable_events_are_retained_off_surface() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let store = SessionStore::new(persistence);
    let session = store
        .create(header("unknown", None), cancellation())
        .await
        .unwrap();

    let required = event("future/event", 0, json!({}), None, None);
    assert!(matches!(
        session.append(required, cancellation()).await,
        Err(SessionError::Protocol(_))
    ));

    let mut ignorable = event("future/event", 0, json!({"kept": true}), None, None);
    ignorable.ignorable = Some(true);
    session.append(ignorable, cancellation()).await.unwrap();

    assert_eq!(session.events().unwrap().len(), 1);
    assert!(session.surface().is_empty());
}

#[tokio::test]
async fn surface_replacement_preserves_source_event_sequences() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let store = SessionStore::new(persistence);
    let session = store
        .create(header("surface", None), cancellation())
        .await
        .unwrap();

    session
        .append(
            user_event(0, "message-a", "old", SurfaceOp::Append),
            cancellation(),
        )
        .await
        .unwrap();
    let mut replacement = user_event(
        1,
        "message-b",
        "new",
        SurfaceOp::Replace { start: 0, end: 1 },
    );
    replacement.source_event_seqs = Some(vec![0]);
    session
        .append_if_surface(replacement, &[0], cancellation())
        .await
        .unwrap();

    let surface = session.surface();
    assert_eq!(surface.len(), 1);
    assert_eq!(surface[0].message.id.as_str(), "message-b");
    assert_eq!(surface[0].source_event_seqs, Some(vec![0]));
}

#[tokio::test]
async fn conditional_surface_append_rejects_a_stale_vector_without_writing() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let store = SessionStore::new(persistence.clone());
    let session = store
        .create(header("conditional-surface", None), cancellation())
        .await
        .unwrap();
    session
        .append(
            user_event(0, "message-a", "old", SurfaceOp::Append),
            cancellation(),
        )
        .await
        .unwrap();
    let expected = vec![0];
    session
        .append(
            user_event(1, "message-b", "newer", SurfaceOp::Append),
            cancellation(),
        )
        .await
        .unwrap();
    let mut replacement = user_event(
        2,
        "message-c",
        "must not be admitted",
        SurfaceOp::Replace { start: 0, end: 1 },
    );
    replacement.source_event_seqs = Some(vec![0]);

    assert_eq!(
        session
            .append_if_surface(replacement, &expected, cancellation())
            .await
            .unwrap_err(),
        SessionError::StaleSurface {
            expected,
            actual: vec![0, 1],
        }
    );
    assert_eq!(
        session
            .surface()
            .iter()
            .map(|entry| entry.event_seq)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(session.events().unwrap().len(), 2);
    assert_eq!(
        persistence
            .inspect(&SessionId::from("conditional-surface"), cancellation())
            .await
            .unwrap()
            .unwrap()
            .event_count,
        2
    );
}

#[tokio::test]
async fn cold_restore_repairs_one_orphan_but_live_restore_rejects_it() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let writer = SessionStore::new(Arc::clone(&persistence));
    let session = writer
        .create(header("orphan", None), cancellation())
        .await
        .unwrap();
    session
        .append(
            event("turn/start", 0, json!({"turn": 7}), None, None),
            cancellation(),
        )
        .await
        .unwrap();

    let reader = SessionStore::new(Arc::clone(&persistence));
    assert!(matches!(
        reader
            .restore(
                &SessionId::from("orphan"),
                RestoreMode::Live,
                cancellation()
            )
            .await,
        Err(SessionError::OrphanTurn)
    ));

    let restored = reader
        .restore(
            &SessionId::from("orphan"),
            RestoreMode::Cold,
            cancellation(),
        )
        .await
        .unwrap();
    let repaired = restored.events().unwrap();
    assert_eq!(repaired.len(), 2);
    assert_eq!(repaired[1].event_type, "turn/end");
    assert_eq!(repaired[1].data["reason"]["kind"], "interrupted");
    assert_eq!(repaired[1].data["synthetic"], true);
}

#[tokio::test]
async fn cold_restore_closes_each_unsettled_tool_before_its_step_and_turn() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let writer = SessionStore::new(Arc::clone(&persistence));
    let session = writer
        .create(header("tool-orphan", None), cancellation())
        .await
        .unwrap();
    for (event_type, data, source_event_seqs, surface_op) in [
        ("turn/start", json!({"turn": 1}), None, None),
        ("step/start", json!({"turn": 1, "step": 1}), None, None),
        (
            "assistant/message",
            json!({
                "turn": 1,
                "step": 1,
                "message": {
                    "id": "assistant",
                    "role": "assistant",
                    "content": [{"type": "tool-call", "id": "call-1", "name": "write", "arguments": "{}"}],
                    "source": {"kind": "model", "provider": "test", "model": "model"},
                },
            }),
            Some(vec![0]),
            Some(SurfaceOp::Append),
        ),
        (
            "tool/call",
            json!({"turn": 1, "step": 1, "callId": "call-1", "name": "write", "arguments": "{}"}),
            None,
            None,
        ),
    ] {
        session
            .append(
                event(
                    event_type,
                    session.next_seq().unwrap(),
                    data,
                    source_event_seqs,
                    surface_op,
                ),
                cancellation(),
            )
            .await
            .unwrap();
    }

    let restored = SessionStore::new(persistence)
        .restore(
            &SessionId::from("tool-orphan"),
            RestoreMode::Cold,
            cancellation(),
        )
        .await
        .unwrap();
    let events = restored.events().unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "turn/start",
            "step/start",
            "assistant/message",
            "tool/call",
            "tool/result",
            "step/end",
            "turn/end"
        ]
    );
    assert_eq!(events[4].data["error"]["code"], "TOOL_OUTCOME_UNKNOWN");
    assert_eq!(events[4].source_event_seqs, Some(vec![3]));
    assert_eq!(events[6].data["reason"]["kind"], "interrupted");
}

#[tokio::test]
async fn subscribers_observe_admitted_live_events_and_flush_delegates() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let store = SessionStore::new(Arc::clone(&persistence));
    let session = store
        .create(header("updates", None), cancellation())
        .await
        .unwrap();
    let mut updates = session.subscribe();

    session
        .append(
            user_event(0, "message", "observe", SurfaceOp::Append),
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(updates.recv().await.unwrap().seq, 0);

    session.flush(cancellation()).await.unwrap();
    assert_eq!(
        persistence
            .inspect(&SessionId::from("updates"), cancellation())
            .await
            .unwrap()
            .unwrap()
            .flush_count,
        1
    );
}

#[tokio::test]
async fn append_next_serializes_concurrent_sequence_allocation() {
    let store = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let session = store
        .create(header("atomic-next", None), cancellation())
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        session.append_next(
            |seq| user_event(seq, "first", "first", SurfaceOp::Append),
            cancellation(),
        ),
        session.append_next(
            |seq| user_event(seq, "second", "second", SurfaceOp::Append),
            cancellation(),
        )
    );
    let mut sequences = [first.unwrap(), second.unwrap()];
    sequences.sort_unstable();
    assert_eq!(sequences, [0, 1]);
    assert_eq!(
        session
            .events()
            .unwrap()
            .into_iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn session_store_publishes_through_context_handle() {
    let context = ContextHandle::root();
    let store = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let handle = context.provide(session_service_key(), store).unwrap();

    assert!(handle.is_current());
    assert_eq!(handle.key().diagnostic_key(), "harness.sessions@1");
    assert!(handle.with(SessionStore::list).unwrap().is_empty());
}

#[tokio::test]
#[ignore = "200k-event disk stress; run explicitly for long-session evidence"]
async fn disk_session_pages_200k_events_with_bounded_resident_history() {
    let root = std::env::temp_dir().join(format!("tessivum-session-stress-{}", Uuid::new_v4()));
    let persistence = Arc::new(JsonlSessionPersistence::new(&root));
    let id = SessionId::from("two-hundred-thousand");
    let mut head = header(id.as_str(), None);
    head.version = SESSION_FORMAT_VERSION;

    // Bulk-create the fixture without paying 200k ordinary append/fsync costs.
    fs::create_dir_all(&root).unwrap();
    let mut fixture = serde_json::to_string(&json!({
        "type": "session",
        "version": head.version,
        "id": id.as_str(),
        "createdAt": 0,
    }))
    .unwrap();
    fixture.push('\n');
    for seq in 0..200_000u64 {
        let mut record = event("future/event", seq, json!({"seq": seq}), None, None);
        record.ignorable = Some(true);
        fixture.push_str(&serde_json::to_string(&record).unwrap());
        fixture.push('\n');
    }
    fs::write(persistence.raw_path(&id), fixture).unwrap();

    let store = SessionStore::new(persistence.clone());
    let session = store
        .restore(&id, RestoreMode::Cold, cancellation())
        .await
        .unwrap();
    assert_eq!(session.event_count(), 200_000);
    assert!(session.resident_event_count() <= 256);
    assert!(session.surface().is_empty());
    assert_eq!(
        session
            .read_events(0, 3)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        session
            .read_events(199_997, 3)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect::<Vec<_>>(),
        vec![199_997, 199_998, 199_999]
    );
    assert_eq!(
        session
            .fold_events(199_000, 0u64, |count, _| count + 1)
            .unwrap(),
        1_000
    );
    let mut tail = event("future/event", 200_000, json!({"seq": 200_000}), None, None);
    tail.ignorable = Some(true);
    session.append(tail, cancellation()).await.unwrap();
    assert_eq!(session.event_count(), 200_001);
    assert_eq!(
        session
            .find_latest_event(|event| (event.event_type == "future/event").then_some(event.seq))
            .unwrap(),
        Some(200_000)
    );
    let mut samples = Vec::new();
    for point in [1_000u64, 10_000, 100_000, 200_000] {
        let from = point.min(200_000);
        let _ = session.read_events(from, 1).unwrap();
        samples.push((point, session.resident_event_count(), allocator_rss_bytes()));
    }
    eprintln!("long-session samples (events, resident_events, allocator_rss_bytes): {samples:?}");
    assert!(samples.iter().all(|(_, resident, _)| *resident <= 256));

    drop(session);
    let restarted = SessionStore::new(persistence)
        .restore(&id, RestoreMode::Cold, cancellation())
        .await
        .unwrap();
    assert_eq!(restarted.event_count(), 200_001);
    assert!(restarted.resident_event_count() <= 256);
    assert_eq!(
        restarted
            .read_events(100_000, 2)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect::<Vec<_>>(),
        vec![100_000, 100_001]
    );
    fs::remove_dir_all(root).unwrap();
}

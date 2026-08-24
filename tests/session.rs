use std::sync::Arc;

use serde_json::json;
use tessivum::{
    protocol::{SessionEvent, SessionHeader, SessionId, SurfaceOp, SESSION_FORMAT_VERSION},
    session::{
        session_service_key, MemorySessionPersistence, RestoreMode, SessionError,
        SessionPersistence, SessionStore,
    },
};
use tessivum_core::ContextHandle;

fn cancellation() -> tessivum_core::CancellationToken {
    ContextHandle::root().scope().cancellation()
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
        agent_preset: None,
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
    assert_eq!(session.events()[0].data, direct);
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
    assert_eq!(session.events().len(), 1);
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
    assert!(session.events().is_empty());
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

    assert_eq!(session.seed_events().len(), 1);
    assert_eq!(session.live_events().len(), 2);
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

    assert_eq!(session.events().len(), 1);
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
    assert_eq!(session.events().len(), 2);
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
    let repaired = restored.events();
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
    let events = restored.events();
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

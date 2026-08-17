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
            "message": {
                "id": id,
                "role": "user",
                "content": [{"type": "text", "text": text}],
                "source": {"kind": "user"},
            },
        }),
        None,
        Some(surface_op),
    )
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
    session.append(replacement, cancellation()).await.unwrap();

    let surface = session.surface();
    assert_eq!(surface.len(), 1);
    assert_eq!(surface[0].message.id.as_str(), "message-b");
    assert_eq!(surface[0].source_event_seqs, Some(vec![0]));
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

#[test]
fn session_store_publishes_through_context_handle() {
    let context = ContextHandle::root();
    let store = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let handle = context.provide(session_service_key(), store).unwrap();

    assert!(handle.is_current());
    assert_eq!(handle.key().diagnostic_key(), "harness.sessions@1");
    assert!(handle.with(SessionStore::list).unwrap().is_empty());
}

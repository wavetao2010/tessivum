use std::{fs, sync::Arc};

use serde_json::json;
use tessivum::{
    persistence_sqlite::SqliteSessionPersistence,
    projection::{ProjectionDefinition, ProjectionError, ProjectionRegistry},
    protocol::{SessionEvent, SessionHeader, SessionId, SESSION_FORMAT_VERSION},
    session::{MemorySessionPersistence, SessionPersistence, SessionStore},
    session_query::{
        SessionListRequest, SessionLogRequest, SessionQuery, SessionQueryError, SessionQueryFilter,
    },
};
use tessivum_core::ContextHandle;
use uuid::Uuid;

fn cancellation() -> tessivum_core::CancellationToken {
    ContextHandle::root().scope().cancellation()
}

fn root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tessivum-sqlite-query-{}", Uuid::new_v4()))
}

fn header(id: &str) -> SessionHeader {
    SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: SessionId::from(id),
        created_at: 7,
        cwd: Some("/work".into()),
        parent_session: None,
        seed_length: None,
        origin: None,
        delegation_depth: None,
        agent_preset: None,
    }
}

fn event(seq: u64) -> SessionEvent {
    SessionEvent {
        event_type: "turn/start".into(),
        seq,
        time: seq + 10,
        data: json!({"turn": seq}),
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
    }
}

#[tokio::test]
async fn sqlite_commits_exact_dtos_and_rejects_partial_or_concurrent_duplicates() {
    let root = root();
    let path = root.join("nested/sessions.sqlite");
    let persistence = Arc::new(SqliteSessionPersistence::open(&path).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let original_header = header("sqlite");
    persistence
        .create(&original_header, cancellation())
        .await
        .unwrap();
    persistence
        .append(&original_header.id, &event(0), cancellation())
        .await
        .unwrap();

    let left = {
        let persistence = Arc::clone(&persistence);
        let id = original_header.id.clone();
        tokio::spawn(async move { persistence.append(&id, &event(1), cancellation()).await })
    };
    let right = {
        let persistence = Arc::clone(&persistence);
        let id = original_header.id.clone();
        tokio::spawn(async move { persistence.append(&id, &event(1), cancellation()).await })
    };
    let outcomes = [left.await.unwrap(), right.await.unwrap()];
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        persistence
            .read_from(&original_header.id, 0, cancellation())
            .await
            .unwrap(),
        vec![event(0), event(1)]
    );

    assert!(persistence
        .append(&original_header.id, &event(1), cancellation())
        .await
        .is_err());
    assert_eq!(
        persistence
            .inspect(&original_header.id, cancellation())
            .await
            .unwrap()
            .unwrap()
            .event_count,
        2
    );
    drop(persistence);

    let reopened = SqliteSessionPersistence::open(&path).unwrap();
    assert_eq!(
        reopened
            .load(&original_header.id, cancellation())
            .await
            .unwrap()
            .unwrap(),
        original_header
    );
    assert_eq!(
        reopened
            .read_from(&SessionId::from("sqlite"), 0, cancellation())
            .await
            .unwrap(),
        vec![event(0), event(1)]
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn sqlite_rollback_is_explicit_and_jsonl_dtos_import_without_loss() {
    let root = root();
    let persistence = SqliteSessionPersistence::open(root.join("db.sqlite")).unwrap();
    let upstream_header: SessionHeader = serde_json::from_str(
        r#"{"type":"session","version":0,"id":"old","createdAt":7,"cwd":"/work"}"#,
    )
    .unwrap();
    let upstream_event: SessionEvent =
        serde_json::from_str(r#"{"type":"turn/start","seq":0,"time":10,"data":{"turn":0}}"#)
            .unwrap();
    persistence
        .create(&upstream_header, cancellation())
        .await
        .unwrap();
    persistence
        .append(&upstream_header.id, &upstream_event, cancellation())
        .await
        .unwrap();
    persistence
        .rollback(&upstream_header.id, 0, cancellation())
        .await
        .unwrap();
    assert!(persistence
        .read_from(&upstream_header.id, 0, cancellation())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        persistence
            .state(&upstream_header.id)
            .unwrap()
            .unwrap()
            .incarnation,
        2
    );
    fs::remove_dir_all(root).unwrap();
}

fn counter_projection(version: u64) -> ProjectionDefinition {
    ProjectionDefinition::new(
        "counter",
        version,
        |_| Ok(json!({"count": 0})),
        |state, _| {
            let count = state["count"].as_u64().unwrap();
            Ok(json!({"count": count + 1}))
        },
        |state| Ok(state.clone()),
    )
}

#[tokio::test]
async fn projections_checkpoint_versions_and_as_of_states_are_observable() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let store = SessionStore::new(Arc::clone(&persistence));
    let session = store
        .create(header("project"), cancellation())
        .await
        .unwrap();
    session.append(event(0), cancellation()).await.unwrap();
    session.append(event(1), cancellation()).await.unwrap();

    let registry = ProjectionRegistry::new();
    registry.register(counter_projection(1)).unwrap();
    registry.attach(Arc::clone(&session)).unwrap();
    assert_eq!(
        registry
            .as_of_seq(&session.id(), "counter", 0)
            .unwrap()
            .state,
        json!({"count": 1})
    );
    assert_eq!(
        registry.snapshot(&session.id(), "counter").unwrap().state,
        json!({"count": 2})
    );
    let checkpoint = registry.checkpoint(&session.id(), "counter").unwrap();

    let restarted = ProjectionRegistry::new();
    restarted.register(counter_projection(1)).unwrap();
    restarted.attach(Arc::clone(&session)).unwrap();
    restarted.restore(&session, checkpoint.clone()).unwrap();
    assert_eq!(
        restarted.snapshot(&session.id(), "counter").unwrap().state,
        json!({"count": 2})
    );

    let incompatible = ProjectionRegistry::new();
    incompatible.register(counter_projection(2)).unwrap();
    incompatible.attach(Arc::clone(&session)).unwrap();
    let error = incompatible
        .restore_floor(&session, checkpoint, 1)
        .unwrap_err();
    assert!(matches!(error, ProjectionError::FullRereadRequired { .. }));
    registry.shutdown().await;
    restarted.shutdown().await;
    incompatible.shutdown().await;
}

#[tokio::test]
async fn query_prefers_live_and_detects_cursor_changes_tampering_and_cancellation() {
    let live_persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let live_store = SessionStore::new(Arc::clone(&live_persistence));
    let live = live_store
        .create(header("live"), cancellation())
        .await
        .unwrap();
    live.append(event(0), cancellation()).await.unwrap();

    let stale_persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    stale_persistence
        .create(&header("live"), cancellation())
        .await
        .unwrap();
    stale_persistence
        .create(&header("second"), cancellation())
        .await
        .unwrap();
    let query = SessionQuery::new(live_store, Arc::clone(&stale_persistence));
    assert!(
        query
            .read(&SessionId::from("live"), cancellation())
            .await
            .unwrap()
            .live
    );

    let first = query
        .list(
            SessionListRequest {
                limit: Some(1),
                ..Default::default()
            },
            cancellation(),
        )
        .await
        .unwrap();
    let cursor = first.cursor.unwrap();
    stale_persistence
        .append(&SessionId::from("second"), &event(0), cancellation())
        .await
        .unwrap();
    assert!(matches!(
        query
            .list(
                SessionListRequest {
                    limit: Some(1),
                    cursor: Some(cursor.clone()),
                    ..Default::default()
                },
                cancellation()
            )
            .await,
        Err(SessionQueryError::CursorStale)
    ));
    let mut tampered = cursor;
    tampered.replace_range(0..1, "0");
    assert!(matches!(
        query
            .list(
                SessionListRequest {
                    limit: Some(1),
                    cursor: Some(tampered),
                    ..Default::default()
                },
                cancellation()
            )
            .await,
        Err(SessionQueryError::InvalidCursor)
    ));

    let cancelled = cancellation();
    assert!(cancelled.cancel());
    assert!(matches!(
        query.list(SessionListRequest::default(), cancelled).await,
        Err(SessionQueryError::Cancelled)
    ));
}

#[tokio::test]
async fn semantic_filter_only_reads_text_blocks_literally_and_snapshots_stay_contiguous() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let store = SessionStore::new(Arc::clone(&persistence));
    let session = store
        .create(header("semantic"), cancellation())
        .await
        .unwrap();
    let metadata = SessionEvent {
        event_type: "turn/start".into(),
        seq: 0,
        time: 10,
        data: json!({"turn": 0, "metadata": "secret [a-z]+"}),
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
    };
    let text = SessionEvent {
        event_type: "turn/end".into(),
        seq: 1,
        time: 11,
        data: json!({"turn": 0, "content": [{"type": "text", "text": "Ångström\n  Value [a-z]+"}]}),
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
    };
    session.append(metadata, cancellation()).await.unwrap();
    session.append(text.clone(), cancellation()).await.unwrap();
    let query = SessionQuery::new(store, persistence);
    let excluded = query
        .log(
            &session.id(),
            SessionLogRequest {
                filter: SessionQueryFilter {
                    text: Some("secret [a-z]+".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert!(excluded.items.is_empty());
    let included = query
        .log(
            &session.id(),
            SessionLogRequest {
                filter: SessionQueryFilter {
                    text: Some("ångström value [a-z]+".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(included.items, vec![text]);
}

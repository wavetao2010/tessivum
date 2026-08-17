use std::{fs, sync::Arc};

use serde_json::json;
use tessivum::{
    persistence_jsonl::JsonlSessionPersistence,
    protocol::{SessionEvent, SessionHeader, SessionId, SESSION_FORMAT_VERSION},
    session::{RestoreMode, SessionError, SessionPersistence, SessionStore},
};
use tessivum_core::ContextHandle;
use uuid::Uuid;

fn cancellation() -> tessivum_core::CancellationToken {
    ContextHandle::root().scope().cancellation()
}

fn root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tessivum-jsonl-{}", Uuid::new_v4()))
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

fn event(seq: u64) -> SessionEvent {
    SessionEvent {
        event_type: "turn/start".into(),
        seq,
        time: 0,
        data: json!({"turn": seq}),
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
    }
}

#[tokio::test]
async fn loads_upstream_header_shape_and_keeps_type_out_of_dto() {
    let root = root();
    let persistence = JsonlSessionPersistence::new(&root);
    let id = SessionId::from("upstream");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        persistence.raw_path(&id),
        r#"{"type":"session","version":0,"id":"upstream","createdAt":0}
"#,
    )
    .unwrap();

    assert_eq!(
        persistence
            .load(&id, cancellation())
            .await
            .unwrap()
            .unwrap(),
        header("upstream")
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn appends_reopen_in_order_and_confines_traversal_ids() {
    let root = root();
    let persistence = JsonlSessionPersistence::new(&root);
    let unsafe_id = SessionId::from("../../outside");
    let unsafe_header = header(unsafe_id.as_str());
    persistence
        .create(&unsafe_header, cancellation())
        .await
        .unwrap();
    assert_eq!(
        persistence.raw_path(&unsafe_id).parent(),
        Some(root.as_path())
    );
    assert!(!root.parent().unwrap().join("outside").exists());

    persistence
        .append(&unsafe_id, &event(0), cancellation())
        .await
        .unwrap();
    persistence
        .append(&unsafe_id, &event(1), cancellation())
        .await
        .unwrap();
    let reopened = JsonlSessionPersistence::new(&root);
    assert_eq!(
        reopened
            .read_from(&unsafe_id, 0, cancellation())
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn concurrent_append_and_duplicate_create_are_serialized() {
    let root = root();
    let persistence = Arc::new(JsonlSessionPersistence::new(&root));
    let head = header("concurrent");
    persistence.create(&head, cancellation()).await.unwrap();
    let first = {
        let persistence = Arc::clone(&persistence);
        let id = head.id.clone();
        tokio::spawn(async move { persistence.append(&id, &event(0), cancellation()).await })
    };
    tokio::task::yield_now().await;
    let second = {
        let persistence = Arc::clone(&persistence);
        let id = head.id.clone();
        tokio::spawn(async move { persistence.append(&id, &event(1), cancellation()).await })
    };
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(
        persistence
            .inspect(&head.id, cancellation())
            .await
            .unwrap()
            .unwrap()
            .event_count,
        2
    );

    let duplicate = header("duplicate");
    let left = {
        let persistence = Arc::clone(&persistence);
        let header = duplicate.clone();
        tokio::spawn(async move { persistence.create(&header, cancellation()).await })
    };
    let right = {
        let persistence = Arc::clone(&persistence);
        tokio::spawn(async move { persistence.create(&duplicate, cancellation()).await })
    };
    let results = [left.await.unwrap(), right.await.unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(results
        .iter()
        .any(|result| matches!(result, Err(SessionError::AlreadyExists(_)))));
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn torn_raw_tail_is_ignored_without_repairing_the_file() {
    let root = root();
    let persistence = JsonlSessionPersistence::new(&root);
    let head = header("raw-tail");
    persistence.create(&head, cancellation()).await.unwrap();
    persistence
        .append(&head.id, &event(0), cancellation())
        .await
        .unwrap();
    let path = persistence.raw_path(&head.id);
    let mut bytes = fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{\"type\":");
    fs::write(&path, &bytes).unwrap();

    assert_eq!(
        persistence
            .read_from(&head.id, 0, cancellation())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        persistence
            .inspect(&head.id, cancellation())
            .await
            .unwrap()
            .unwrap()
            .event_count,
        1
    );
    assert_eq!(fs::read(path).unwrap(), bytes);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn torn_compressed_tail_is_ignored_but_committed_corruption_rejects() {
    let root = root();
    let compressed = JsonlSessionPersistence::zstd(&root);
    let compressed_header = header("zstd-tail");
    compressed
        .create(&compressed_header, cancellation())
        .await
        .unwrap();
    compressed
        .append(&compressed_header.id, &event(0), cancellation())
        .await
        .unwrap();
    let compressed_path = compressed.compressed_path(&compressed_header.id);
    let mut compressed_bytes = fs::read(&compressed_path).unwrap();
    compressed_bytes.extend_from_slice(&[1, 2, 3]);
    fs::write(&compressed_path, compressed_bytes).unwrap();
    assert_eq!(
        compressed
            .read_from(&compressed_header.id, 0, cancellation())
            .await
            .unwrap()
            .len(),
        1
    );

    let raw = JsonlSessionPersistence::new(&root);
    let raw_header = header("committed-corruption");
    raw.create(&raw_header, cancellation()).await.unwrap();
    let raw_path = raw.raw_path(&raw_header.id);
    let mut raw_bytes = fs::read(&raw_path).unwrap();
    raw_bytes.extend_from_slice(b"not-json\n");
    fs::write(&raw_path, &raw_bytes).unwrap();
    assert!(raw.load(&raw_header.id, cancellation()).await.is_err());
    assert!(raw.inspect(&raw_header.id, cancellation()).await.is_err());
    assert!(raw
        .read_from(&raw_header.id, 0, cancellation())
        .await
        .is_err());
    assert_eq!(fs::read(raw_path).unwrap(), raw_bytes);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn rejects_version_id_sequence_and_required_event_corruption() {
    let root = root();
    let persistence = JsonlSessionPersistence::new(&root);
    let id = SessionId::from("corrupt");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        persistence.raw_path(&id),
        r#"{"type":"session","version":0,"id":"corrupt","createdAt":0}
{"type":"turn/start","seq":2,"time":0,"data":{"turn":2}}
"#,
    )
    .unwrap();
    assert!(persistence.read_from(&id, 0, cancellation()).await.is_err());

    let unknown = SessionId::from("unknown");
    fs::write(
        persistence.raw_path(&unknown),
        r#"{"type":"session","version":0,"id":"unknown","createdAt":0}
{"type":"future/event","seq":0,"time":0,"data":{}}
"#,
    )
    .unwrap();
    assert!(persistence.load(&unknown, cancellation()).await.is_err());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn list_and_cold_restore_continue_from_durable_log() {
    let root = root();
    let persistence: Arc<dyn SessionPersistence> = Arc::new(JsonlSessionPersistence::new(&root));
    let store = SessionStore::new(Arc::clone(&persistence));
    let first = store.create(header("b"), cancellation()).await.unwrap();
    let second = store.create(header("a"), cancellation()).await.unwrap();
    assert_eq!(
        persistence
            .list(cancellation())
            .await
            .unwrap()
            .into_iter()
            .map(|inspection| inspection.header.id)
            .collect::<Vec<_>>(),
        vec![SessionId::from("a"), SessionId::from("b")]
    );

    first.append(event(0), cancellation()).await.unwrap();
    let reader = SessionStore::new(persistence);
    let restored = reader
        .restore(&SessionId::from("b"), RestoreMode::Cold, cancellation())
        .await
        .unwrap();
    assert_eq!(restored.events().len(), 2);
    restored.append(event(2), cancellation()).await.unwrap();
    assert_eq!(restored.next_seq().unwrap(), 3);
    drop(second);
    fs::remove_dir_all(root).unwrap();
}

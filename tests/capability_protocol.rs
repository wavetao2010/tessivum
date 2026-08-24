use serde_json::{json, Value};
use tessivum::protocol::{SessionEvent, TodoItem, TodoStatus};

const REQUIRED_DURABLE_EVENTS: &[&str] = &[
    "goal/change",
    "plan/change",
    "approval/policy",
    "approval/asked",
    "approval/decided",
    "job/done",
    "subagent/contained-start",
    "subagent/contained-end",
    "tool-workflow/run-start",
    "tool-workflow/agent-start",
    "tool-workflow/agent-end",
    "tool-workflow/run-end",
];

const COMPACTION_LOG_EVENTS: &[&str] =
    &["compaction/start", "compaction/summary", "compaction/end"];

fn wire_event(event_type: &str, seq: u64, data: Value) -> Value {
    json!({
        "type": event_type,
        "seq": seq,
        "time": seq,
        "data": data,
    })
}
fn required_event_data(event_type: &str) -> Value {
    match event_type {
        "tool-workflow/run-start" => json!({"runId": "run-1", "name": "workflow"}),
        "tool-workflow/agent-start" => json!({
            "runId": "run-1",
            "seq": 1,
            "label": "worker",
            "childId": "child-1",
        }),
        "tool-workflow/agent-end" => {
            json!({"runId": "run-1", "seq": 1, "outcome": "completed"})
        }
        "tool-workflow/run-end" => json!({"runId": "run-1", "stopReason": "completed"}),
        _ => json!({"event": event_type}),
    }
}

#[test]
fn durable_capability_events_validate_without_ignorable_and_round_trip() {
    for (seq, event_type) in REQUIRED_DURABLE_EVENTS.iter().enumerate() {
        let wire = wire_event(event_type, seq as u64, required_event_data(event_type));
        let event: SessionEvent =
            serde_json::from_value(wire.clone()).expect("durable event envelope deserializes");

        assert_eq!(event.ignorable, None, "{event_type} must be required");
        event
            .validate()
            .unwrap_or_else(|error| panic!("{event_type} must validate: {error}"));
        assert_eq!(serde_json::to_value(event).unwrap(), wire);
    }
}

#[test]
fn compaction_lifecycle_is_explicitly_ignorable_and_round_trips() {
    for (seq, event_type) in COMPACTION_LOG_EVENTS.iter().enumerate() {
        let bare = wire_event(event_type, seq as u64, json!({}));
        let bare_event: SessionEvent =
            serde_json::from_value(bare.clone()).expect("compaction envelope deserializes");
        assert!(
            bare_event.validate().is_err(),
            "{event_type} must not become a required event"
        );

        let mut explicit_false = bare;
        explicit_false["ignorable"] = json!(false);
        let explicit_false: SessionEvent =
            serde_json::from_value(explicit_false).expect("compaction envelope deserializes");
        assert!(
            explicit_false.validate().is_err(),
            "{event_type} must require ignorable true"
        );

        let mut ignorable = wire_event(event_type, seq as u64, json!({}));
        ignorable["ignorable"] = json!(true);
        let event: SessionEvent =
            serde_json::from_value(ignorable.clone()).expect("compaction envelope deserializes");
        event
            .validate()
            .unwrap_or_else(|error| panic!("{event_type} must validate when ignorable: {error}"));
        assert_eq!(serde_json::to_value(event).unwrap(), ignorable);
    }
}

#[test]
fn capability_log_events_forbid_surface_metadata() {
    for (seq, event_type) in REQUIRED_DURABLE_EVENTS
        .iter()
        .chain(COMPACTION_LOG_EVENTS)
        .enumerate()
    {
        let mut surfaced = wire_event(event_type, seq as u64, json!({}));
        if COMPACTION_LOG_EVENTS.contains(event_type) {
            surfaced["ignorable"] = json!(true);
        }
        surfaced["surfaceOp"] = json!("append");
        let surfaced: SessionEvent =
            serde_json::from_value(surfaced).expect("surface metadata deserializes");
        assert!(
            surfaced.validate().is_err(),
            "{event_type} must not accept surfaceOp"
        );

        let mut sourced = wire_event(event_type, seq as u64, json!({}));
        if COMPACTION_LOG_EVENTS.contains(event_type) {
            sourced["ignorable"] = json!(true);
        }
        sourced["sourceEventSeqs"] = json!([]);
        let sourced: SessionEvent =
            serde_json::from_value(sourced).expect("source metadata deserializes");
        assert!(
            sourced.validate().is_err(),
            "{event_type} must not accept sourceEventSeqs"
        );
    }
}

#[test]
fn todo_write_keeps_the_existing_minimal_planning_shape() {
    let wire = wire_event(
        "todo/write",
        7,
        json!({"todos": [{"content": "Reconstruct the session", "status": "in_progress"}]}),
    );
    let event: SessionEvent =
        serde_json::from_value(wire.clone()).expect("todo/write envelope deserializes");

    event.validate().expect("todo/write is a known event");
    assert_eq!(serde_json::to_value(&event).unwrap(), wire);
    assert!(event.surface_op.is_none());
    assert!(event.source_event_seqs.is_none());

    let todos: Vec<TodoItem> =
        serde_json::from_value(event.data["todos"].clone()).expect("todo items deserialize");
    assert_eq!(todos[0].status, TodoStatus::InProgress);
    todos
        .iter()
        .try_for_each(TodoItem::validate)
        .expect("content and status are valid");

    assert!(serde_json::from_value::<TodoItem>(json!({"content": "missing status"})).is_err());
    let empty = TodoItem {
        content: String::new(),
        status: TodoStatus::Pending,
    };
    assert!(empty.validate().is_err());
}

#[test]
fn durable_capability_prefix_round_trips_with_contiguous_sequences() {
    let prefix: Vec<Value> = REQUIRED_DURABLE_EVENTS
        .iter()
        .enumerate()
        .map(|(offset, event_type)| {
            wire_event(
                event_type,
                100 + offset as u64,
                required_event_data(event_type),
            )
        })
        .collect();
    let events: Vec<SessionEvent> = prefix
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .expect("capability prefix deserializes");

    for event in &events {
        event.validate().expect("capability event validates");
    }
    assert!(events.windows(2).all(|pair| pair[1].seq == pair[0].seq + 1));
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        REQUIRED_DURABLE_EVENTS
    );
    assert_eq!(
        events
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        prefix
    );
}

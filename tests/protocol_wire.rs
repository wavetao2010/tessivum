use serde_json::{json, Value};
use tessivum::protocol::{ProviderRequestId, SessionEvent, SessionHeader, StreamChunk, SurfaceOp};

const EXPECTED_EVENTS: &str = include_str!("../fixtures/headless/expected-events.json");
const RECORDED_REPLAY: &str = include_str!("../fixtures/headless/recorded-replay.jsonl");

fn expected_fixture() -> Value {
    serde_json::from_str(EXPECTED_EVENTS).expect("expected event fixture is JSON")
}

#[test]
fn recorded_headless_replay_preserves_raw_stream_wire_shape() {
    let frames: Vec<Value> = RECORDED_REPLAY
        .lines()
        .map(|line| serde_json::from_str(line).expect("replay line is JSON"))
        .collect();

    let append: SurfaceOp = serde_json::from_value(json!("append")).expect("append surface op");
    assert_eq!(serde_json::to_value(append).unwrap(), json!("append"));
    assert!(serde_json::from_value::<SurfaceOp>(json!("splice")).is_err());
    assert_eq!(frames.len(), 10);
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame["chunk"]["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "block-start",
            "tool-call-delta",
            "block-end",
            "usage",
            "finish",
            "block-start",
            "text-delta",
            "block-end",
            "usage",
            "finish",
        ]
    );

    for frame in &frames {
        let request_id: ProviderRequestId =
            serde_json::from_value(frame["requestId"].clone()).expect("opaque request id");
        assert_eq!(
            serde_json::to_value(request_id).unwrap(),
            frame["requestId"]
        );

        let chunk: StreamChunk =
            serde_json::from_value(frame["chunk"].clone()).expect("known stream chunk");
        assert_eq!(serde_json::to_value(chunk).unwrap(), frame["chunk"]);
    }

    assert_eq!(
        frames[1]["chunk"]["argumentsDelta"],
        frames[2]["chunk"]["block"]["arguments"]
    );
    assert_eq!(frames[3]["chunk"]["type"], "usage");
    assert_eq!(frames[4]["chunk"]["type"], "finish");
}

#[test]
fn headless_fixture_round_trips_header_events_and_order() {
    let fixture = expected_fixture();
    let header: SessionHeader =
        serde_json::from_value(fixture["header"].clone()).expect("session header");
    header.validate().expect("format 0 header is valid");
    assert_eq!(serde_json::to_value(header).unwrap(), fixture["header"]);

    let event_json = fixture["events"].as_array().expect("event array");
    let events: Vec<SessionEvent> = event_json
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .expect("event fixture deserializes");
    for event in &events {
        event.validate().expect("fixture event is valid");
    }
    assert_eq!(
        events
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        event_json.to_vec()
    );

    assert_eq!(
        event_json
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "turn/start",
            "step/start",
            "user/message",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/message",
            "tool/call",
            "tool/result",
            "step/end",
            "step/start",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/message",
            "step/end",
            "turn/end",
            "experimental/replay-marker",
        ]
    );
    assert_eq!(
        event_json[8]["sourceEventSeqs"],
        json!([13, 14, 15, 16, 17])
    );
    assert_eq!(event_json[10]["sourceEventSeqs"], json!([19]));
    assert_eq!(
        event_json[18]["sourceEventSeqs"],
        json!([24, 25, 26, 27, 28])
    );
    assert_eq!(
        event_json[10]["data"]["message"]["source"]["callId"],
        event_json[9]["data"]["callId"]
    );
}

#[test]
fn empty_assistant_source_sequences_and_ignorable_unknown_events_round_trip() {
    let fixture = expected_fixture();
    let mut empty_sources = fixture["events"][8].clone();
    empty_sources["sourceEventSeqs"] = json!([]);
    let assistant: SessionEvent =
        serde_json::from_value(empty_sources.clone()).expect("empty source sequence list is valid");
    assistant
        .validate()
        .expect("empty source sequence list validates");
    assert_eq!(serde_json::to_value(assistant).unwrap(), empty_sources);

    let unknown = fixture["events"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    let unknown_event: SessionEvent =
        serde_json::from_value(unknown.clone()).expect("ignorable unknown event parses");
    unknown_event
        .validate()
        .expect("ignorable unknown event validates");
    assert_eq!(serde_json::to_value(unknown_event).unwrap(), unknown);

    let mut required_unknown = unknown;
    required_unknown
        .as_object_mut()
        .unwrap()
        .remove("ignorable");
    let required_unknown: SessionEvent =
        serde_json::from_value(required_unknown).expect("unknown event remains inspectable");
    assert!(required_unknown.validate().is_err());
}

#[test]
fn validation_rejects_misspelled_required_event() {
    let misspelled: SessionEvent = serde_json::from_value(json!({
        "type": "approval/askd",
        "seq": 0,
        "time": 0,
        "data": {}
    }))
    .expect("event envelope deserializes");

    assert_eq!(
        misspelled
            .validate()
            .expect_err("misspelled event rejects")
            .code,
        "UNKNOWN_REQUIRED_EVENT"
    );
}

#[test]
fn validation_rejects_nonzero_format_negative_sequences_and_misplaced_surface_metadata() {
    let fixture = expected_fixture();

    let mut unsupported_header = fixture["header"].clone();
    unsupported_header["version"] = json!(1);
    let unsupported_header: SessionHeader = serde_json::from_value(unsupported_header)
        .expect("header keeps its numeric version for validation");
    assert!(unsupported_header.validate().is_err());

    let mut negative_sequence = fixture["events"][0].clone();
    negative_sequence["seq"] = json!(-1);
    if let Ok(event) = serde_json::from_value::<SessionEvent>(negative_sequence) {
        assert!(event.validate().is_err());
    }

    let mut wrongly_surfaced = fixture["events"][0].clone();
    wrongly_surfaced["surfaceOp"] = json!("append");
    let wrongly_surfaced: SessionEvent = serde_json::from_value(wrongly_surfaced)
        .expect("surface metadata is checked by SessionEvent validation");
    assert!(wrongly_surfaced.validate().is_err());
}

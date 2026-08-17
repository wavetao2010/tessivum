use std::collections::BTreeMap;

use serde_json::{json, Value};

#[path = "../src/oracle.rs"]
mod oracle;

use oracle::{normalize_session_trace, ReplacementMap};

const EXPECTED_EVENTS: &str = include_str!("../fixtures/headless/expected-events.json");
const RECORDED_REPLAY: &str = include_str!("../fixtures/headless/recorded-replay.jsonl");
const TYPESCRIPT_NORMALIZED: &str = include_str!("../fixtures/headless/typescript-normalized.json");

fn fixture(source: &str) -> Value {
    serde_json::from_str(source).expect("fixture is valid JSON")
}

fn normalize(trace: &Value, replacements: &ReplacementMap) -> Value {
    normalize_session_trace(
        &trace["header"],
        trace["events"].as_array().expect("trace has events"),
        replacements,
    )
    .expect("trace serializes")
}

fn replacements() -> ReplacementMap {
    BTreeMap::from([
        ("session-headless-replay".into(), "<session-id>".into()),
        ("/workspace".into(), "<cwd>".into()),
    ])
}

#[test]
fn normalization_is_idempotent() {
    let expected = fixture(EXPECTED_EVENTS);
    let replacements = replacements();
    let normalized = normalize(&expected, &replacements);

    assert_eq!(
        normalize(&normalized, &replacements),
        normalized,
        "canonical traces do not change when normalized again"
    );
}

#[test]
fn normalization_changes_only_explicit_volatile_fields() {
    let trace = json!({
        "header": {
            "version": 0,
            "id": "run-session",
            "createdAt": 42,
            "cwd": "/private/run",
            "parentSession": "parent-session",
            "delegationDepth": 1
        },
        "events": [
            {
                "type": "user/message",
                "seq": 4,
                "time": 900,
                "data": {
                    "role": "user",
                    "id": "run-message",
                    "content": [{"type": "text", "text": "keep run-session /private/run"}],
                    "source": {"kind": "user"}
                },
                "sourceEventSeqs": [3],
                "surfaceOp": "append"
            },
            {
                "type": "assistant/chunk",
                "seq": 5,
                "time": 901,
                "requestId": "provider-request",
                "data": {
                    "turn": 1,
                    "step": 1,
                    "chunk": {
                        "type": "tool-call-delta",
                        "index": 0,
                        "id": "run-call",
                        "name": "bash",
                        "argumentsDelta": r#"{"command":"printf run-session /private/run"}"#
                    }
                }
            },
            {
                "type": "assistant/chunk",
                "seq": 6,
                "time": 902,
                "data": {
                    "chunk": {
                        "type": "finish",
                        "reason": {
                            "kind": "error",
                            "failure": {
                                "message": "provider failure",
                                "code": "MODEL_DOWN",
                                "requestId": "provider-request"
                            }
                        }
                    }
                }
            },
            {
                "type": "vendor/required-event",
                "seq": 7,
                "time": 903,
                "ignorable": true,
                "data": {"id": "run-session", "text": "keep run-session /private/run"}
            }
        ]
    });
    let replacements = BTreeMap::from([
        ("run-session".into(), "<session-id>".into()),
        ("parent-session".into(), "<parent-session-id>".into()),
        ("run-message".into(), "<message-id>".into()),
        ("run-call".into(), "<tool-call-id>".into()),
        ("provider-request".into(), "<provider-request-id>".into()),
        ("/private/run".into(), "<cwd>".into()),
    ]);

    let normalized = normalize(&trace, &replacements);
    let expected = json!({
        "header": {
            "version": 0,
            "id": "<session-id>",
            "createdAt": 42,
            "cwd": "<cwd>",
            "parentSession": "<parent-session-id>",
            "delegationDepth": 1
        },
        "events": [
            {
                "type": "user/message",
                "seq": 4,
                "time": 0,
                "data": {
                    "role": "user",
                    "id": "<message-id>",
                    "content": [{"type": "text", "text": "keep run-session /private/run"}],
                    "source": {"kind": "user"}
                },
                "sourceEventSeqs": [3],
                "surfaceOp": "append"
            },
            {
                "type": "assistant/chunk",
                "seq": 5,
                "time": 0,
                "requestId": "<provider-request-id>",
                "data": {
                    "turn": 1,
                    "step": 1,
                    "chunk": {
                        "type": "tool-call-delta",
                        "index": 0,
                        "id": "<tool-call-id>",
                        "name": "bash",
                        "argumentsDelta": r#"{"command":"printf run-session /private/run"}"#
                    }
                }
            },
            {
                "type": "assistant/chunk",
                "seq": 6,
                "time": 0,
                "data": {
                    "chunk": {
                        "type": "finish",
                        "reason": {
                            "kind": "error",
                            "failure": {
                                "message": "provider failure",
                                "code": "MODEL_DOWN",
                                "requestId": "<provider-request-id>"
                            }
                        }
                    }
                }
            },
            {
                "type": "vendor/required-event",
                "seq": 7,
                "time": 0,
                "ignorable": true,
                "data": {"id": "run-session", "text": "keep run-session /private/run"}
            }
        ]
    });

    assert_eq!(normalized, expected);
}

#[test]
fn normalization_replaces_tool_and_message_correlations_without_parsing_arguments() {
    let header = json!({"id": "session-id", "cwd": "/run"});
    let events = vec![json!({
        "type": "tool/result",
        "seq": 0,
        "time": 1,
        "data": {
            "message": {
                "role": "user",
                "id": "message-id",
                "content": [{
                    "type": "tool-result",
                    "toolCallId": "tool-call-id",
                    "content": [{"type": "text", "text": "tool-call-id /run"}],
                    "isError": false
                }],
                "source": {"kind": "tool", "callId": "tool-call-id"}
            }
        },
        "sourceEventSeqs": [0],
        "surfaceOp": "append"
    })];
    let replacements = BTreeMap::from([
        ("session-id".into(), "<session-id>".into()),
        ("message-id".into(), "<message-id>".into()),
        ("tool-call-id".into(), "<tool-call-id>".into()),
        ("/run".into(), "<cwd>".into()),
    ]);

    let normalized = normalize_session_trace(header, events, &replacements).unwrap();
    assert_eq!(normalized["header"]["id"], json!("<session-id>"));
    assert_eq!(
        normalized["events"][0]["data"]["message"]["id"],
        json!("<message-id>")
    );
    assert_eq!(
        normalized["events"][0]["data"]["message"]["source"]["callId"],
        json!("<tool-call-id>")
    );
    assert_eq!(
        normalized["events"][0]["data"]["message"]["content"][0]["toolCallId"],
        json!("<tool-call-id>")
    );
    assert_eq!(
        normalized["events"][0]["data"]["message"]["content"][0]["content"][0]["text"],
        json!("tool-call-id /run")
    );
}

#[test]
fn canonical_comparison_rejects_reordered_missing_and_changed_model_data() {
    let expected = fixture(EXPECTED_EVENTS);
    let replacements = replacements();
    let canonical = normalize(&expected, &replacements);

    assert_eq!(
        canonical["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        "normalization preserves event order"
    );
    assert_eq!(
        canonical["events"][8]["sourceEventSeqs"],
        json!([13, 14, 15, 16, 17])
    );
    assert_eq!(canonical["events"][8]["surfaceOp"], json!("append"));
    assert_eq!(
        canonical["events"][4]["data"]["chunk"]["argumentsDelta"],
        expected["events"][4]["data"]["chunk"]["argumentsDelta"],
        "raw tool arguments remain byte-for-byte model-visible data"
    );

    let mut reordered = canonical.clone();
    reordered["events"].as_array_mut().unwrap().swap(0, 1);
    assert_ne!(normalize(&reordered, &replacements), canonical);

    let mut missing = canonical.clone();
    missing["events"].as_array_mut().unwrap().remove(8);
    assert_ne!(normalize(&missing, &replacements), canonical);

    let mut changed = canonical.clone();
    changed["events"][2]["data"]["content"][0]["text"] = json!("different user instruction");
    assert_ne!(normalize(&changed, &replacements), canonical);
}

#[test]
fn typescript_oracle_retains_durable_sequence_and_recorded_replay_chunks() {
    let typescript = fixture(TYPESCRIPT_NORMALIZED);
    let events = typescript["events"].as_array().expect("event array");
    let frames: Vec<Value> = RECORDED_REPLAY
        .lines()
        .map(|line| serde_json::from_str(line).expect("replay line is JSON"))
        .collect();

    assert_eq!(typescript["header"]["version"], json!(0));
    assert_eq!(typescript["header"]["delegationDepth"], json!(0));
    assert_eq!(events.len(), 32);
    assert_eq!(
        events
            .iter()
            .enumerate()
            .map(|(index, event)| event["seq"].as_u64() == Some(index as u64))
            .collect::<Vec<_>>(),
        vec![true; 32],
        "fixture retains every durable event in sequence order"
    );
    assert!(events.iter().all(|event| event["time"] == json!(0)));
    assert_eq!(events[18]["sourceEventSeqs"], json!([13, 14, 15, 16, 17]));
    assert_eq!(events[20]["sourceEventSeqs"], json!([19]));
    assert_eq!(events[29]["sourceEventSeqs"], json!([24, 25, 26, 27, 28]));
    assert_eq!(events[32 - 1]["type"], json!("turn/end"));

    let durable_chunks: Vec<Value> = events
        .iter()
        .filter(|event| event["type"] == "assistant/chunk")
        .map(|event| event["data"]["chunk"].clone())
        .collect();
    let replay_chunks: Vec<Value> = frames
        .into_iter()
        .map(|frame| frame["chunk"].clone())
        .collect();
    assert_eq!(durable_chunks, replay_chunks);
}

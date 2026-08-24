use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use tessivum::{
    headless::{run_headless, HeadlessConfig},
    oracle::{normalize_session_trace, ReplacementMap},
    persistence_jsonl::JsonlSessionPersistence,
    session::SessionPersistence,
    SessionEvent, SessionId,
};
use tessivum_core::ContextHandle;
use uuid::Uuid;

const REPLAY: &str = include_str!("../fixtures/headless/recorded-replay.jsonl");
const TYPESCRIPT_NORMALIZED: &str = include_str!("../fixtures/headless/typescript-normalized.json");
const TASK: &str = "Prove the product headless profile path with one real tool round trip.";
const ANSWER: &str = "CLI tool round trip complete: CLI_TOOL_ROUND_TRIP";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tessivum-headless-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("temporary directory creates");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn config(root: &TempDir, session: &str, resume: bool) -> HeadlessConfig {
    HeadlessConfig {
        data_dir: root.path().join("durable-log"),
        cwd: root.path().into(),
        session_id: SessionId::from(session),
        resume,
        provider: "cli-mock".into(),
        model: "cli-mock".into(),
        max_tokens: Some(128),
        replay_jsonl: REPLAY.into(),
        enable_trusted_bash: true,
        system_prompt: Some("<system>".into()),
    }
}

fn cancellation() -> tessivum_core::CancellationToken {
    ContextHandle::root().scope().cancellation()
}

#[tokio::test]
async fn recorded_bash_round_trip_is_balanced_and_root_is_quiescent() {
    let root = TempDir::new();
    let config = config(&root, "session-headless-replay", false);
    let result = run_headless(config.clone(), TASK.into())
        .await
        .expect("recorded headless run succeeds");

    assert_eq!(result.final_text, ANSWER);
    assert_fixture_event_invariants(&result.events);
    assert_request_header_shapes(&result.events, &["initial"]);
    assert_eq!(
        result
            .events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "turn/start",
            "step/start",
            "user/message",
            "user/message",
            "request/header",
            "request/context",
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
        ]
    );

    let persistence = JsonlSessionPersistence::new(&config.data_dir);
    let events = persistence
        .read_from(&config.session_id, 0, cancellation())
        .await
        .expect("the root scope leaves a durable, readable log");
    assert_eq!(events, result.events);
    assert_fixture_event_invariants(&events);
    assert_request_header_shapes(&events, &["initial"]);
    assert!(events.iter().all(|event| {
        event.event_type != "turn/end" || event.data["reason"]["kind"] != "interrupted"
    }));
}

#[tokio::test]
async fn recorded_resume_uses_a_fresh_runtime_and_continues_the_same_log() {
    let root = TempDir::new();
    let first_config = config(&root, "resume-headless", false);
    let first = run_headless(first_config.clone(), TASK.into())
        .await
        .expect("first process-equivalent run succeeds");

    let duplicate = run_headless(first_config.clone(), TASK.into())
        .await
        .expect_err("a non-resume duplicate must not replace the durable log");
    assert_eq!(duplicate.code(), "SESSION_ALREADY_EXISTS");

    let second = run_headless(config(&root, "resume-headless", true), TASK.into())
        .await
        .expect("fresh runtime cold-resumes the existing session");
    assert_eq!(second.final_text, ANSWER);
    assert_fixture_event_invariants(&first.events);
    assert_request_header_shapes(&first.events, &["initial"]);
    assert_fixture_event_invariants(&second.events);
    assert_request_header_shapes(&second.events, &["resume"]);
    assert_eq!(
        second.events.first().map(|event| event.seq),
        Some(first.events.len() as u64)
    );

    let persistence = JsonlSessionPersistence::new(root.path().join("durable-log"));
    let persisted = persistence
        .read_from(&SessionId::from("resume-headless"), 0, cancellation())
        .await
        .expect("both process-equivalent runs share one log");
    assert_eq!(persisted.len(), first.events.len() + second.events.len());
    assert_fixture_event_invariants(&persisted);
    assert_request_header_shapes(&persisted, &["initial", "resume"]);
    assert_eq!(
        persisted
            .iter()
            .filter(|event| event.event_type == "turn/end")
            .count(),
        2,
        "each completed process owns one balanced turn"
    );
}

#[tokio::test]
async fn rejects_missing_replay_and_blank_task_before_starting_a_session() {
    let root = TempDir::new();
    let mut missing_replay = config(&root, "missing-replay", false);
    missing_replay.replay_jsonl.clear();
    let error = run_headless(missing_replay, TASK.into())
        .await
        .expect_err("empty replays are not a runnable model route");
    assert_eq!(error.code(), "INVALID_HEADLESS_REPLAY");

    let error = run_headless(config(&root, "blank-task", false), " \n\t ".into())
        .await
        .expect_err("blank tasks are rejected before durable setup");
    assert_eq!(error.code(), "INVALID_HEADLESS_TASK");
}

#[tokio::test]
async fn normalized_rust_suffix_matches_typescript_for_overlapping_events() {
    let root = TempDir::new();
    let config = config(&root, "session-headless-replay", false);
    let result = run_headless(config.clone(), TASK.into())
        .await
        .expect("recorded run succeeds");
    let header = JsonlSessionPersistence::new(&config.data_dir)
        .load(&config.session_id, cancellation())
        .await
        .expect("durable header loads")
        .expect("session exists");

    let mut replacements = ReplacementMap::from([
        ("session-headless-replay".into(), "<session-id>".into()),
        (
            root.path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "<cwd>".into(),
        ),
    ]);
    for event in &result.events {
        if let Some(id) = event.data.pointer("/message/id").and_then(Value::as_str) {
            replacements.insert(id.into(), "<session-id>".into());
        }
    }
    let rust = normalize_session_trace(header, &result.events, &replacements)
        .expect("rust suffix serializes for oracle comparison");
    let typescript: Value =
        serde_json::from_str(TYPESCRIPT_NORMALIZED).expect("oracle fixture parses");

    assert_eq!(
        overlapping_semantics(&rust),
        overlapping_semantics(&typescript)
    );
}

fn overlapping_semantics(trace: &Value) -> Vec<Value> {
    trace["events"]
        .as_array()
        .expect("trace has event array")
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some(
                    "assistant/chunk"
                        | "assistant/message"
                        | "tool/call"
                        | "tool/result"
                        | "step/end"
                        | "turn/end"
                )
            )
        })
        .map(|event| {
            let mut event = event.clone();
            let object = event.as_object_mut().expect("event is an object");
            object.remove("seq");
            object.remove("time");
            object.remove("sourceEventSeqs");
            if object.get("type").and_then(Value::as_str) == Some("tool/result") {
                if let Some(data) = object.get_mut("data").and_then(Value::as_object_mut) {
                    data.remove("meta");
                }
            }
            event
        })
        .collect()
}

fn assert_fixture_event_invariants(events: &[SessionEvent]) {
    assert!(!events.is_empty(), "fixture run records events");
    let first_seq = events[0].seq;
    for (offset, event) in events.iter().enumerate() {
        assert_eq!(
            event.seq,
            first_seq + offset as u64,
            "event sequence is contiguous"
        );
    }

    assert_balanced_brackets(events);
    assert_user_message_shapes(events);
    assert_tool_correlations(events);
    assert_assistant_chunk_sources(events);
}

fn assert_balanced_brackets(events: &[SessionEvent]) {
    let mut active_turn = None;
    let mut active_step = None;

    for event in events {
        match event.event_type.as_str() {
            "turn/start" => {
                assert!(active_turn.is_none() && active_step.is_none());
                active_turn = Some(event.data["turn"].as_u64().expect("turn start has turn"));
            }
            "step/start" => {
                let turn = event.data["turn"].as_u64().expect("step start has turn");
                let step = event.data["step"].as_u64().expect("step start has step");
                assert_eq!(active_turn, Some(turn));
                assert!(active_step.is_none());
                active_step = Some(step);
            }
            "step/end" => {
                assert_eq!(event.data["turn"].as_u64(), active_turn);
                assert_eq!(event.data["step"].as_u64(), active_step);
                active_step = None;
            }
            "turn/end" => {
                assert_eq!(event.data["turn"].as_u64(), active_turn);
                assert!(active_step.is_none());
                active_turn = None;
            }
            _ => {
                let has_turn = event.data.get("turn").is_some();
                let has_step = event.data.get("step").is_some();
                assert_eq!(has_turn, has_step, "turn and step travel together");
                if has_turn {
                    assert_eq!(event.data["turn"].as_u64(), active_turn);
                    assert_eq!(event.data["step"].as_u64(), active_step);
                }
            }
        }
    }

    assert!(active_turn.is_none() && active_step.is_none());
}

fn assert_user_message_shapes(events: &[SessionEvent]) {
    let messages = events
        .iter()
        .filter(|event| event.event_type == "user/message")
        .collect::<Vec<_>>();
    assert!(!messages.is_empty(), "fixture emits its user message");

    for event in messages {
        let data = event
            .data
            .as_object()
            .expect("user message data is an object");
        assert_eq!(data.len(), 4, "user message is the Message object itself");
        assert!(data["id"].is_string());
        assert_eq!(data["role"], "user");
        assert!(data["content"].is_array());
        assert!(data["source"].is_object());
        assert!(data.get("message").is_none());
        assert!(data.get("turn").is_none());
        assert!(data.get("step").is_none());
    }
}

fn assert_request_header_shapes(events: &[SessionEvent], expected_reasons: &[&str]) {
    let headers = events
        .iter()
        .filter(|event| event.event_type == "request/header")
        .collect::<Vec<_>>();
    assert_eq!(
        headers.len(),
        expected_reasons.len(),
        "no redundant unchanged request header is appended"
    );

    for (event, reason) in headers.into_iter().zip(expected_reasons) {
        let data = event
            .data
            .as_object()
            .expect("request header data is an object");
        assert_eq!(
            data.len(),
            2,
            "request header has exactly header and reason"
        );
        assert!(data["header"].is_object());
        assert_eq!(data["reason"], *reason);
        assert!(data.get("turn").is_none());
        assert!(data.get("step").is_none());
    }
}

fn assert_tool_correlations(events: &[SessionEvent]) {
    let calls = events
        .iter()
        .filter(|event| event.event_type == "tool/call")
        .collect::<Vec<_>>();
    let results = events
        .iter()
        .filter(|event| event.event_type == "tool/result")
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), results.len(), "each tool call has one result");

    for call in calls {
        let call_id = call.data["callId"].as_str().expect("tool call has callId");
        let turn = call.data["turn"].as_u64().expect("tool call has turn");
        let step = call.data["step"].as_u64().expect("tool call has step");
        let matches = results
            .iter()
            .filter(|result| {
                result.data["turn"].as_u64() == Some(turn)
                    && result.data["step"].as_u64() == Some(step)
                    && result
                        .data
                        .pointer("/message/source/callId")
                        .and_then(Value::as_str)
                        == Some(call_id)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            matches.len(),
            1,
            "tool result matches one tool call and step"
        );
        let result = *matches[0];
        assert_eq!(result.source_event_seqs.as_deref(), Some(&[call.seq][..]));
        assert_eq!(
            result
                .data
                .pointer("/message/content/0/toolCallId")
                .and_then(Value::as_str),
            Some(call_id)
        );
    }
}

fn assert_assistant_chunk_sources(events: &[SessionEvent]) {
    for assistant in events
        .iter()
        .filter(|event| event.event_type == "assistant/message")
    {
        let turn = assistant.data["turn"]
            .as_u64()
            .expect("assistant message has turn");
        let step = assistant.data["step"]
            .as_u64()
            .expect("assistant message has step");
        let chunks = events
            .iter()
            .filter(|event| {
                event.event_type == "assistant/chunk"
                    && event.data["turn"].as_u64() == Some(turn)
                    && event.data["step"].as_u64() == Some(step)
            })
            .map(|event| event.seq)
            .collect::<Vec<_>>();
        assert!(!chunks.is_empty(), "assistant message has source chunks");
        assert_eq!(
            assistant.source_event_seqs.as_deref(),
            Some(chunks.as_slice())
        );
    }
}

use serde_json::{json, Value};
use tessivum::agent_mode::AgentModeId;
use tessivum::protocol::{
    ContentBlock, EpochHeader, FinishReason, LlmCallConfig, LlmCallConfigAdapterDefaults,
    LlmFailure, Message, MessageId, MessageRole, MessageSource, ProviderRequestId, RequestContext,
    SessionEvent, SessionHeader, StreamChunk, SurfaceOp, TokenUsage, ToolCallId, ToolSchema,
};

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
fn session_headers_serialize_native_modes_and_migrate_only_builtin_presets() {
    let header = SessionHeader {
        version: 0,
        id: "native-mode".into(),
        created_at: 1,
        cwd: None,
        parent_session: None,
        seed_length: None,
        origin: None,
        delegation_depth: None,
        agent_mode: Some(AgentModeId::ptc()),
    };
    assert_eq!(
        serde_json::to_value(&header).unwrap(),
        json!({"version": 0, "id": "native-mode", "createdAt": 1, "agentMode": "ptc"})
    );

    for (legacy_preset, agent_mode) in [
        ("standard", "standard"),
        ("code", "ptc"),
        ("minimal", "minimal"),
        ("cordis", "composition"),
    ] {
        let migrated: SessionHeader = serde_json::from_value(json!({
            "version": 0,
            "id": legacy_preset,
            "createdAt": 0,
            "agentPreset": legacy_preset,
        }))
        .unwrap();
        assert_eq!(
            migrated.agent_mode,
            Some(AgentModeId::new(agent_mode).unwrap())
        );
        assert_eq!(
            serde_json::to_value(migrated).unwrap()["agentMode"],
            json!(agent_mode)
        );
    }

    assert!(serde_json::from_value::<SessionHeader>(json!({
        "version": 0,
        "id": "ambiguous",
        "createdAt": 0,
        "agentMode": "standard",
        "agentPreset": "standard",
    }))
    .is_err());

    let error = serde_json::from_value::<SessionHeader>(json!({
        "version": 0,
        "id": "custom",
        "createdAt": 0,
        "agentPreset": "repository-maintainer",
    }))
    .unwrap_err()
    .to_string();
    let root = std::env::var_os("TESSIVUM_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".tessivum"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".tessivum"));
    assert!(error.contains("MODE_MIGRATION_REQUIRED"));
    assert!(error.contains(&format!(
        "{}/modes/repository-maintainer/mode.toml",
        root.display()
    )));
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

#[test]
fn llm_chunks_round_trip_every_variant_and_reject_provider_leakage() {
    let usage = TokenUsage {
        input_tokens: 11,
        output_tokens: 7,
        cache_read_tokens: Some(3),
        cache_write_tokens: Some(2),
        reasoning_tokens: Some(5),
    };
    let failure = LlmFailure {
        message: "provider busy".into(),
        code: "RATE_LIMIT".into(),
        status: Some(429),
        provider_retry_after_ms: Some(250),
        request_id: Some("req-7".into()),
    };
    let chunks = vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".into(),
        },
        StreamChunk::TextDelta {
            index: 1,
            text: "visible".into(),
        },
        StreamChunk::ReasoningDelta {
            index: 2,
            text: "thinking".into(),
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: ToolCallId::from("call-1"),
            name: Some("lookup".into()),
            arguments_delta: "{\"q\":\"rust\"}".into(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: ToolCallId::from("call-1"),
                name: "lookup".into(),
                arguments: "{\"q\":\"rust\"}".into(),
            },
        },
        StreamChunk::Usage {
            usage: usage.clone(),
        },
        StreamChunk::Finish {
            reason: FinishReason::Stop,
            replay_state: Some(json!({"cursor": [1, 2, 3]})),
        },
    ];
    for chunk in &chunks {
        chunk.validate().expect("known chunk validates");
        let wire = serde_json::to_value(chunk).expect("chunk serializes");
        let round_trip: StreamChunk = serde_json::from_value(wire.clone()).expect("chunk parses");
        assert_eq!(round_trip, *chunk);
        assert_eq!(serde_json::to_value(round_trip).unwrap(), wire);
    }
    assert_eq!(
        serde_json::to_value(&chunks[3]).unwrap(),
        json!({
            "type":"tool-call-delta",
            "index":0,
            "id":"call-1",
            "name":"lookup",
            "argumentsDelta":"{\"q\":\"rust\"}"
        })
    );
    for reason in [
        FinishReason::Stop,
        FinishReason::ToolCalls,
        FinishReason::MaxTokens,
        FinishReason::Error {
            failure: failure.clone(),
        },
        FinishReason::Aborted { failure },
    ] {
        let chunk = StreamChunk::Finish {
            reason,
            replay_state: None,
        };
        chunk.validate().expect("terminal reason validates");
        let wire = serde_json::to_value(&chunk).unwrap();
        assert_eq!(serde_json::from_value::<StreamChunk>(wire).unwrap(), chunk);
    }

    assert!(serde_json::from_value::<StreamChunk>(json!({
        "type":"text-delta", "index":0, "text":"x", "providerMetadata":{}
    }))
    .is_err());
    assert!(serde_json::from_value::<StreamChunk>(json!({
        "type":"finish",
        "reason":{"kind":"error","failure":{"message":"busy","code":"RATE_LIMIT","vendor":{"retryable":true}}}
    }))
    .is_err());
    assert_eq!(
        StreamChunk::BlockStart {
            index: 0,
            block_type: "unknown".into(),
        }
        .validate()
        .unwrap_err()
        .code,
        "INVALID_STREAM_BLOCK_TYPE"
    );
    let incomplete_tool_identity = StreamChunk::ToolCallDelta {
        index: 0,
        id: ToolCallId::from(""),
        name: None,
        arguments_delta: String::new(),
    };
    incomplete_tool_identity
        .validate()
        .expect("raw adapter identity is lossless");
    assert_eq!(
        serde_json::to_value(incomplete_tool_identity).unwrap()["id"],
        json!("")
    );
    assert_eq!(
        LlmFailure {
            message: "retry immediately".into(),
            code: "RATE_LIMIT".into(),
            status: None,
            provider_retry_after_ms: Some(0),
            request_id: None,
        }
        .validate()
        .unwrap_err()
        .code,
        "INVALID_POSITIVE_VALUE"
    );
}

#[test]
fn model_message_blocks_round_trip_without_provider_fields() {
    let blocks = vec![
        ContentBlock::Text {
            text: "visible".into(),
        },
        ContentBlock::Reasoning {
            text: "private reasoning".into(),
        },
        ContentBlock::Image {
            attachment: json!({"id":"attachment-1","mediaType":"image/png"}),
        },
        ContentBlock::ToolCall {
            id: ToolCallId::from("call-1"),
            name: "lookup".into(),
            arguments: "{\"query\":\"rust\"}".into(),
        },
        ContentBlock::ToolResult {
            tool_call_id: ToolCallId::from("call-1"),
            content: vec![ContentBlock::Text {
                text: "result".into(),
            }],
            is_error: Some(false),
        },
    ];
    let message = Message {
        id: MessageId::from("message-1"),
        role: MessageRole::Assistant,
        content: blocks,
        source: MessageSource::Model {
            provider: "neutral-route".into(),
            model: "model-a".into(),
            replay_state: Some(json!({"cursor":"opaque"})),
        },
    };
    message.validate().expect("model message validates");
    let wire = serde_json::to_value(&message).unwrap();
    assert_eq!(
        serde_json::from_value::<Message>(wire.clone()).unwrap(),
        message
    );
    let mut leaked = wire;
    leaked["source"]["providerPayload"] = json!({"trace":"private"});
    assert!(serde_json::from_value::<Message>(leaked).is_err());
}

#[test]
fn prepared_header_is_exact_config_tool_snapshot_and_context_is_separate() {
    let tool = ToolSchema {
        name: "lookup".into(),
        description: "Fetch one result".into(),
        parameters: json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        }),
    };
    let header = EpochHeader {
        config: LlmCallConfig {
            provider: "neutral-route".into(),
            model: "model-a".into(),
            reasoning_effort: Some("high".into()),
            temperature: Some(0.2),
            max_tokens: Some(8192),
            stop: Some(vec!["<END>".into()]),
        },
        adapter_defaults: Some(LlmCallConfigAdapterDefaults {
            reasoning_effort: Some(true),
            max_tokens: Some(true),
        }),
        system: Some("Follow the request.".into()),
        tools: Some(vec![tool.clone()]),
    };
    header.validate().expect("prepared header validates");
    assert_eq!(
        serde_json::to_value(&header).unwrap(),
        json!({
            "config": {
                "provider": "neutral-route",
                "model": "model-a",
                "reasoningEffort": "high",
                "temperature": 0.2,
                "maxTokens": 8192,
                "stop": ["<END>"]
            },
            "adapterDefaults": {"reasoningEffort": true, "maxTokens": true},
            "system": "Follow the request.",
            "tools": [{
                "name": "lookup",
                "description": "Fetch one result",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            }]
        })
    );
    let round_trip: EpochHeader = serde_json::from_value(serde_json::to_value(&header).unwrap())
        .expect("prepared header parses");
    assert_eq!(round_trip, header);

    let context = RequestContext {
        provider: "neutral-route".into(),
        model: "model-a".into(),
        context_window: Some(128_000),
    };
    context
        .validate()
        .expect("separate route context validates");
    assert_eq!(
        serde_json::to_value(context).unwrap(),
        json!({"provider":"neutral-route", "model":"model-a", "contextWindow":128000})
    );

    let mut duplicate = header.clone();
    duplicate.tools = Some(vec![tool.clone(), tool]);
    assert_eq!(
        duplicate.validate().unwrap_err().code,
        "DUPLICATE_TOOL_SCHEMA"
    );
    let mut leaked = serde_json::to_value(&header).unwrap();
    leaked["providerOptions"] = json!({"temperatureMode":"vendor-private"});
    assert!(serde_json::from_value::<EpochHeader>(leaked).is_err());

    let mut empty_system = header.clone();
    empty_system.system = Some(String::new());
    assert_eq!(
        empty_system.validate().unwrap_err().code,
        "INVALID_EPOCH_HEADER"
    );
    let mut empty_tools = header.clone();
    empty_tools.tools = Some(Vec::new());
    assert_eq!(
        empty_tools.validate().unwrap_err().code,
        "INVALID_EPOCH_HEADER"
    );
    let mut invalid_stop = header.config.clone();
    invalid_stop.stop = Some(vec![String::new()]);
    assert_eq!(
        invalid_stop.validate().unwrap_err().code,
        "INVALID_STOP_SEQUENCES"
    );
    assert_eq!(
        LlmCallConfigAdapterDefaults {
            reasoning_effort: None,
            max_tokens: None,
        }
        .validate()
        .unwrap_err()
        .code,
        "INVALID_ADAPTER_DEFAULTS"
    );
}

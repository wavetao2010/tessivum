use std::{
    collections::VecDeque,
    fs,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures_util::stream;
use serde_json::{json, Value};
use tessivum::{
    agent::{AgentCancelCause, AgentOptions, AgentRegistry},
    agent_loop::AgentLoopFactory,
    agent_preset::AgentPresetService,
    llm::{LlmAdapter, LlmRetryPolicy, LlmRuntime, LlmStream, RecordedLlmAdapter},
    session::{MemorySessionPersistence, SessionStore},
    system_prompt::{PromptRegistration, PromptSection, SystemPrompt},
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRunContext, ToolRuntime,
    },
    ContentBlock, FinishReason, GenerateRequest, LlmFailure, Message, MessageRole, MessageSource,
    SessionEvent, SessionHeader, SessionId, SessionOrigin, StreamChunk, SurfaceOp, ToolCallId,
};
use tessivum_core::{CancellationToken, ContextHandle};

fn cancellation() -> CancellationToken {
    ContextHandle::root().scope().cancellation()
}

fn header(id: &str) -> SessionHeader {
    SessionHeader {
        version: 0,
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

fn user(id: &str) -> Message {
    Message {
        id: id.into(),
        role: MessageRole::User,
        content: vec![ContentBlock::Text { text: id.into() }],
        source: MessageSource::User {
            client_time_zone: None,
        },
    }
}

#[derive(Clone)]
struct DeterministicAdapter {
    streams: Arc<Mutex<VecDeque<Vec<StreamChunk>>>>,
}

#[async_trait]
impl LlmAdapter for DeterministicAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, tessivum::TessivumError> {
        Ok(Box::pin(stream::iter(
            self.streams
                .lock()
                .unwrap()
                .pop_front()
                .unwrap()
                .into_iter()
                .map(Ok),
        )))
    }
}

#[derive(Clone)]
struct RecordingAdapter {
    requests: Arc<parking_lot::Mutex<Vec<GenerateRequest>>>,
    streams: Arc<parking_lot::Mutex<VecDeque<Vec<StreamChunk>>>>,
}

#[async_trait]
impl LlmAdapter for RecordingAdapter {
    async fn generate(
        &self,
        request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, tessivum::TessivumError> {
        self.requests.lock().push(request);
        Ok(Box::pin(stream::iter(
            self.streams.lock().pop_front().unwrap().into_iter().map(Ok),
        )))
    }
}

struct BlockingAdapter;

#[async_trait]
impl LlmAdapter for BlockingAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, tessivum::TessivumError> {
        Ok(Box::pin(stream::pending()))
    }
}

struct Echo;

#[async_trait]
impl ToolHandler for Echo {
    async fn run(&self, _context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: arguments["value"].as_str().unwrap().into(),
            }],
            false,
            Value::Null,
        ))
    }
}

struct BlockingTool;

#[async_trait]
impl ToolHandler for BlockingTool {
    async fn run(&self, context: ToolRunContext, _arguments: Value) -> ToolHandlerResult {
        context.cancellation.cancelled().await;
        Ok(ToolOutput::new(Vec::new(), false, Value::Null))
    }
}

struct PromptChangingEcho {
    prompt: SystemPrompt,
    registration: Arc<Mutex<Option<PromptRegistration>>>,
}

#[async_trait]
impl ToolHandler for PromptChangingEcho {
    async fn run(&self, _context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let registration = self
            .prompt
            .register(PromptSection::new("changed", 0, "changed"))?;
        *self.registration.lock().unwrap() = Some(registration);
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: arguments["value"].as_str().unwrap().into(),
            }],
            false,
            Value::Null,
        ))
    }
}

fn tool_turn() -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".into(),
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: ToolCallId::from("call-1"),
            name: Some("echo".into()),
            arguments_delta: r#"{"value":"round-trip"}"#.into(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: ToolCallId::from("call-1"),
                name: "echo".into(),
                arguments: r#"{"value":"round-trip"}"#.into(),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
            replay_state: None,
        },
    ]
}

fn text_turn(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".into(),
        },
        StreamChunk::TextDelta {
            index: 0,
            text: text.into(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text { text: text.into() },
        },
        StreamChunk::Finish {
            reason: FinishReason::Stop,
            replay_state: None,
        },
    ]
}

async fn durable_events(adapter: Arc<dyn LlmAdapter>) -> Vec<SessionEvent> {
    let llm = LlmRuntime::new();
    let _provider = llm.register("test", adapter).unwrap();
    let tools = ToolRuntime::new();
    let _tool = tools
        .register(ToolDefinition::new(
            "echo",
            "echoes",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            Echo,
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(
            llm,
            SystemPrompt::new(),
            tools,
        )))
        .unwrap();
    let agent = registry
        .create(
            header("replay-equivalence"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    agent.followup(user("question")).await.unwrap();
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    agent.dispose().await.unwrap();
    events
}

fn normalize_generated_message_ids(events: Vec<SessionEvent>) -> Vec<Value> {
    events
        .into_iter()
        .map(|event| {
            let mut value = serde_json::to_value(event).unwrap();
            value["time"] = json!("<generated>");
            if let Some(message) = value["data"]["message"].as_object_mut() {
                message.insert("id".into(), json!("<generated>"));
            }
            value
        })
        .collect()
}

#[tokio::test]
async fn recorded_replay_matches_a_native_adapter_through_the_durable_tool_loop() {
    let native = Arc::new(DeterministicAdapter {
        streams: Arc::new(Mutex::new(VecDeque::from([
            tool_turn(),
            text_turn("native and replay agree"),
        ]))),
    });
    let recording = [
        json!({
            "sessionId": "replay-equivalence",
            "provider": "test",
            "model": "deterministic",
            "requestId": "tool",
            "chunks": tool_turn(),
        }),
        json!({
            "sessionId": "replay-equivalence",
            "provider": "test",
            "model": "deterministic",
            "requestId": "text",
            "chunks": text_turn("native and replay agree"),
        }),
    ]
    .into_iter()
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    let replay = Arc::new(RecordedLlmAdapter::from_jsonl(&recording).unwrap());

    let native_events = durable_events(native).await;
    let replay_events = durable_events(replay.clone()).await;
    assert!(native_events.iter().all(|event| event.time > 0));
    assert!(replay_events.iter().all(|event| event.time > 0));
    assert_eq!(
        normalize_generated_message_ids(replay_events),
        normalize_generated_message_ids(native_events),
    );
    replay.assert_consumed().unwrap();
}

fn failed_turn(code: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".into(),
        },
        StreamChunk::TextDelta {
            index: 0,
            text: "discarded partial output".into(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text {
                text: "discarded partial output".into(),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::Error {
                failure: LlmFailure {
                    message: "transient provider failure".into(),
                    code: code.into(),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                },
            },
            replay_state: None,
        },
    ]
}

fn failed_tool_turn() -> Vec<StreamChunk> {
    let mut chunks = tool_turn();
    *chunks.last_mut().unwrap() = StreamChunk::Finish {
        reason: FinishReason::Error {
            failure: LlmFailure {
                message: "malformed provider termination".into(),
                code: "MALFORMED_STREAM".into(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            },
        },
        replay_state: None,
    };
    chunks
}

#[tokio::test]
async fn durable_tool_round_trip_records_balanced_model_ordered_events() {
    let llm = LlmRuntime::new();
    let adapter = DeterministicAdapter {
        streams: Arc::new(Mutex::new(VecDeque::from([tool_turn(), text_turn("done")]))),
    };
    let _provider = llm.register("test", Arc::new(adapter)).unwrap();
    let tools = ToolRuntime::new();
    let _tool = tools
        .register(ToolDefinition::new(
            "echo",
            "echoes",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            Echo,
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(
            llm,
            SystemPrompt::new(),
            tools,
        )))
        .unwrap();
    let agent = registry
        .create(
            header("round-trip"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("question")).await.unwrap();
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    assert_eq!(
        events
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
            "assistant/message",
            "tool/call",
            "tool/result",
            "step/end",
            "step/start",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/message",
            "step/end",
            "turn/end",
        ],
    );
    let user_message = events
        .iter()
        .find(|event| event.event_type == "user/message")
        .unwrap();
    assert_eq!(
        user_message.data,
        serde_json::to_value(user("question")).unwrap()
    );
    assert_eq!(user_message.surface_op, Some(SurfaceOp::Append));
    assert_eq!(user_message.source_event_seqs, None);
    let request_headers = events
        .iter()
        .filter(|event| event.event_type == "request/header")
        .collect::<Vec<_>>();
    assert_eq!(request_headers.len(), 1);
    assert_eq!(
        request_headers[0].data,
        json!({
            "header": {
                "config": {"provider": "test", "model": "deterministic"},
                "tools": [{
                    "name": "echo",
                    "description": "echoes",
                    "parameters": {"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}
                }]
            },
            "reason": "initial"
        })
    );
    let assistant = events
        .iter()
        .find(|event| event.event_type == "assistant/message")
        .unwrap();
    assert_eq!(assistant.source_event_seqs.as_ref().unwrap().len(), 4);
    assert_eq!(
        agent.session().derive_messages().last().unwrap().content,
        vec![ContentBlock::Text {
            text: "done".into()
        }],
    );
    assert!(events.iter().all(
        |event| event.event_type != "turn/end" || event.data["reason"]["kind"] != "interrupted"
    ));
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn durable_inbox_claims_precede_their_fifo_user_messages() {
    let llm = LlmRuntime::new();
    let adapter = DeterministicAdapter {
        streams: Arc::new(Mutex::new(VecDeque::from([
            text_turn("first"),
            text_turn("second"),
        ]))),
    };
    let _provider = llm.register("test", Arc::new(adapter)).unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("durable-inbox-claims"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let steering = user("steering");
    let followup = user("followup");
    let session = agent.session();
    for (target, message) in [("next-step", &steering), ("next-turn", &followup)] {
        session
            .append(
                SessionEvent {
                    event_type: "agent/inbox/enqueued".into(),
                    seq: session.next_seq().unwrap(),
                    time: 0,
                    data: json!({"target": target, "message": message}),
                    ignorable: None,
                    source_event_seqs: None,
                    surface_op: None,
                },
                cancellation(),
            )
            .await
            .unwrap();
    }
    agent.steer(steering).await.unwrap();
    agent.followup(followup).await.unwrap();
    agent.when_idle().await.unwrap();

    let events = session.events();
    let claims = events
        .iter()
        .filter(|event| event.event_type == "agent/inbox/spliced")
        .map(|event| event.data.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        claims,
        vec![
            json!({"target": "next-turn", "start": 0, "removedCount": 1, "inserted": []}),
            json!({"target": "next-step", "start": 0, "removedCount": 1, "inserted": []}),
        ]
    );
    let claimed = events
        .iter()
        .filter(|event| event.event_type == "user/message")
        .filter_map(|event| event.data["id"].as_str())
        .filter(|id| matches!(*id, "steering" | "followup"))
        .collect::<Vec<_>>();
    assert_eq!(claimed, vec!["steering", "followup"]);
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn changed_effective_header_emits_change_event() {
    let llm = LlmRuntime::new();
    let adapter = DeterministicAdapter {
        streams: Arc::new(Mutex::new(VecDeque::from([tool_turn(), text_turn("done")]))),
    };
    let _provider = llm.register("test", Arc::new(adapter)).unwrap();
    let tools = ToolRuntime::new();
    let prompt = SystemPrompt::new();
    let registrations = Arc::new(Mutex::new(None));
    let _tool = tools
        .register(ToolDefinition::new(
            "echo",
            "echoes",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            PromptChangingEcho {
                prompt: prompt.clone(),
                registration: Arc::clone(&registrations),
            },
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(llm, prompt, tools)))
        .unwrap();
    let agent = registry
        .create(
            header("changed-header"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("question")).await.unwrap();
    agent.when_idle().await.unwrap();
    let headers = agent
        .session()
        .events()
        .into_iter()
        .filter(|event| event.event_type == "request/header")
        .collect::<Vec<_>>();
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].data["reason"], "initial");
    assert_eq!(headers[1].data["reason"], "change");
    assert_eq!(
        headers[0]
            .data
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["header", "reason"]
    );
    assert_eq!(
        headers[1]
            .data
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["header", "reason"]
    );
    assert_ne!(headers[0].data["header"], headers[1].data["header"]);
    assert_eq!(headers[1].data["header"]["system"], "changed");
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn preloaded_request_header_makes_first_runtime_header_resume() {
    let store = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let session = store
        .create(header("resumed"), cancellation())
        .await
        .unwrap();
    session
        .append(
            SessionEvent {
                event_type: "request/header".into(),
                seq: 0,
                time: 0,
                data: json!({
                    "header": {"config": {"provider": "previous", "model": "previous"}},
                    "reason": "initial"
                }),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let llm = LlmRuntime::new();
    let adapter = DeterministicAdapter {
        streams: Arc::new(Mutex::new(VecDeque::from([text_turn("resumed")]))),
    };
    let _provider = llm.register("test", Arc::new(adapter)).unwrap();
    let registry = AgentRegistry::new(store);
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .resume(
            SessionId::from("resumed"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("question")).await.unwrap();
    agent.when_idle().await.unwrap();
    let headers = agent
        .session()
        .events()
        .into_iter()
        .filter(|event| event.event_type == "request/header")
        .collect::<Vec<_>>();
    assert_eq!(headers.len(), 2);
    assert_eq!(
        headers[1].data,
        json!({
            "header": {"config": {"provider": "test", "model": "deterministic"}},
            "reason": "resume"
        })
    );
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn retry_preserves_partial_chunks_without_committing_or_executing_them() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register_with_retry_policy(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([
                    failed_turn("TRANSPORT"),
                    text_turn("recovered"),
                ]))),
            }),
            Some(LlmRetryPolicy::resolve(None).unwrap()),
        )
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("partial-retry"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("retry-input")).await.unwrap();
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    let retry_seq = events
        .iter()
        .find(|event| event.event_type == "llm/retry")
        .unwrap()
        .seq;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/chunk" && event.seq < retry_seq)
            .count(),
        4
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/message")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "llm/retry-started")
            .count(),
        1
    );
    assert!(events.iter().all(|event| event.event_type != "tool/call"));
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn retry_budget_is_reconstructed_from_the_durable_ledger() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register_with_retry_policy(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([
                    failed_turn("SERVER"),
                    failed_turn("SERVER"),
                    failed_turn("SERVER"),
                ]))),
            }),
            Some(LlmRetryPolicy::resolve(None).unwrap()),
        )
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("retry-exhausted"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("retry-input")).await.unwrap();
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "llm/retry")
            .count(),
        2
    );
    let retry = events
        .iter()
        .find(|event| event.event_type == "llm/retry")
        .unwrap();
    assert_eq!(retry.data["mode"], "normal");
    assert_eq!(retry.data["maxRetries"], 2);
    assert!(retry.data["policyKey"].is_string());
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "step/end")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "turn/end")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "error");
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn cancellation_during_backoff_wins_without_starting_another_attempt() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register_with_retry_policy(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([
                    failed_turn("RATE_LIMIT"),
                    text_turn("must not run"),
                ]))),
            }),
            Some(LlmRetryPolicy::resolve(None).unwrap()),
        )
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("cancel-retry"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let session = agent.session();
    let mut updates = session.subscribe();

    agent.followup(user("retry-input")).await.unwrap();
    loop {
        if updates.recv().await.unwrap().event_type == "llm/retry" {
            break;
        }
    }
    assert!(agent.cancel(AgentCancelCause::User, false));
    agent.when_idle().await.unwrap();
    let events = session.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "llm/retry-started")
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "step/end")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "aborted");
    assert_eq!(
        events.last().unwrap().data["reason"]["reason"]["kind"],
        "user"
    );
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn cancellation_during_provider_wait_closes_one_step_and_turn() {
    let llm = LlmRuntime::new();
    let _provider = llm.register("test", Arc::new(BlockingAdapter)).unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("cancel-provider"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let session = agent.session();
    let mut updates = session.subscribe();

    agent.followup(user("wait")).await.unwrap();
    loop {
        if updates.recv().await.unwrap().event_type == "step/start" {
            break;
        }
    }
    assert!(agent.cancel(AgentCancelCause::User, false));
    agent.when_idle().await.unwrap();
    let events = session.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/message")
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "step/end")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "turn/end")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "aborted");
    assert_eq!(
        events.last().unwrap().data["reason"]["reason"]["kind"],
        "user"
    );
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn cancellation_during_tool_wait_settles_the_started_call_once() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([tool_turn()]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _tool = tools
        .register(ToolDefinition::new(
            "echo",
            "waits",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            BlockingTool,
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(
            llm,
            SystemPrompt::new(),
            tools,
        )))
        .unwrap();
    let agent = registry
        .create(
            header("cancel-tool"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let session = agent.session();
    let mut updates = session.subscribe();

    agent.followup(user("tool")).await.unwrap();
    loop {
        if updates.recv().await.unwrap().event_type == "tool/call" {
            break;
        }
    }
    assert!(agent.cancel(AgentCancelCause::User, false));
    agent.when_idle().await.unwrap();
    let events = session.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool/call")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool/result")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "step/end")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "turn/end")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "aborted");
    assert_eq!(
        events.last().unwrap().data["reason"]["reason"]["kind"],
        "user"
    );
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn failed_tool_stream_never_starts_durable_tool_lifecycle() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([failed_tool_turn()]))),
            }),
        )
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(AgentLoopFactory::new(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("failed-tool-stream"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("tool")).await.unwrap();
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/chunk")
            .count(),
        4
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/message")
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool/call")
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool/result")
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "step/end")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "turn/end")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "error");
    agent.dispose().await.unwrap();
}

const MINIMAL_COMPOSITION: &str = r#"
- name: '@deepseek-ai/dsh-persona'
  config:
    text: You are a helpful software engineer assistant.
    complete: true
- name: cordis:group
  group: true
  config:
    - name: '@deepseek-ai/dsh-tool-bash-persistent'
    - name: '@deepseek-ai/dsh-tool-str-replace-editor'
    - name: '@deepseek-ai/dsh-tool-fs'
      disabled: true
"#;

const MINIMAL_WITHOUT_SHELL: &str = r#"
- name: '@deepseek-ai/dsh-persona'
  config:
    text: You are a helpful software engineer assistant.
    complete: true
- name: '@deepseek-ai/dsh-tool-str-replace-editor'
"#;

fn install_tools(runtime: &ToolRuntime, names: &[&str]) -> Vec<tessivum::tools::ToolRegistration> {
    names
        .iter()
        .map(|name| {
            runtime
                .register(ToolDefinition::new(
                    *name,
                    *name,
                    json!({"type":"object","properties":{},"additionalProperties":false}),
                    Echo,
                ))
                .unwrap()
        })
        .collect()
}

async fn request_once(registry: &AgentRegistry, mut session: SessionHeader, preset: Option<&str>) {
    session.agent_preset = preset.map(str::to_owned);
    let agent = registry
        .create(
            session,
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    agent.followup(user("request")).await.unwrap();
    agent.when_idle().await.unwrap();
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn copied_minimal_preset_owns_its_prompt_and_tools_after_edits() {
    let root =
        std::env::temp_dir().join(format!("tessivum-preset-catalog-{}", uuid::Uuid::new_v4()));
    let system = root.join("system");
    let user = root.join("user");
    fs::create_dir_all(system.join("minimal")).unwrap();
    fs::write(system.join("minimal/agent.cordis.yml"), MINIMAL_COMPOSITION).unwrap();
    let presets = Arc::new(AgentPresetService::new(system.clone(), user.clone()));
    presets
        .copy("minimal", "renamed-minimal", None)
        .await
        .unwrap();

    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([
                    text_turn("first"),
                    text_turn("second"),
                ]))),
            }),
        )
        .unwrap();
    let prompt = SystemPrompt::new();
    let _host_prompt = prompt
        .register(PromptSection::new("host", 0, "host prompt"))
        .unwrap();
    let tools = ToolRuntime::new();
    let _tools = install_tools(&tools, &["bash", "str_replace_editor", "read"]);
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            AgentLoopFactory::new(llm, prompt, tools.clone())
                .with_dispatch_tools(tools)
                .with_presets(Arc::clone(&presets))
                .with_standard_catalog(),
        ))
        .unwrap();

    request_once(&registry, header("copied-minimal"), Some("renamed-minimal")).await;
    fs::write(
        user.join("renamed-minimal/agent.cordis.yml"),
        MINIMAL_WITHOUT_SHELL,
    )
    .unwrap();
    request_once(&registry, header("removed-tool"), Some("renamed-minimal")).await;

    let requests = requests.lock();
    assert_eq!(
        requests[0].system.as_deref(),
        Some("You are a helpful software engineer assistant.")
    );
    assert_eq!(
        requests[0]
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["bash", "str_replace_editor"]
    );
    assert_eq!(
        requests[1]
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["str_replace_editor"]
    );
    drop(requests);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn code_mode_minimal_keeps_the_run_code_catalog_without_broadening() {
    let root = std::env::temp_dir().join(format!("tessivum-code-preset-{}", uuid::Uuid::new_v4()));
    let system = root.join("system");
    let user = root.join("user");
    fs::create_dir_all(system.join("minimal")).unwrap();
    fs::write(system.join("minimal/agent.cordis.yml"), MINIMAL_COMPOSITION).unwrap();
    let presets = Arc::new(AgentPresetService::new(system.clone(), user.clone()));
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([text_turn("ok")]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _tools = install_tools(&tools, &["bash", "str_replace_editor", "run_code"]);
    let direct = tools
        .scoped(tessivum::tools::ToolRestrictions::allow_only(["run_code"]))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            AgentLoopFactory::new(llm, SystemPrompt::new(), direct)
                .with_dispatch_tools(tools)
                .with_presets(presets)
                .with_code_mode(),
        ))
        .unwrap();

    request_once(&registry, header("code-minimal"), Some("minimal")).await;
    let requests = requests.lock();
    assert_eq!(
        requests[0]
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["run_code"]
    );
    drop(requests);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn native_child_requests_exclude_owner_bound_tools() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([text_turn("ok")]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _tools = install_tools(
        &tools,
        &[
            "ask_user_question",
            "bash",
            "create_goal",
            "exit_plan_mode",
            "get_goal",
            "jobs.kill",
            "jobs.list",
            "jobs.read",
            "jobs.wait",
            "read",
            "schedule_create",
            "schedule_delete",
            "schedule_list",
            "todo_write",
            "update_goal",
        ],
    );
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            AgentLoopFactory::new(llm, SystemPrompt::new(), tools.clone())
                .with_dispatch_tools(tools)
                .with_standard_catalog(),
        ))
        .unwrap();
    let mut child = header("native-child");
    child.origin = Some(SessionOrigin::Subagent);
    request_once(&registry, child, None).await;

    let requests = requests.lock();
    assert_eq!(
        requests[0]
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["read"]
    );
}

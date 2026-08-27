use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures_util::stream;
use serde_json::{json, Value};
use tessivum::{
    agent::{AgentCancelCause, AgentError, AgentOptions, AgentRegistry},
    agent_loop::AgentLoopFactory,
    agent_mode::{AgentModeId, AgentModeRegistry},
    code_runtime::{ProcessCodeRuntime, ProcessCodeRuntimeConfig},
    llm::{LlmAdapter, LlmRetryPolicy, LlmRuntime, LlmStream, RecordedLlmAdapter},
    session::{MemorySessionPersistence, SessionStore},
    system_prompt::{PromptRegistration, PromptSection, SystemPrompt},
    tools::{
        ToolApproval, ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRunContext,
        ToolRuntime,
    },
    ContentBlock, FinishReason, GenerateRequest, LlmFailure, Message, MessageRole, MessageSource,
    SessionEvent, SessionHeader, SessionId, SessionOrigin, StreamChunk, SurfaceOp, ToolCallId,
    ToolSchema,
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
        agent_mode: None,
    }
}

fn modes() -> Arc<AgentModeRegistry> {
    Arc::new(AgentModeRegistry::with_roots(Vec::new(), None))
}

fn factory(llm: LlmRuntime, prompt: SystemPrompt, tools: ToolRuntime) -> AgentLoopFactory {
    AgentLoopFactory::new(llm, prompt, tools, modes(), AgentModeId::standard())
}

fn ptc_runtime() -> ProcessCodeRuntime {
    ProcessCodeRuntime::new(ProcessCodeRuntimeConfig::ptc_javascript().unwrap()).unwrap()
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
            name: Some("read".into()),
            arguments_delta: r#"{"value":"round-trip"}"#.into(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: ToolCallId::from("call-1"),
                name: "read".into(),
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
            "read",
            "reads",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            Echo,
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, SystemPrompt::new(), tools)))
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
            "read",
            "reads",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            Echo,
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, SystemPrompt::new(), tools)))
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
                "system": "Use the additive Tessivum persona, workspace instructions, and runtime context.",
                "tools": [{
                    "name": "read",
                    "description": "reads",
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
        .register_factory(Arc::new(factory(
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
            "read",
            "reads",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            PromptChangingEcho {
                prompt: prompt.clone(),
                registration: Arc::clone(&registrations),
            },
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, prompt, tools)))
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
    assert_eq!(
        headers[1].data["header"]["system"],
        "Use the additive Tessivum persona, workspace instructions, and runtime context.\n\nchanged"
    );
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
        .register_factory(Arc::new(factory(
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
            "header": {
                "config": {"provider": "test", "model": "deterministic"},
                "system": "Use the additive Tessivum persona, workspace instructions, and runtime context."
            },
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
        .register_factory(Arc::new(factory(
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
        .register_factory(Arc::new(factory(
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
        .register_factory(Arc::new(factory(
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
        .register_factory(Arc::new(factory(
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
            "read",
            "waits",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            BlockingTool,
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, SystemPrompt::new(), tools)))
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
        .register_factory(Arc::new(factory(
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

fn install_tools(runtime: &ToolRuntime, names: &[&str]) -> Vec<tessivum::tools::ToolRegistration> {
    names
        .iter()
        .map(|name| {
            runtime
                .register(ToolDefinition::new(
                    *name,
                    *name,
                    json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}},"additionalProperties":false}),
                    Echo,
                ))
                .unwrap()
        })
        .collect()
}

fn options() -> AgentOptions {
    AgentOptions {
        provider: "test".into(),
        model: "deterministic".into(),
        reasoning_effort: None,
        max_tokens: None,
    }
}

fn tool_names(request: &GenerateRequest) -> Vec<String> {
    request
        .tools
        .as_ref()
        .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
        .unwrap_or_default()
}

fn request_for<'a>(requests: &'a [GenerateRequest], session: &str) -> &'a GenerateRequest {
    requests
        .iter()
        .find(|request| {
            request
                .session_id
                .as_ref()
                .is_some_and(|id| id.as_str() == session)
        })
        .unwrap()
}

fn run_code_turn(code: &str) -> Vec<StreamChunk> {
    let arguments = json!({"description":"nested test","code":code}).to_string();
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".into(),
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: ToolCallId::from("code-call"),
            name: Some("run_code".into()),
            arguments_delta: arguments.clone(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: ToolCallId::from("code-call"),
                name: "run_code".into(),
                arguments,
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
            replay_state: None,
        },
    ]
}

#[tokio::test]
async fn four_session_runtime_specs_are_isolated() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([
                    text_turn("standard"),
                    text_turn("minimal"),
                    text_turn("composition"),
                    text_turn("ptc"),
                ]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _tools = install_tools(
        &tools,
        &[
            "bash",
            "composition_define",
            "composition_inspect",
            "composition_run",
            "composition_stop",
            "composition_validate",
            "exit_plan_mode",
            "read",
            "str_replace_editor",
            "todo_write",
        ],
    );
    let prompt = SystemPrompt::new();
    let _host_prompt = prompt
        .register(PromptSection::new("host", 0, "host prompt"))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            AgentLoopFactory::new(llm, prompt, tools, modes(), AgentModeId::standard())
                .with_code_runtime(ptc_runtime()),
        ))
        .unwrap();

    let standard = registry
        .create(header("mode-standard"), options(), cancellation())
        .await
        .unwrap();
    let mut minimal_header = header("mode-minimal");
    minimal_header.agent_mode = Some(AgentModeId::minimal());
    let minimal = registry
        .create(minimal_header, options(), cancellation())
        .await
        .unwrap();
    let mut composition_header = header("mode-composition");
    composition_header.agent_mode = Some(AgentModeId::composition());
    let composition = registry
        .create(composition_header, options(), cancellation())
        .await
        .unwrap();
    let mut ptc_header = header("mode-ptc");
    ptc_header.agent_mode = Some(AgentModeId::ptc());
    let ptc = registry
        .create(ptc_header, options(), cancellation())
        .await
        .unwrap();

    let (standard_result, minimal_result, composition_result, ptc_result) = tokio::join!(
        standard.followup(user("standard")),
        minimal.followup(user("minimal")),
        composition.followup(user("composition")),
        ptc.followup(user("ptc")),
    );
    standard_result.unwrap();
    minimal_result.unwrap();
    composition_result.unwrap();
    ptc_result.unwrap();
    let (standard_idle, minimal_idle, composition_idle, ptc_idle) = tokio::join!(
        standard.when_idle(),
        minimal.when_idle(),
        composition.when_idle(),
        ptc.when_idle(),
    );
    standard_idle.unwrap();
    minimal_idle.unwrap();
    composition_idle.unwrap();
    ptc_idle.unwrap();

    let requests = requests.lock();
    assert_eq!(
        tool_names(request_for(&requests, "mode-standard")),
        [
            "bash",
            "exit_plan_mode",
            "read",
            "str_replace_editor",
            "todo_write"
        ]
    );
    assert_eq!(
        tool_names(request_for(&requests, "mode-minimal")),
        ["bash", "str_replace_editor"]
    );
    assert_eq!(
        tool_names(request_for(&requests, "mode-composition")),
        [
            "bash",
            "composition_define",
            "composition_inspect",
            "composition_run",
            "composition_stop",
            "composition_validate",
            "exit_plan_mode",
            "read",
            "str_replace_editor",
            "todo_write",
        ]
    );
    assert_eq!(tool_names(request_for(&requests, "mode-ptc")), ["run_code"]);
    drop(requests);
    standard.dispose().await.unwrap();
    minimal.dispose().await.unwrap();
    composition.dispose().await.unwrap();
    ptc.dispose().await.unwrap();
}

#[tokio::test]
async fn complete_mode_replaces_host_prompt_while_additive_mode_contributes_once() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([
                    text_turn("standard"),
                    text_turn("minimal"),
                ]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _tools = install_tools(&tools, &["bash", "read", "str_replace_editor"]);
    let prompt = SystemPrompt::new();
    let _host_prompt = prompt
        .register(PromptSection::new("host", 0, "host prompt"))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, prompt, tools)))
        .unwrap();
    let standard = registry
        .create(header("prompt-standard"), options(), cancellation())
        .await
        .unwrap();
    let mut minimal_header = header("prompt-minimal");
    minimal_header.agent_mode = Some(AgentModeId::minimal());
    let minimal = registry
        .create(minimal_header, options(), cancellation())
        .await
        .unwrap();

    standard.followup(user("standard")).await.unwrap();
    minimal.followup(user("minimal")).await.unwrap();
    standard.when_idle().await.unwrap();
    minimal.when_idle().await.unwrap();
    let requests = requests.lock();
    assert_eq!(
        request_for(&requests, "prompt-standard").system.as_deref(),
        Some("Use the additive Tessivum persona, workspace instructions, and runtime context.\n\nhost prompt")
    );
    assert_eq!(
        request_for(&requests, "prompt-minimal").system.as_deref(),
        Some("Use only bash and str_replace_editor to complete the task.")
    );
    drop(requests);
    standard.dispose().await.unwrap();
    minimal.dispose().await.unwrap();
}

#[tokio::test]
async fn latest_mode_selection_event_overrides_header_and_programmatic_catalog_cannot_recurse() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([text_turn("ptc")]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _tools = install_tools(&tools, &["read", "run_code"]);
    let store = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let registry = AgentRegistry::new(store.clone());
    let _factory = registry
        .register_factory(Arc::new(
            factory(llm, SystemPrompt::new(), tools).with_code_runtime(ptc_runtime()),
        ))
        .unwrap();
    let mut selected = header("mode-event-wins");
    selected.agent_mode = Some(AgentModeId::minimal());
    selected.seed_length = Some(1);
    store
        .create_seeded(
            selected,
            vec![SessionEvent {
                event_type: "agent-mode/selected".into(),
                seq: 0,
                time: 0,
                data: json!({"agentMode":"ptc"}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            }],
            cancellation(),
        )
        .await
        .unwrap();
    let agent = registry
        .resume(
            SessionId::from("mode-event-wins"),
            options(),
            cancellation(),
        )
        .await
        .unwrap();
    agent.followup(user("ptc")).await.unwrap();
    agent.when_idle().await.unwrap();

    let requests = requests.lock();

    assert_eq!(
        tool_names(request_for(&requests, "mode-event-wins")),
        ["run_code"]
    );
    drop(requests);
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn programmatic_mode_requires_configured_code_runtime() {
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(
            LlmRuntime::new(),
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let mut selected = header("missing-ptc-runtime");
    selected.agent_mode = Some(AgentModeId::ptc());
    let error = match registry.create(selected, options(), cancellation()).await {
        Ok(_) => panic!("programmatic mode unexpectedly started without a code runtime"),
        Err(error) => error,
    };
    match error {
        AgentError::Message(error) => assert_eq!(error.code, "PTC_RUNTIME_UNAVAILABLE"),
        other => panic!("unexpected programmatic setup error: {other:?}"),
    }
}

#[tokio::test]
async fn native_child_mode_excludes_owner_bound_tools() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([text_turn(
                    "child",
                )]))),
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
        .register_factory(Arc::new(factory(llm, SystemPrompt::new(), tools)))
        .unwrap();
    let mut child = header("native-child");
    child.origin = Some(SessionOrigin::Subagent);
    let child = registry
        .create(child, options(), cancellation())
        .await
        .unwrap();
    child.followup(user("child")).await.unwrap();
    child.when_idle().await.unwrap();
    assert_eq!(tool_names(&requests.lock()[0]), ["read"]);
    child.dispose().await.unwrap();
}

struct AlwaysApprove;

#[async_trait]
impl ToolApproval for AlwaysApprove {
    async fn approve(
        &self,
        _context: &ToolRunContext,
        _schema: &ToolSchema,
        _arguments: &Value,
    ) -> Result<Option<bool>, tessivum::TessivumError> {
        Ok(Some(true))
    }
}

#[tokio::test]
async fn programmatic_nested_tools_preserve_denial_and_approval() {
    for approved in [false, true] {
        let llm = LlmRuntime::new();
        let _provider = llm
            .register(
                "test",
                Arc::new(DeterministicAdapter {
                    streams: Arc::new(Mutex::new(VecDeque::from([
                        run_code_turn("return await tools.read({value: 'nested'});"),
                        text_turn("done"),
                    ]))),
                }),
            )
            .unwrap();
        let tools = ToolRuntime::new();
        if approved {
            tools.set_approval(Some(Arc::new(AlwaysApprove)));
        }
        let _tool = tools
            .register(ToolDefinition::new(
                "read",
                "read",
                json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}},"additionalProperties":false}),
                Echo,
            ))
            .unwrap();
        let registry =
            AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
        let _factory = registry
            .register_factory(Arc::new(
                factory(llm, SystemPrompt::new(), tools)
                    .with_code_runtime(ptc_runtime())
                    .with_approval_required_tools(["read".into()]),
            ))
            .unwrap();
        let mut selected = header(if approved {
            "nested-approval"
        } else {
            "nested-denial"
        });
        selected.agent_mode = Some(AgentModeId::ptc());
        let agent = registry
            .create(selected, options(), cancellation())
            .await
            .unwrap();
        agent.followup(user("nested")).await.unwrap();
        agent.when_idle().await.unwrap();
        let result = agent
            .session()
            .events()
            .into_iter()
            .find(|event| event.event_type == "tool/result")
            .unwrap();
        assert_eq!(
            result.data["meta"]["codeDispatches"][1]["data"]["isError"],
            json!(!approved)
        );
        agent.dispose().await.unwrap();
    }
}

struct NotifyingBlockingTool(Arc<tokio::sync::Notify>);

#[async_trait]
impl ToolHandler for NotifyingBlockingTool {
    async fn run(&self, context: ToolRunContext, _arguments: Value) -> ToolHandlerResult {
        self.0.notify_one();
        context.cancellation.cancelled().await;
        Ok(ToolOutput::new(Vec::new(), false, Value::Null))
    }
}

#[tokio::test]
async fn programmatic_nested_tool_cancellation_reaches_the_native_dispatcher() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([run_code_turn(
                    "return await tools.bash({value: 'wait'});",
                )]))),
            }),
        )
        .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let tools = ToolRuntime::new();
    let _tool = tools
        .register(ToolDefinition::new(
            "bash",
            "bash",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}},"additionalProperties":false}),
            NotifyingBlockingTool(Arc::clone(&started)),
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            factory(llm, SystemPrompt::new(), tools).with_code_runtime(ptc_runtime()),
        ))
        .unwrap();
    let mut selected = header("nested-cancel");
    selected.agent_mode = Some(AgentModeId::ptc());
    let agent = registry
        .create(selected, options(), cancellation())
        .await
        .unwrap();
    let wait_for_start = started.notified();
    agent.followup(user("cancel")).await.unwrap();
    wait_for_start.await;
    assert!(agent.cancel(AgentCancelCause::User, false));
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool/result")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "aborted");
    agent.dispose().await.unwrap();
}

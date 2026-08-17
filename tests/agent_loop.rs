use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use futures_util::stream;
use serde_json::{json, Value};
use tessivum::{
    agent::{AgentOptions, AgentRegistry},
    agent_loop::AgentLoopFactory,
    llm::{LlmAdapter, LlmRuntime, LlmStream},
    session::{MemorySessionPersistence, SessionStore},
    system_prompt::{PromptRegistration, PromptSection, SystemPrompt},
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRunContext, ToolRuntime,
    },
    ContentBlock, FinishReason, GenerateRequest, Message, MessageRole, MessageSource, SessionEvent,
    SessionHeader, SessionId, StreamChunk, SurfaceOp, ToolCallId,
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
        source: MessageSource::User,
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
            "request/header",
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

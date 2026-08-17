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
    system_prompt::SystemPrompt,
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRunContext, ToolRuntime,
    },
    ContentBlock, FinishReason, GenerateRequest, Message, MessageRole, MessageSource,
    SessionHeader, SessionId, StreamChunk, ToolCallId,
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
            "request/header",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/message",
            "step/end",
            "turn/end",
        ],
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

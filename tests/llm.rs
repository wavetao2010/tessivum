use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use futures_util::{stream, TryStreamExt};
use serde_json::json;
use tessivum::{
    llm::{llm_service_key, BlockAssembler, LlmAdapter, LlmRuntime, LlmStream, RecordedLlmAdapter},
    ContentBlock, FinishReason, GenerateRequest, MessageSource, SessionId, StreamChunk,
    TessivumError, TokenUsage, ToolCallId,
};
use tessivum_core::{CancellationToken, ContextHandle};

#[derive(Clone)]
struct StaticAdapter {
    calls: Arc<AtomicUsize>,
    chunks: Vec<StreamChunk>,
}

#[async_trait]
impl LlmAdapter for StaticAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::iter(
            self.chunks.clone().into_iter().map(Ok),
        )))
    }
}

struct PendingAdapter;

#[async_trait]
impl LlmAdapter for PendingAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        Ok(Box::pin(stream::pending()))
    }
}

fn cancellation() -> CancellationToken {
    ContextHandle::root().scope().cancellation()
}

fn request(session_id: Option<&str>) -> GenerateRequest {
    GenerateRequest {
        provider: "recorded".into(),
        model: "model-a".into(),
        reasoning_effort: None,
        messages: Vec::new(),
        system: None,
        tools: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        session_id: session_id.map(SessionId::from),
        purpose: None,
    }
}

fn text_stream(text: &str) -> Vec<StreamChunk> {
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

#[test]
fn assembler_mixes_sorted_blocks_and_keeps_raw_chunks() {
    let mut assembler = BlockAssembler::with_message_id("recorded", "model-a", "message-1".into());
    let chunks = vec![
        StreamChunk::BlockStart {
            index: 2,
            block_type: "tool-call".into(),
        },
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".into(),
        },
        StreamChunk::BlockStart {
            index: 1,
            block_type: "reasoning".into(),
        },
        StreamChunk::TextDelta {
            index: 0,
            text: "hello ".into(),
        },
        StreamChunk::TextDelta {
            index: 0,
            text: "world".into(),
        },
        StreamChunk::ReasoningDelta {
            index: 1,
            text: "because".into(),
        },
        StreamChunk::ToolCallDelta {
            index: 2,
            id: ToolCallId::from("call-1"),
            name: Some("search".into()),
            arguments_delta: "{\"q\":".into(),
        },
        StreamChunk::ToolCallDelta {
            index: 2,
            id: ToolCallId::from("call-1"),
            name: None,
            arguments_delta: "\"rust\"}".into(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text {
                text: "provider summary is ignored".into(),
            },
        },
        StreamChunk::BlockEnd {
            index: 1,
            block: ContentBlock::Reasoning {
                text: "provider summary is ignored".into(),
            },
        },
        StreamChunk::BlockEnd {
            index: 2,
            block: ContentBlock::ToolCall {
                id: ToolCallId::from("call-1"),
                name: "search".into(),
                arguments: "ignored".into(),
            },
        },
        StreamChunk::Usage {
            usage: TokenUsage {
                input_tokens: 3,
                output_tokens: 2,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: Some(1),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
            replay_state: Some(json!({"cursor": "r1"})),
        },
    ];

    let mut complete = None;
    for chunk in chunks.clone() {
        complete = assembler.push(chunk).unwrap().or(complete);
    }
    let complete = complete.expect("finish completes the generation");

    assert_eq!(complete.chunks, chunks);
    assert_eq!(complete.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        complete.message.content,
        vec![
            ContentBlock::Text {
                text: "hello world".into(),
            },
            ContentBlock::Reasoning {
                text: "because".into(),
            },
            ContentBlock::ToolCall {
                id: ToolCallId::from("call-1"),
                name: "search".into(),
                arguments: "{\"q\":\"rust\"}".into(),
            },
        ]
    );
    assert!(matches!(
        complete.message.source,
        MessageSource::Model {
            ref provider,
            ref model,
            replay_state: Some(ref state),
        } if provider == "recorded" && model == "model-a" && state == &json!({"cursor": "r1"})
    ));
}

#[test]
fn assembler_rejects_terminal_order_failures_and_ignores_late_deltas() {
    let mut duplicate_usage = BlockAssembler::new("p", "m");
    duplicate_usage
        .push(StreamChunk::Usage {
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            },
        })
        .unwrap();
    let error = duplicate_usage
        .push(StreamChunk::Usage {
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            },
        })
        .unwrap_err();
    assert_eq!(error.code, "DUPLICATE_LLM_USAGE");

    let mut closed = BlockAssembler::new("p", "m");
    for chunk in text_stream("first")[..3].iter().cloned() {
        closed.push(chunk).unwrap();
    }
    closed
        .push(StreamChunk::TextDelta {
            index: 0,
            text: " ignored".into(),
        })
        .unwrap();
    let complete = closed
        .push(StreamChunk::Finish {
            reason: FinishReason::Stop,
            replay_state: None,
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        complete.message.content,
        vec![ContentBlock::Text {
            text: "first".into()
        }]
    );
    let after_finish = closed
        .push(StreamChunk::Usage {
            usage: TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            },
        })
        .unwrap_err();
    assert_eq!(after_finish.code, "LLM_STREAM_AFTER_FINISH");

    let mut ended_twice = BlockAssembler::new("p", "m");
    for chunk in text_stream("once")[..3].iter().cloned() {
        ended_twice.push(chunk).unwrap();
    }
    let duplicate_end = ended_twice
        .push(StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text {
                text: "once".into(),
            },
        })
        .unwrap_err();
    assert_eq!(duplicate_end.code, "INVALID_LLM_STREAM");

    let mut open = BlockAssembler::new("p", "m");
    open.push(StreamChunk::BlockStart {
        index: 0,
        block_type: "text".into(),
    })
    .unwrap();
    let error = open
        .push(StreamChunk::Finish {
            reason: FinishReason::Stop,
            replay_state: None,
        })
        .unwrap_err();
    assert_eq!(error.code, "LLM_FINISH_WITH_OPEN_BLOCK");
}

#[tokio::test]
async fn runtime_routes_once_and_registration_handles_remove_providers() {
    let runtime = LlmRuntime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let registration = runtime
        .register(
            "recorded",
            Arc::new(StaticAdapter {
                calls: Arc::clone(&calls),
                chunks: text_stream("one attempt"),
            }),
        )
        .unwrap();
    let duplicate = runtime
        .register(
            "recorded",
            Arc::new(StaticAdapter {
                calls: Arc::new(AtomicUsize::new(0)),
                chunks: Vec::new(),
            }),
        )
        .unwrap_err();
    assert_eq!(duplicate.code, "DUPLICATE_LLM_PROVIDER");

    let complete = runtime
        .complete(request(None), cancellation())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(complete.chunks, text_stream("one attempt"));

    drop(registration);
    let error = match runtime.generate(request(None), cancellation()).await {
        Ok(_) => panic!("disposed provider must not return a stream"),
        Err(error) => error,
    };
    assert_eq!(error.code, "LLM_PROVIDER_NOT_FOUND");
}

#[tokio::test]
async fn cancellation_interrupts_the_returned_stream() {
    let runtime = LlmRuntime::new();
    let _registration = runtime
        .register("recorded", Arc::new(PendingAdapter))
        .unwrap();
    let token = cancellation();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let token = token.clone();
        async move { runtime.complete(request(None), token).await }
    });
    tokio::task::yield_now().await;
    assert!(token.cancel());
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.code, "LLM_CANCELLED");
}

#[tokio::test]
async fn recorded_replay_routes_deterministically_without_consuming_entries() {
    let adapter = RecordedLlmAdapter::from_jsonl(
        r#"{"sessionId":"s-1","provider":"recorded","model":"model-a","chunk":{"type":"block-start","index":0,"blockType":"text"}}
{"sessionId":"s-1","provider":"recorded","model":"model-a","chunk":{"type":"text-delta","index":0,"text":"replayed"}}
{"sessionId":"s-1","provider":"recorded","model":"model-a","chunk":{"type":"block-end","index":0,"block":{"type":"text","text":"replayed"}}}
{"sessionId":"s-1","provider":"recorded","model":"model-a","chunk":{"type":"finish","reason":{"kind":"stop"}}}"#,
    )
    .unwrap();
    let matching_request = request(Some("s-1"));
    let first = adapter
        .generate(matching_request.clone(), cancellation())
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let second = adapter
        .generate(matching_request, cancellation())
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(first, second);

    let error = match adapter
        .generate(request(Some("wrong-session")), cancellation())
        .await
    {
        Ok(_) => panic!("unrecorded route must not return a stream"),
        Err(error) => error,
    };
    assert_eq!(error.code, "RECORDED_LLM_ROUTE_NOT_FOUND");
}

#[tokio::test]
async fn max_tokens_omits_an_open_tool_call() {
    let runtime = LlmRuntime::new();
    let _registration = runtime
        .register(
            "recorded",
            Arc::new(StaticAdapter {
                calls: Arc::new(AtomicUsize::new(0)),
                chunks: vec![
                    StreamChunk::BlockStart {
                        index: 0,
                        block_type: "tool-call".into(),
                    },
                    StreamChunk::ToolCallDelta {
                        index: 0,
                        id: ToolCallId::from("unsafe"),
                        name: Some("shell".into()),
                        arguments_delta: "{\"command\":\"rm".into(),
                    },
                    StreamChunk::Finish {
                        reason: FinishReason::MaxTokens,
                        replay_state: None,
                    },
                ],
            }),
        )
        .unwrap();
    let complete = runtime
        .complete(request(None), cancellation())
        .await
        .unwrap();
    assert!(complete.message.content.is_empty());
    assert_eq!(complete.finish_reason, FinishReason::MaxTokens);
}

#[test]
fn runtime_publishes_the_versioned_service_to_context() {
    let context = ContextHandle::root();
    let runtime = LlmRuntime::new();
    let provider = runtime.clone().publish(&context).unwrap();
    assert_eq!(llm_service_key().diagnostic_key(), "harness.llm@1");
    let consumer = context
        .get::<LlmRuntime>(&llm_service_key())
        .unwrap()
        .expect("runtime service is visible");
    assert!(consumer.with(|_| ()).is_ok());
    drop(provider);
}

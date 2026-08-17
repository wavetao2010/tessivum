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

async fn replay(adapter: &RecordedLlmAdapter, request: GenerateRequest) -> Vec<StreamChunk> {
    adapter
        .generate(request, cancellation())
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap()
}

async fn replay_text(adapter: &RecordedLlmAdapter, request: GenerateRequest) -> String {
    replay(adapter, request)
        .await
        .into_iter()
        .find_map(|chunk| match chunk {
            StreamChunk::TextDelta { text, .. } => Some(text),
            _ => None,
        })
        .expect("replay attempt contains a text delta")
}

fn replay_line(session_id: &str, request_id: &str, text: &str) -> String {
    json!({
        "sessionId": session_id,
        "provider": "recorded",
        "model": "model-a",
        "requestId": request_id,
        "chunks": text_stream(text),
    })
    .to_string()
}

#[tokio::test]
async fn recorded_replay_consumes_request_id_attempts_from_protocol_fixture() {
    let adapter = RecordedLlmAdapter::from_jsonl_with_route(
        include_str!("../fixtures/headless/recorded-replay.jsonl"),
        Some(SessionId::from("s-1")),
        "recorded",
        "model-a",
    )
    .unwrap();
    let request = request(Some("s-1"));

    let tool_attempt = replay(&adapter, request.clone()).await;
    assert_eq!(tool_attempt.len(), 5);
    assert!(matches!(
        tool_attempt.last(),
        Some(StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
            ..
        })
    ));

    let text_attempt = replay(&adapter, request).await;
    assert_eq!(text_attempt.len(), 5);
    assert!(matches!(
        text_attempt.last(),
        Some(StreamChunk::Finish {
            reason: FinishReason::Stop,
            ..
        })
    ));
    assert!(text_attempt.iter().any(|chunk| matches!(
        chunk,
        StreamChunk::TextDelta { text, .. } if text == "CLI tool round trip complete: CLI_TOOL_ROUND_TRIP"
    )));
}

#[tokio::test]
async fn recorded_replay_preserves_legacy_lines_and_exhausts_routes() {
    let adapter = RecordedLlmAdapter::from_jsonl(
        r#"{"sessionId":"s-1","provider":"recorded","model":"model-a","chunk":{"type":"block-start","index":0,"blockType":"text"}}
{"sessionId":"s-1","provider":"recorded","model":"model-a","chunk":{"type":"text-delta","index":0,"text":"replayed"}}
{"sessionId":"s-1","provider":"recorded","model":"model-a","chunk":{"type":"block-end","index":0,"block":{"type":"text","text":"replayed"}}}
{"sessionId":"s-1","provider":"recorded","model":"model-a","chunk":{"type":"finish","reason":{"kind":"stop"}}}"#,
    )
    .unwrap();
    let matching_request = request(Some("s-1"));
    let cancelled = cancellation();
    assert!(cancelled.cancel());
    let cancellation_error = match adapter.generate(matching_request.clone(), cancelled).await {
        Ok(_) => panic!("cancelled replay must not return a stream"),
        Err(error) => error,
    };
    assert_eq!(cancellation_error.code, "LLM_CANCELLED");
    assert_eq!(
        replay(&adapter, matching_request.clone()).await,
        text_stream("replayed")
    );

    let exhausted = match adapter.generate(matching_request, cancellation()).await {
        Ok(_) => panic!("exhausted replay must not return a stream"),
        Err(error) => error,
    };
    assert_eq!(exhausted.code, "RECORDED_LLM_EXHAUSTED");
    let missing_route = match adapter
        .generate(request(Some("wrong-session")), cancellation())
        .await
    {
        Ok(_) => panic!("unrecorded route must not return a stream"),
        Err(error) => error,
    };
    assert_eq!(missing_route.code, "RECORDED_LLM_ROUTE_NOT_FOUND");
}

#[tokio::test]
async fn recorded_replay_keeps_session_cursors_independent() {
    let adapter = RecordedLlmAdapter::from_jsonl(
        &[
            replay_line("s-1", "s-1-first", "session one first"),
            replay_line("s-2", "s-2-first", "session two first"),
            replay_line("s-1", "s-1-second", "session one second"),
        ]
        .join("\n"),
    )
    .unwrap();

    assert_eq!(
        replay_text(&adapter, request(Some("s-1"))).await,
        "session one first"
    );
    assert_eq!(
        replay_text(&adapter, request(Some("s-2"))).await,
        "session two first"
    );
    assert_eq!(
        replay_text(&adapter, request(Some("s-1"))).await,
        "session one second"
    );
}

#[tokio::test]
async fn recorded_replay_reset_and_fresh_adapters_start_at_the_first_attempt() {
    let recording = [
        replay_line("s-1", "first", "first attempt"),
        replay_line("s-1", "second", "second attempt"),
    ]
    .join("\n");
    let adapter = RecordedLlmAdapter::from_jsonl(&recording).unwrap();

    assert_eq!(
        replay_text(&adapter, request(Some("s-1"))).await,
        "first attempt"
    );
    assert_eq!(
        replay_text(&adapter, request(Some("s-1"))).await,
        "second attempt"
    );
    adapter.reset();
    assert_eq!(
        replay_text(&adapter, request(Some("s-1"))).await,
        "first attempt"
    );

    let fresh = RecordedLlmAdapter::from_jsonl(&recording).unwrap();
    assert_eq!(
        replay_text(&fresh, request(Some("s-1"))).await,
        "first attempt"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_replay_calls_receive_distinct_complete_attempts() {
    let adapter = Arc::new(
        RecordedLlmAdapter::from_jsonl(
            &[
                replay_line("s-1", "first", "first attempt"),
                replay_line("s-1", "second", "second attempt"),
            ]
            .join("\n"),
        )
        .unwrap(),
    );
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let first = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        let barrier = Arc::clone(&barrier);
        async move {
            barrier.wait().await;
            replay(&adapter, request(Some("s-1"))).await
        }
    });
    let second = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move {
            barrier.wait().await;
            replay(&adapter, request(Some("s-1"))).await
        }
    });
    let mut attempts = vec![first.await.unwrap(), second.await.unwrap()];
    for attempt in &attempts {
        assert!(matches!(attempt.last(), Some(StreamChunk::Finish { .. })));
    }
    attempts.sort_by_key(|attempt| match &attempt[1] {
        StreamChunk::TextDelta { text, .. } => text.clone(),
        _ => panic!("text attempt has a text delta"),
    });
    assert_eq!(attempts[0], text_stream("first attempt"));
    assert_eq!(attempts[1], text_stream("second attempt"));
}

#[test]
fn recorded_replay_requires_routes_without_defaults_and_rejects_chunks_after_finish() {
    let missing_route = match RecordedLlmAdapter::from_jsonl(
        r#"{"requestId":"one","chunk":{"type":"finish","reason":{"kind":"stop"}}}"#,
    ) {
        Ok(_) => panic!("replay without a route must fail"),
        Err(error) => error,
    };
    assert_eq!(missing_route.code, "INVALID_LLM_REPLAY");

    let after_finish = match RecordedLlmAdapter::from_jsonl(
        r#"{"provider":"recorded","model":"model-a","chunk":{"type":"finish","reason":{"kind":"stop"}}}
{"provider":"recorded","model":"model-a","chunk":{"type":"text-delta","index":0,"text":"late"}}"#,
    ) {
        Ok(_) => panic!("replay cannot contain chunks after finish"),
        Err(error) => error,
    };
    assert_eq!(after_finish.code, "INVALID_LLM_REPLAY");
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
async fn recorded_replay_is_deterministic_after_reset() {
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
    adapter.reset();
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

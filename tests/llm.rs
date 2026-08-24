use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures_util::{stream, TryStreamExt};
use parking_lot::Mutex;
use serde_json::json;
use tessivum::{
    llm::{
        llm_service_key, parse_replay_session_log, BlockAssembler, LlmAdapter, LlmRuntime,
        LlmStream, RecordedLlmAdapter,
    },
    ContentBlock, FinishReason, GenerateRequest, Message, MessageId, MessageRole, MessageSource,
    SessionId, StreamChunk, TessivumError, TokenUsage, ToolCallId,
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

struct FailingAdapter {
    error: TessivumError,
    during_stream: bool,
}

#[async_trait]
impl LlmAdapter for FailingAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        if self.during_stream {
            return Ok(Box::pin(stream::iter(vec![Err(self.error.clone())])));
        }
        Err(self.error.clone())
    }
}

struct DiscoveryAdapter {
    calls: Arc<AtomicUsize>,
    configs: Arc<Mutex<Vec<serde_json::Value>>>,
    results: Mutex<Vec<Result<serde_json::Value, TessivumError>>>,
}

impl DiscoveryAdapter {
    fn new(results: Vec<Result<serde_json::Value, TessivumError>>) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            configs: Arc::new(Mutex::new(Vec::new())),
            results: Mutex::new(results),
        }
    }
}

#[async_trait]
impl LlmAdapter for DiscoveryAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        Err(TessivumError::new(
            "UNEXPECTED_GENERATE",
            "model discovery tests do not generate",
            "test",
            serde_json::Value::Null,
        ))
    }

    async fn models(
        &self,
        mut config: serde_json::Value,
    ) -> Result<serde_json::Value, TessivumError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.configs.lock().push(config.clone());
        config["mutated"] = json!(true);
        self.results.lock().remove(0)
    }
}

struct PendingModelsAdapter {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmAdapter for PendingModelsAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        Err(TessivumError::new(
            "UNEXPECTED_GENERATE",
            "model discovery tests do not generate",
            "test",
            serde_json::Value::Null,
        ))
    }

    async fn models(&self, _config: serde_json::Value) -> Result<serde_json::Value, TessivumError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
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

#[test]
fn packed_text_chunk_times_are_cumulative() {
    let recording = [
        json!({"type": "session", "id": "packed"}),
        json!({
            "type": "text-chunks",
            "seq0": 4,
            "time0": 100,
            "data": {"turn": 1, "step": 1, "index": 0, "dt": [27, 2], "texts": ["a", "b", "c"]},
        }),
    ]
    .into_iter()
    .map(|row| row.to_string())
    .collect::<Vec<_>>()
    .join("\n");

    let events = parse_replay_session_log(&recording).unwrap();
    assert_eq!(
        events.iter().map(|event| event.time).collect::<Vec<_>>(),
        [100, 127, 129]
    );
}

fn session_recording(id: &str, calls: &[Vec<StreamChunk>]) -> String {
    let mut lines = vec![json!({
        "type": "session",
        "version": 0,
        "id": id,
        "createdAt": 0,
    })
    .to_string()];
    let mut seq = 0_u64;
    for (step, chunks) in calls.iter().enumerate() {
        for chunk in chunks {
            lines.push(
                json!({
                    "type": "assistant/chunk",
                    "seq": seq,
                    "time": 0,
                    "data": {"turn": 1, "step": step + 1, "chunk": chunk},
                })
                .to_string(),
            );
            seq += 1;
        }
    }
    lines.join("\n")
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
                text: "provider summary is ignored".into(),
            },
            ContentBlock::Reasoning {
                text: "provider summary is ignored".into(),
            },
            ContentBlock::ToolCall {
                id: ToolCallId::from("call-1"),
                name: "search".into(),
                arguments: "ignored".into(),
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

    let mut tool_identity = BlockAssembler::new("p", "m");
    tool_identity
        .push(StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".into(),
        })
        .unwrap();
    tool_identity
        .push(StreamChunk::ToolCallDelta {
            index: 0,
            id: ToolCallId::from("call-a"),
            name: Some("echo".into()),
            arguments_delta: "{}".into(),
        })
        .unwrap();
    let error = tool_identity
        .push(StreamChunk::ToolCallDelta {
            index: 0,
            id: ToolCallId::from("call-b"),
            name: None,
            arguments_delta: String::new(),
        })
        .unwrap_err();
    assert_eq!(error.code, "INVALID_LLM_STREAM");
    let error = tool_identity
        .push(StreamChunk::ToolCallDelta {
            index: 0,
            id: ToolCallId::from("call-a"),
            name: Some("other".into()),
            arguments_delta: String::new(),
        })
        .unwrap_err();
    assert_eq!(error.code, "INVALID_LLM_STREAM");

    let error = tool_identity
        .push(StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: ToolCallId::from("call-b"),
                name: "echo".into(),
                arguments: "{}".into(),
            },
        })
        .unwrap_err();
    assert_eq!(error.code, "INVALID_LLM_STREAM");

    let mut interrupted = BlockAssembler::new("p", "m");
    interrupted
        .push(StreamChunk::BlockStart {
            index: 0,
            block_type: "text".into(),
        })
        .unwrap();
    let complete = interrupted
        .push(StreamChunk::Finish {
            reason: FinishReason::Error {
                failure: tessivum::LlmFailure {
                    message: "provider reset".into(),
                    code: "RESET".into(),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                },
            },
            replay_state: None,
        })
        .unwrap()
        .unwrap();
    assert!(complete.message.content.is_empty());
    assert!(matches!(complete.finish_reason, FinishReason::Error { .. }));
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
    let chunks = runtime
        .generate(request(None), cancellation())
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert!(matches!(
        chunks.as_slice(),
        [StreamChunk::Finish {
            reason: FinishReason::Error { failure },
            replay_state: None,
        }] if failure.code == "LLM_PROVIDER_NOT_FOUND"
    ));
}

#[tokio::test]
async fn cancellation_ends_the_returned_stream_with_an_aborted_finish() {
    let runtime = LlmRuntime::new();
    let _registration = runtime
        .register("recorded", Arc::new(PendingAdapter))
        .unwrap();
    let token = cancellation();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let token = token.clone();
        async move {
            runtime
                .generate(request(None), token)
                .await
                .unwrap()
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
        }
    });
    tokio::task::yield_now().await;
    assert!(token.cancel());
    let chunks = task.await.unwrap();
    assert!(matches!(
        chunks.as_slice(),
        [StreamChunk::Finish {
            reason: FinishReason::Aborted { failure },
            replay_state: None,
        }] if failure.code == "LLM_CANCELLED"
    ));
}

#[tokio::test]
async fn runtime_normalizes_adapter_failures_into_one_terminal_chunk() {
    for during_stream in [false, true] {
        let runtime = LlmRuntime::new();
        let _registration = runtime
            .register(
                "recorded",
                Arc::new(FailingAdapter {
                    error: TessivumError::new(
                        "RATE_LIMIT",
                        "provider busy",
                        "provider",
                        json!({"status": 429, "providerRetryAfterMs": 250, "requestId": "req-7"}),
                    ),
                    during_stream,
                }),
            )
            .unwrap();
        let chunks = runtime
            .generate(request(None), cancellation())
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(
            chunks,
            vec![StreamChunk::Finish {
                reason: FinishReason::Error {
                    failure: tessivum::LlmFailure {
                        message: "provider busy".into(),
                        code: "RATE_LIMIT".into(),
                        status: Some(429),
                        provider_retry_after_ms: Some(250),
                        request_id: Some("req-7".into()),
                    },
                },
                replay_state: None,
            }]
        );
    }
}

#[tokio::test]
async fn runtime_normalizes_an_unterminated_adapter_stream() {
    let runtime = LlmRuntime::new();
    let _registration = runtime
        .register(
            "recorded",
            Arc::new(StaticAdapter {
                calls: Arc::new(AtomicUsize::new(0)),
                chunks: vec![StreamChunk::TextDelta {
                    index: 0,
                    text: "partial".into(),
                }],
            }),
        )
        .unwrap();
    let chunks = runtime
        .generate(request(None), cancellation())
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert!(matches!(
        chunks.as_slice(),
        [StreamChunk::TextDelta { text, .. }, StreamChunk::Finish {
            reason: FinishReason::Error { failure },
            replay_state: None,
        }] if text == "partial" && failure.code == "LLM_STREAM_ENDED_EARLY"
    ));
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
}

#[tokio::test]
async fn provider_models_retries_transient_failure_with_fresh_config_snapshots() {
    let adapter = Arc::new(DiscoveryAdapter::new(vec![
        Err(TessivumError::new(
            "UPSTREAM_TEMPORARY",
            "retry me",
            "provider",
            json!({"transient": true}),
        )),
        Ok(json!([{
            "id": "one",
            "name": "One",
            "contextWindow": 4096,
            "maxOutput": 1024,
            "reasoning": false,
            "input": ["text"]
        }])),
    ]));
    let runtime = LlmRuntime::new();
    let _registration = runtime.register("active", adapter.clone()).unwrap();
    let config = json!({"nested": {"token": "snapshot"}});

    let started = Instant::now();
    let models = runtime
        .models("active".into(), config.clone())
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(250));

    assert_eq!(models[0]["id"], "one");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    assert_eq!(*adapter.configs.lock(), vec![config.clone(), config]);
}
#[tokio::test]
async fn provider_models_preserves_retry_metadata_and_never_retries_unknown_or_invalid_routes() {
    let adapter = Arc::new(DiscoveryAdapter::new(vec![Err(TessivumError::new(
        "UPSTREAM_BAD_REQUEST",
        "not retryable",
        "provider",
        json!({"cause": "bad config"}),
    ))]));
    let runtime = LlmRuntime::new();
    let _registration = runtime.register("active", adapter.clone()).unwrap();

    let non_retryable = runtime
        .models("active".into(), json!({}))
        .await
        .unwrap_err();
    assert_eq!(non_retryable.code, "UPSTREAM_BAD_REQUEST");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        non_retryable.details,
        json!({
            "cause": "bad config",
            "provider": "active",
            "attempts": 1,
            "retries": 0,
            "retryable": false,
        })
    );

    let unknown = runtime
        .models("active-alias".into(), json!({}))
        .await
        .unwrap_err();
    assert_eq!(unknown.code, "LLM_PROVIDER_NOT_FOUND");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        unknown.details,
        json!({
            "provider": "active-alias",
            "attempts": 0,
            "retries": 0,
            "retryable": false,
        })
    );
}

#[tokio::test]
async fn provider_models_exhausts_transient_timeout_failures_after_one_retry() {
    let adapter = Arc::new(DiscoveryAdapter::new(vec![
        Err(TessivumError::new(
            "UPSTREAM_TEMPORARY",
            "first transient failure",
            "provider",
            json!({}),
        )),
        Err(TessivumError::new(
            "UPSTREAM_TIMEOUT",
            "second timeout failure",
            "provider",
            json!({}),
        )),
    ]));
    let runtime = LlmRuntime::new();
    let _registration = runtime.register("active", adapter.clone()).unwrap();

    let error = runtime
        .models("active".into(), json!({}))
        .await
        .unwrap_err();

    assert_eq!(error.code, "UPSTREAM_TIMEOUT");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        error.details,
        json!({
            "provider": "active",
            "attempts": 2,
            "retries": 1,
            "retryable": true,
        })
    );
}

#[tokio::test]
async fn provider_models_times_out_each_attempt_before_exhausting_the_single_retry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = LlmRuntime::new();
    let _registration = runtime
        .register(
            "active",
            Arc::new(PendingModelsAdapter {
                calls: Arc::clone(&calls),
            }),
        )
        .unwrap();

    let started = Instant::now();
    let error = runtime
        .models("active".into(), json!({}))
        .await
        .unwrap_err();
    assert!(started.elapsed() >= Duration::from_secs(10));

    assert_eq!(error.code, "MODEL_DISCOVERY_TIMEOUT");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        error.details,
        json!({
            "provider": "active",
            "attempts": 2,
            "retries": 1,
            "retryable": true,
        })
    );
}

#[tokio::test]
async fn provider_models_rejects_non_object_config_before_calling_the_adapter() {
    let adapter = Arc::new(DiscoveryAdapter::new(Vec::new()));
    let runtime = LlmRuntime::new();
    let _registration = runtime.register("active", adapter.clone()).unwrap();

    let error = runtime
        .models("active".into(), json!(["not-an-object"]))
        .await
        .unwrap_err();

    assert_eq!(error.code, "INVALID_MODEL_DISCOVERY_CONFIG");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        error.details,
        json!({
            "provider": "active",
            "attempts": 0,
            "retries": 0,
            "retryable": false,
        })
    );
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

#[tokio::test]
async fn durable_session_replay_derives_ordered_tool_and_text_attempts() {
    let tool = vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".into(),
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: ToolCallId::from("call-replay"),
            name: Some("echo".into()),
            arguments_delta: r#"{"value":"replayed"}"#.into(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: ToolCallId::from("call-replay"),
                name: "echo".into(),
                arguments: r#"{"value":"replayed"}"#.into(),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
            replay_state: None,
        },
    ];
    let adapter = RecordedLlmAdapter::from_session_jsonl(&session_recording(
        "recorded-parent",
        &[tool.clone(), text_stream("after tool")],
    ))
    .unwrap();

    let first = replay(&adapter, request(Some("live-parent"))).await;
    let second = replay(&adapter, request(Some("live-parent"))).await;
    assert_eq!(first, tool);
    assert_eq!(second, text_stream("after tool"));
    adapter.assert_consumed().unwrap();
}

#[test]
fn durable_replay_rejects_malformed_lines_chunks_and_unfinished_calls() {
    let malformed =
        RecordedLlmAdapter::from_session_jsonl("{\"type\":\"session\",\"id\":\"bad\"}\nnot-json")
            .unwrap_err();
    assert_eq!(malformed.code, "INVALID_LLM_REPLAY");

    let malformed_chunk = RecordedLlmAdapter::from_jsonl(
        r#"{"provider":"recorded","model":"model-a","chunks":[{"type":"bogus"}]}"#,
    )
    .unwrap_err();
    assert_eq!(malformed_chunk.code, "INVALID_LLM_REPLAY");

    let unfinished = RecordedLlmAdapter::from_session_jsonl(
        "{\"type\":\"session\",\"version\":0,\"id\":\"bad\",\"createdAt\":0}\n\
         {\"type\":\"assistant/chunk\",\"seq\":0,\"time\":0,\"data\":{\"turn\":1,\"step\":1,\"chunk\":{\"type\":\"text-delta\",\"index\":0,\"text\":\"partial\"}}}",
    )
    .unwrap_err();
    assert_eq!(unfinished.code, "INVALID_LLM_REPLAY");
}

#[tokio::test]
async fn durable_replay_overrides_match_request_text_and_trailing_attempts_fail_loud() {
    let recording = session_recording("recorded", &[text_stream("derived")]);
    let override_json = json!([{
        "kind": "chunks",
        "chunks": [
            {"type": "text-delta", "index": 0, "text": "{{fromRequest:(goal-[0-9]+)}}"},
            {"type": "finish", "reason": {"kind": "stop"}}
        ]
    }])
    .to_string();
    let adapter =
        RecordedLlmAdapter::from_session_jsonl_with_override(&recording, &override_json).unwrap();
    let mut matched = request(Some("live"));
    matched.messages = vec![Message {
        id: MessageId::from("request"),
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "stale goal-7 then goal-42".into(),
        }],
        source: MessageSource::User {
            client_time_zone: None,
        },
    }];
    assert_eq!(replay_text(&adapter, matched).await, "goal-42");
    adapter.assert_consumed().unwrap();

    let trailing = RecordedLlmAdapter::from_session_jsonl(&session_recording(
        "recorded",
        &[text_stream("one"), text_stream("two")],
    ))
    .unwrap();
    assert_eq!(replay_text(&trailing, request(Some("live"))).await, "one");
    assert_eq!(
        trailing.assert_consumed().unwrap_err().code,
        "INVALID_LLM_REPLAY"
    );
    assert_eq!(replay_text(&trailing, request(Some("live"))).await, "two");
    trailing.assert_consumed().unwrap();
}

#[tokio::test]
async fn durable_replay_errors_hangs_and_paced_waits_are_cancellation_aware() {
    let recording = session_recording("recorded", &[text_stream("derived")]);
    let throw = json!([{
        "kind": "throw",
        "chunks": [{"type": "text-delta", "index": 0, "text": "prefix"}],
        "message": "retryable upstream failure",
        "code": "UPSTREAM_TEMPORARY"
    }])
    .to_string();
    let adapter = RecordedLlmAdapter::from_session_jsonl_with_override(&recording, &throw).unwrap();
    let error = adapter
        .generate(request(Some("live")), cancellation())
        .await
        .unwrap()
        .try_collect::<Vec<_>>()
        .await
        .unwrap_err();
    assert_eq!(error.code, "UPSTREAM_TEMPORARY");

    let hanging =
        RecordedLlmAdapter::from_session_jsonl_with_override(&recording, r#"[{"kind":"hang"}]"#)
            .unwrap();
    let token = cancellation();
    let stream = hanging
        .generate(request(Some("live")), token.clone())
        .await
        .unwrap();
    let waiting = tokio::spawn(async move { stream.try_collect::<Vec<_>>().await });
    tokio::task::yield_now().await;
    assert!(token.cancel());
    assert_eq!(waiting.await.unwrap().unwrap_err().code, "LLM_CANCELLED");

    let paced = RecordedLlmAdapter::from_session_jsonl(&recording)
        .unwrap()
        .with_pace_ms(60_000);
    let token = cancellation();
    let stream = paced
        .generate(request(Some("paced")), token.clone())
        .await
        .unwrap();
    let waiting = tokio::spawn(async move { stream.try_collect::<Vec<_>>().await });
    tokio::task::yield_now().await;
    assert!(token.cancel());
    assert_eq!(waiting.await.unwrap().unwrap_err().code, "LLM_CANCELLED");
}

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use tessivum::{
    attachments::AttachmentStore,
    compatible_api::CompatibleApiAdapter,
    llm::LlmRuntime,
    openai_responses::{ProviderSnapshot, ResponsesModel, ResponsesRoute},
    ContentBlock, FinishReason, GenerateRequest, Message, MessageId, MessageRole, MessageSource,
};
use tessivum_core::ContextHandle;

#[derive(Default)]
struct RelayState {
    chat: Mutex<Vec<Value>>,
    anthropic: Mutex<Vec<Value>>,
}

async fn chat(
    State(state): State<Arc<RelayState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    assert_eq!(headers[header::AUTHORIZATION], "Bearer test-key");
    state.chat.lock().unwrap().push(body);
    let events = [
        json!({"choices":[{"delta":{"content":"chat-ok"},"finish_reason":null}]}).to_string(),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}]}).to_string(),
        json!({"choices":[],"usage":{"prompt_tokens":4,"completion_tokens":2}}).to_string(),
        "[DONE]".into(),
    ];
    let stream = events.map(|event| format!("data: {event}\n\n")).concat();
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(stream))
        .unwrap()
}

async fn anthropic(
    State(state): State<Arc<RelayState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response<Body> {
    assert_eq!(headers["x-api-key"], "test-key");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    state.anthropic.lock().unwrap().push(body);
    let events = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"anthropic-ok"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ]
    .map(|event| format!("data: {event}\n\n"))
    .concat();
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(events))
        .unwrap()
}

fn request(provider: &str, model: &str) -> GenerateRequest {
    GenerateRequest {
        provider: provider.into(),
        model: model.into(),
        reasoning_effort: None,
        messages: vec![Message {
            id: MessageId::random(),
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            source: MessageSource::User {
                client_time_zone: None,
            },
        }],
        system: Some("system".into()),
        tools: None,
        temperature: None,
        max_tokens: Some(64),
        stop: None,
        session_id: None,
        purpose: None,
    }
}

#[tokio::test]
async fn configured_protocol_controls_endpoint_body_headers_and_stream_conversion() {
    let state = Arc::new(RelayState::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/v1/chat/completions", post(chat))
                    .route("/v1/messages", post(anthropic))
                    .with_state(state),
            )
            .await
            .unwrap();
        }
    });
    let base = format!("http://{address}/v1");
    let resolver = move |provider: &str, model: &str| {
        let api = match provider {
            "chat" => "openai-completions",
            "anthropic" => "anthropic-messages",
            _ => panic!("unexpected provider"),
        };
        let model = ResponsesModel::new(model);
        ProviderSnapshot::new(
            ResponsesRoute::new(provider, provider, &base, "TEST_KEY", vec![model.clone()])
                .with_api(api),
            model,
            "test-key",
        )
    };
    let data = TempDir::new();
    let store = Arc::new(AttachmentStore::new(&data.0, Default::default()).unwrap());
    let adapter = Arc::new(CompatibleApiAdapter::with_resolver_and_store(
        resolver, store,
    ));
    let runtime = LlmRuntime::new();
    let _chat = runtime.register("chat", adapter.clone()).unwrap();
    let _anthropic = runtime.register("anthropic", adapter).unwrap();
    let cancellation = ContextHandle::root().scope().cancellation();

    let chat_result = runtime
        .complete(request("chat", "chat-model"), cancellation.clone())
        .await
        .unwrap();
    let anthropic_result = runtime
        .complete(request("anthropic", "claude"), cancellation)
        .await
        .unwrap();

    assert_eq!(
        chat_result.message.content,
        vec![ContentBlock::Text {
            text: "chat-ok".into()
        }]
    );
    assert_eq!(chat_result.finish_reason, FinishReason::Stop);
    assert_eq!(chat_result.usage.unwrap().input_tokens, 4);
    assert_eq!(
        anthropic_result.message.content,
        vec![ContentBlock::Text {
            text: "anthropic-ok".into()
        }]
    );
    assert_eq!(anthropic_result.finish_reason, FinishReason::Stop);
    assert_eq!(anthropic_result.usage.unwrap().output_tokens, 3);
    assert_eq!(state.chat.lock().unwrap()[0]["model"], "chat-model");
    assert_eq!(
        state.chat.lock().unwrap()[0]["messages"][0]["role"],
        "system"
    );
    assert_eq!(state.anthropic.lock().unwrap()[0]["model"], "claude");
    assert_eq!(state.anthropic.lock().unwrap()[0]["system"], "system");
    server.abort();
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("tessivum-compatible-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

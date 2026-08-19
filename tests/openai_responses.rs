use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Response, StatusCode},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use tessivum::{
    attachments::{AttachmentInput, AttachmentStore},
    llm::LlmRuntime,
    openai_responses::{
        OpenAiResponsesAdapter, ProviderSnapshot, ResponsesModel, ResponsesRoute,
        ResponsesRouteResolver,
    },
    ContentBlock, FinishReason, GenerateRequest, Message, MessageId, MessageRole, MessageSource,
    SessionId, ToolCallId, ToolSchema,
};
use tessivum_core::ContextHandle;

#[derive(Default)]
struct RelayState {
    requests: Mutex<Vec<Value>>,
    authorizations: Mutex<Vec<String>>,
}

async fn responses(
    State(state): State<Arc<RelayState>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response<Body> {
    state.requests.lock().unwrap().push(request);
    state.authorizations.lock().unwrap().push(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
    );
    let call = state.requests.lock().unwrap().len();
    let events = if call == 1 {
        vec![
            json!({"type":"response.created","response":{"id":"resp_1"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[]}}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"plan"}),
            json!({"type":"response.reasoning_summary_part.done","output_index":0}),
            json!({"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"next"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"plan"},{"type":"summary_text","text":"next"}],"encrypted_content":"encrypted-reasoning"}}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"bash","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"command\":\"pwd\"}"}),
            json!({"type":"response.function_call_arguments.done","output_index":1,"arguments":"{\"command\":\"pwd\"}"}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"bash","arguments":"{\"command\":\"pwd\"}"}}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[
                {"id":"rs_1","type":"reasoning","summary":[{"type":"summary_text","text":"plan"},{"type":"summary_text","text":"next"}]},
                {"id":"fc_1","type":"function_call","call_id":"call_1","name":"bash","arguments":"{\"command\":\"pwd\"}"}
            ],"usage":{"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":3}}}}),
        ]
    } else {
        vec![
            json!({"type":"response.created","response":{"id":"resp_2"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_2","type":"message","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"done"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_2","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]}}),
            json!({"type":"response.completed","response":{"id":"resp_2","status":"completed","output":[{"id":"msg_2","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]}],"usage":{"input_tokens":20,"output_tokens":2}}}),
        ]
    };
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

fn request(messages: Vec<Message>, tools: Option<Vec<ToolSchema>>) -> GenerateRequest {
    GenerateRequest {
        provider: "openai-responses".into(),
        model: "relay-codex".into(),
        reasoning_effort: Some("high".into()),
        messages,
        system: Some("You are a coding agent.".into()),
        tools,
        temperature: None,
        max_tokens: Some(8),
        stop: None,
        session_id: Some(SessionId::from("session-1")),
        purpose: None,
    }
}

fn user(text: &str) -> Message {
    Message {
        id: MessageId::random(),
        role: MessageRole::User,
        content: vec![ContentBlock::Text { text: text.into() }],
        source: MessageSource::User,
    }
}
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes
}

#[tokio::test]
async fn responses_materializes_durable_images_in_order_and_tool_output_arrays() {
    let state = Arc::new(RelayState::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/v1/responses", post(responses))
        .with_state(Arc::clone(&state));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let data = TempDir::new();
    let store = Arc::new(AttachmentStore::new(&data.0, Default::default()).unwrap());
    let reference = store
        .save(AttachmentInput::new(png(1, 1), Some("one.png".into())))
        .await
        .unwrap();
    let model = ResponsesModel::new("relay-codex").with_input(["text", "image"]);
    let route = ResponsesRoute::new(
        "openai-responses",
        "Relay",
        format!("http://{address}/v1/"),
        "relay-key",
        vec![model.clone()],
    );
    let resolver = move |provider: &str, model_id: &str| {
        assert_eq!(provider, "openai-responses");
        assert_eq!(model_id, "relay-codex");
        ProviderSnapshot::new(route.clone(), model.clone(), "relay-key")
    };
    let adapter = Arc::new(OpenAiResponsesAdapter::with_resolver_and_store(
        resolver, store,
    ));
    let runtime = LlmRuntime::new();
    let _registration = runtime.register("openai-responses", adapter).unwrap();
    let cancellation = ContextHandle::root().scope().cancellation();
    let first_user = Message {
        id: MessageId::random(),
        role: MessageRole::User,
        content: vec![
            ContentBlock::Text {
                text: "before".into(),
            },
            ContentBlock::Image {
                attachment: serde_json::to_value(&reference).unwrap(),
            },
            ContentBlock::Text {
                text: "after".into(),
            },
        ],
        source: MessageSource::User,
    };
    let first = runtime
        .complete(
            request(vec![first_user.clone()], None),
            cancellation.clone(),
        )
        .await
        .unwrap();
    let tool_result = Message {
        id: MessageId::random(),
        role: MessageRole::User,
        content: vec![ContentBlock::ToolResult {
            tool_call_id: ToolCallId::from("call_1"),
            content: vec![
                ContentBlock::Text {
                    text: "tool text".into(),
                },
                ContentBlock::Image {
                    attachment: serde_json::to_value(&reference).unwrap(),
                },
            ],
            is_error: Some(false),
        }],
        source: MessageSource::Tool {
            call_id: ToolCallId::from("call_1"),
        },
    };
    runtime
        .complete(
            request(vec![first_user, first.message, tool_result], None),
            cancellation,
        )
        .await
        .unwrap();
    let requests = state.requests.lock().unwrap();
    let first_content = requests[0]["input"][0]["content"].as_array().unwrap();
    assert_eq!(
        first_content[0],
        json!({"type":"input_text","text":"before"})
    );
    assert_eq!(
        first_content[2],
        json!({"type":"input_text","text":"after"})
    );
    assert!(first_content[1]["image_url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    let second_output = requests[1]["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert!(second_output["output"].is_array());
    assert_eq!(
        second_output["output"][0],
        json!({"type":"input_text","text":"tool text"})
    );
    assert!(second_output["output"][1]["image_url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    server.abort();
}
#[tokio::test]
async fn native_responses_streams_tools_and_replays_encrypted_reasoning_to_a_relay() {
    let state = Arc::new(RelayState::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/v1/responses", post(responses))
        .with_state(Arc::clone(&state));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let adapter = Arc::new(
        OpenAiResponsesAdapter::new(&format!("http://{address}/v1/"), "relay-key").unwrap(),
    );
    let runtime = LlmRuntime::new();
    let _registration = runtime
        .register("openai-responses", adapter)
        .expect("provider registers");
    let cancellation = ContextHandle::root().scope().cancellation();
    let user = user("inspect the repository");
    let tools = vec![ToolSchema {
        name: "bash".into(),
        description: "run a command".into(),
        parameters: json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
    }];

    let first = runtime
        .complete(
            request(vec![user.clone()], Some(tools)),
            cancellation.clone(),
        )
        .await
        .unwrap();
    assert_eq!(first.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        first.message.content,
        vec![
            ContentBlock::Reasoning {
                text: "plan\n\nnext".into()
            },
            ContentBlock::ToolCall {
                id: ToolCallId::from("call_1"),
                name: "bash".into(),
                arguments: "{\"command\":\"pwd\"}".into(),
            },
        ]
    );
    assert_eq!(first.usage.unwrap().input_tokens, 8);

    let tool_result = Message {
        id: MessageId::random(),
        role: MessageRole::User,
        content: vec![ContentBlock::ToolResult {
            tool_call_id: ToolCallId::from("call_1"),
            content: vec![ContentBlock::Text {
                text: "/workspace".into(),
            }],
            is_error: Some(false),
        }],
        source: MessageSource::Tool {
            call_id: ToolCallId::from("call_1"),
        },
    };
    let second = runtime
        .complete(
            request(vec![user, first.message, tool_result], None),
            cancellation,
        )
        .await
        .unwrap();
    assert_eq!(second.finish_reason, FinishReason::Stop);
    assert_eq!(
        second.message.content,
        vec![ContentBlock::Text {
            text: "done".into()
        }]
    );

    let requests = state.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], json!("relay-codex"));
    assert_eq!(requests[0]["stream"], json!(true));
    assert_eq!(requests[0]["store"], json!(false));
    assert_eq!(requests[0]["max_output_tokens"], json!(16));
    assert_eq!(
        requests[0]["reasoning"],
        json!({"effort":"high","summary":"auto"})
    );
    assert_eq!(
        requests[0]["include"],
        json!(["reasoning.encrypted_content"])
    );
    let second_input = requests[1]["input"].as_array().unwrap();
    assert!(second_input.iter().any(|item| {
        item["type"] == "reasoning" && item["encrypted_content"] == "encrypted-reasoning"
    }));
    assert!(second_input
        .iter()
        .any(|item| { item["type"] == "function_call" && item["call_id"] == "call_1" }));
    assert!(second_input.iter().any(|item| {
        item["type"] == "function_call_output"
            && item["call_id"] == "call_1"
            && item["output"] == "/workspace"
    }));
    assert_eq!(
        *state.authorizations.lock().unwrap(),
        vec!["Bearer relay-key", "Bearer relay-key"]
    );

    server.abort();
}

async fn text_response() -> Response<Body> {
    let events = [
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_live","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":"live relay"}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_live","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"live relay","annotations":[]}]}}),
        json!({"type":"response.completed","response":{"id":"resp_live","status":"completed","output":[{"id":"msg_live","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"live relay","annotations":[]}]}],"usage":{"input_tokens":1,"output_tokens":2}}}),
    ];
    let body = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tessivum-openai-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn headless_binary_uses_openai_responses_environment() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/responses", post(text_response)),
        )
        .await
        .unwrap();
    });
    let data = TempDir::new();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_tessivum"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("OPENAI_API_KEY", "relay-key")
        .env("OPENAI_BASE_URL", format!("http://{address}/v1"))
        .args([
            "--data-dir",
            data.0.to_str().unwrap(),
            "--session",
            "live-process",
            "--provider",
            "openai-responses",
            "--model",
            "relay-codex",
            "say hello",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "live relay"
    );
    server.abort();
}

#[test]
fn responses_route_rejects_unknown_modalities_and_duplicate_models() {
    let invalid = ResponsesRoute::new(
        "relay",
        "Relay",
        "https://relay.example/v1",
        "relay-key",
        vec![ResponsesModel::new("model").with_input(["audio"])],
    );
    let error = ProviderSnapshot::without_key(invalid, ResponsesModel::new("model"))
        .expect_err("unknown modality must fail closed");
    assert_eq!(error.code, "INVALID_OPENAI_MODALITY");

    let duplicate = ResponsesRoute::new(
        "relay",
        "Relay",
        "https://relay.example/v1",
        "relay-key",
        vec![ResponsesModel::new("model"), ResponsesModel::new("model")],
    );
    let error = ProviderSnapshot::without_key(duplicate, ResponsesModel::new("model"))
        .expect_err("duplicate model ids must fail closed");
    assert_eq!(error.code, "INVALID_OPENAI_MODEL");
}

#[test]
fn provider_snapshot_debug_and_errors_do_not_expose_credentials() {
    let secret = "super-secret-key";
    let route = ResponsesRoute::new(
        "relay",
        "Relay",
        "https://relay.example/v1",
        "relay-key",
        vec![ResponsesModel::new("model")],
    );
    let snapshot = ProviderSnapshot::new(route, ResponsesModel::new("model"), secret).unwrap();
    assert!(!format!("{snapshot:?}").contains(secret));
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(secret));
}

#[test]
fn resolver_trait_captures_route_and_model_per_call() {
    let route = ResponsesRoute::new(
        "relay",
        "Relay",
        "https://relay.example/v1",
        "relay-key",
        vec![ResponsesModel::new("model")],
    );
    let resolver = move |provider: &str, model: &str| {
        assert_eq!(provider, "relay");
        assert_eq!(model, "model");
        ProviderSnapshot::new(route.clone(), ResponsesModel::new(model), "secret")
    };
    let snapshot = resolver.resolve("relay", "model").unwrap();
    assert_eq!(snapshot.route.id, "relay");
    assert_eq!(snapshot.model.id, "model");
}

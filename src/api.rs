//! HTTP, SSE, and WebSocket transport over the shared host contract.
//!
//! The transport deliberately owns no durable state: `HostApi` remains the only
//! authority for admission, event replay, and cancellation.

use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    future::Future,
    io,
    net::{Ipv4Addr, SocketAddr},
    path::Path as FsPath,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::stream;
use axum::{
    body::to_bytes,
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Path, Request, State,
    },
    http::{header, HeaderMap, StatusCode, Uri},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures_util::{
    stream::{FuturesUnordered, StreamExt},
    SinkExt,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::{
    net::TcpListener,
    sync::{broadcast, mpsc, oneshot, Mutex as AsyncMutex},
    task::JoinHandle,
};

use crate::{
    frontend::FrontendStatic,
    host::{HostApi, HostDescriptor, HostNotification},
    protocol::{
        AgentCancelCause, InitializeParams, SessionEventNotification, SessionId,
        SessionPromptParams, MAX_SAFE_INTEGER,
    },
    TessivumError,
};
use uuid::Uuid;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Per-WebSocket outgoing message cap. A slow peer is disconnected instead of
/// retaining unbounded host notifications.
pub const MAX_SOCKET_QUEUE: usize = 32;
const MAX_REQUEST_ID_BYTES: usize = 128;

/// A bind configuration for [`ApiServer`]. The default only listens on loopback
/// and lets the OS select a port.
pub struct ApiServerConfig {
    pub bind_addr: SocketAddr,
    pub frontend: Option<FrontendStatic>,
}

impl Default for ApiServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            frontend: None,
        }
    }
}

/// A running API listener. Dropping it stops admission; [`Self::shutdown`] also
/// waits for the listener and every streaming socket to exit.
pub struct ApiServer {
    address: SocketAddr,
    socket_shutdown: broadcast::Sender<()>,
    listener_shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl ApiServer {
    /// Binds a loopback ephemeral port.
    pub async fn bind(host: Arc<dyn HostApi>) -> io::Result<Self> {
        Self::bind_with_config(host, ApiServerConfig::default()).await
    }

    /// Binds an explicit address without static frontend assets.
    pub async fn bind_at(host: Arc<dyn HostApi>, bind_addr: SocketAddr) -> io::Result<Self> {
        Self::bind_with_config(
            host,
            ApiServerConfig {
                bind_addr,
                frontend: None,
            },
        )
        .await
    }

    /// Binds the configured listener and starts the transport task.
    pub async fn bind_with_config(
        host: Arc<dyn HostApi>,
        config: ApiServerConfig,
    ) -> io::Result<Self> {
        if !config.bind_addr.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "API listeners require a loopback bind address",
            ));
        }
        let listener = TcpListener::bind(config.bind_addr).await?;
        let address = listener.local_addr()?;
        let (socket_shutdown, _) = broadcast::channel(1);
        let (listener_shutdown, listener_stopped) = oneshot::channel();
        let app = router_with_shutdown(host, config.frontend, socket_shutdown.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = listener_stopped.await;
                })
                .await
        });

        Ok(Self {
            address,
            socket_shutdown,
            listener_shutdown: Some(listener_shutdown),
            task: Some(task),
        })
    }

    /// The actual bound socket address, including an OS-selected port.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// Stops accepts, wakes every SSE/WebSocket handler, and waits for them.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        let _ = self.socket_shutdown.send(());
        if let Some(stop) = self.listener_shutdown.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|error| io::Error::other(error.to_string()))??;
        }
        Ok(())
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        let _ = self.socket_shutdown.send(());
        if let Some(stop) = self.listener_shutdown.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Builds the API router. `frontend` is intentionally accepted at this
/// boundary so a frontend profile can compose the same host object without
/// giving static routes any host privileges.
pub fn router(host: Arc<dyn HostApi>, frontend: Option<FrontendStatic>) -> Router {
    let (socket_shutdown, _) = broadcast::channel(1);
    router_with_shutdown(host, frontend, socket_shutdown)
}

fn router_with_shutdown(
    host: Arc<dyn HostApi>,
    frontend: Option<FrontendStatic>,
    socket_shutdown: broadcast::Sender<()>,
) -> Router {
    let compat = Arc::new(CompatibilityState::new(host.descriptor()));
    let state = ApiState {
        host,
        socket_shutdown,
        frontend,
        compat,
    };
    Router::new()
        // Static routes are registered before the catch-all unary route.
        .route("/events/{session}", get(sse_events))
        .route("/ws", get(websocket_upgrade))
        .route("/api/events.mux", get(compat_events_mux))
        .route("/api/events.host", get(compat_events_host))
        .route(
            "/api/{method}",
            post(compat_unary).fallback(method_not_allowed),
        )
        .route(
            "/api/{namespace}/{method}",
            post(unary).fallback(method_not_allowed),
        )
        .fallback(frontend_fallback)
        .with_state(state)
}

/// Returns the longest segment-aligned route prefix in `prefixes`.
///
/// This is kept public for frontend composition: `/assets` matches
/// `/assets/app.js`, never `/assets-old`.
pub fn longest_prefix<'a>(
    path: &str,
    prefixes: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    prefixes
        .into_iter()
        .filter(|prefix| {
            path == *prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| prefix.ends_with('/') || suffix.starts_with('/'))
        })
        .max_by_key(|prefix| prefix.len())
}

#[derive(Clone)]
struct ApiState {
    host: Arc<dyn HostApi>,
    socket_shutdown: broadcast::Sender<()>,
    frontend: Option<FrontendStatic>,
    compat: Arc<CompatibilityState>,
}

const MAX_COMPAT_FRAME_QUEUE: usize = 32;

struct CompatibilityState {
    cwd: String,
    provider: String,
    model: String,
    max_tokens: Option<u64>,
    data: Mutex<CompatibilityData>,
    initialized: AsyncMutex<bool>,
    frames: broadcast::Sender<CompatFrame>,
}

struct CompatibilityData {
    workspaces: BTreeMap<String, CompatWorkspace>,
    sessions: BTreeMap<SessionId, CompatSession>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompatWorkspace {
    workspace_id: String,
    path: String,
    title: String,
    session_ids: Vec<SessionId>,
    created_at: String,
    updated_at: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompatSession {
    session_id: SessionId,
    updated_at: u64,
    running: bool,
    blank: bool,
    cwd: Option<String>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CompatStream {
    Host,
    Mux,
}

#[derive(Clone)]
struct CompatFrame {
    stream: CompatStream,
    payload: Value,
}

impl CompatibilityState {
    fn new(descriptor: HostDescriptor) -> Self {
        let timestamp = compat_timestamp();
        let workspace = CompatWorkspace {
            workspace_id: "default".into(),
            path: descriptor.cwd.clone(),
            title: workspace_title(&descriptor.cwd),
            session_ids: Vec::new(),
            created_at: timestamp.clone(),
            updated_at: timestamp,
        };
        let (frames, _) = broadcast::channel(MAX_COMPAT_FRAME_QUEUE);
        Self {
            cwd: descriptor.cwd,
            provider: descriptor.provider,
            model: descriptor.model,
            max_tokens: descriptor.max_tokens,
            data: Mutex::new(CompatibilityData {
                workspaces: BTreeMap::from([(workspace.workspace_id.clone(), workspace)]),
                sessions: BTreeMap::new(),
            }),
            initialized: AsyncMutex::new(false),
            frames,
        }
    }
}

fn compat_timestamp() -> String {
    "1970-01-01T00:00:00.000Z".into()
}

fn compat_updated_at() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(MAX_SAFE_INTEGER as u128) as u64
}

fn workspace_title(path: &str) -> String {
    FsPath::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(path)
        .to_owned()
}

fn compat_data(state: &CompatibilityState) -> std::sync::MutexGuard<'_, CompatibilityData> {
    state
        .data
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn broadcast_compat(state: &CompatibilityState, stream: CompatStream, payload: Value) {
    let _ = state.frames.send(CompatFrame { stream, payload });
}

async fn api_not_found() -> Response {
    response_error(None, ApiError::not_found())
}

async fn frontend_fallback(State(state): State<ApiState>, request: Request) -> Response {
    let path = request.uri().path();
    if path == "/api" || path.starts_with("/api/") {
        return api_not_found().await;
    }
    match state.frontend {
        Some(frontend) => frontend.serve(request.method().clone(), path),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequestEnvelope {
    request_id: String,
    args: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WsRequestEnvelope {
    request_id: String,
    namespace: String,
    method: String,
    args: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResponseEnvelope {
    request_id: Option<String>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorEnvelope>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope {
    code: String,
    message: String,
}

#[derive(Clone)]
struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "INVALID_REQUEST".into(),
            message: message.into(),
        }
    }

    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "METHOD_NOT_FOUND".into(),
            message: "unknown API method".into(),
        }
    }

    fn too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "PAYLOAD_TOO_LARGE".into(),
            message: "request frame exceeds the configured limit".into(),
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "HOST_UNAVAILABLE".into(),
            message: message.into(),
        }
    }
}

#[derive(Clone)]
struct CompatError {
    code: String,
    message: String,
    details: Value,
}

impl CompatError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "bad-request".into(),
            message: message.into(),
            details: json!({"issues": []}),
        }
    }

    fn not_found() -> Self {
        Self::internal("unknown API method")
    }

    fn too_large() -> Self {
        Self::internal("request frame exceeds the configured limit")
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: "internal".into(),
            message: message.into(),
            details: json!({}),
        }
    }
}

fn compat_host_error(error: TessivumError) -> CompatError {
    if error.code == "CANCELLED" {
        CompatError {
            code: "cancelled".into(),
            message: error.message,
            details: json!({}),
        }
    } else {
        CompatError::internal(error.message)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatRequestEnvelope {
    #[serde(rename = "type")]
    request_type: String,
    rpc_id: String,
    method: String,
    payload: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompatEmptyPayload {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatWorkspaceCreate {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSessionCreate {
    workspace_id: Option<String>,
    cwd: Option<String>,
    session_id: Option<SessionId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSessionRef {
    session_id: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSubagentList {
    parent_session_id: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatHistory {
    session_id: SessionId,
    before_seq: Option<u64>,
    max_messages: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSessionModels {
    session_id: Option<SessionId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatLlmModels {
    provider: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatCredentialsDescribe {
    refs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatPrompt {
    session_id: SessionId,
    mode: CompatPromptMode,
    content: Vec<CompatPromptContent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum CompatPromptMode {
    Queue,
    Steer,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum CompatPromptContent {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        data: String,
        name: Option<String>,
    },
}

async fn compat_unary(
    State(state): State<ApiState>,
    Path(method): Path<String>,
    request: Request,
) -> Response {
    if !is_json(request.headers()) {
        return compat_response_error(
            Value::Null,
            CompatError::invalid("Content-Type must be application/json"),
        );
    }
    if content_length_exceeds(request.headers(), MAX_FRAME_BYTES) {
        return compat_response_error(Value::Null, CompatError::too_large());
    }
    let body = match to_bytes(request.into_body(), MAX_FRAME_BYTES).await {
        Ok(body) => body,
        Err(_) => return compat_response_error(Value::Null, CompatError::too_large()),
    };
    let envelope: CompatRequestEnvelope = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(error) => {
            return compat_response_error(Value::Null, CompatError::invalid(error.to_string()))
        }
    };
    if let Err(error) = validate_request_id(&envelope.rpc_id) {
        return compat_response_error(Value::Null, compat_api_error(error));
    }
    let rpc_id = Value::String(envelope.rpc_id);
    if envelope.request_type != "client-request" {
        return compat_response_error(rpc_id, CompatError::invalid("type must be client-request"));
    }
    if envelope.method != method {
        return compat_response_error(
            rpc_id,
            CompatError::invalid("URL method must match body method"),
        );
    }
    match compat_dispatch(&state, &method, envelope.payload).await {
        Ok(value) => compat_response_ok(rpc_id, value),
        Err(error) => compat_response_error(rpc_id, error),
    }
}

fn compat_api_error(error: ApiError) -> CompatError {
    CompatError::internal(error.message)
}

fn compat_response_ok(rpc_id: Value, value: Value) -> Response {
    compat_response(rpc_id, json!({"ok": true, "value": value}))
}

fn compat_response_error(rpc_id: Value, error: CompatError) -> Response {
    compat_response(
        rpc_id,
        json!({
            "ok": false,
            "error": {
                "code": error.code,
                "message": error.message,
                "details": error.details,
            }
        }),
    )
}

fn compat_response(rpc_id: Value, result: Value) -> Response {
    let response_rpc_id = rpc_id.clone();
    let body = json!({
        "type": "server-response",
        "rpcId": rpc_id,
        "result": result,
    });
    if serde_json::to_vec(&body).is_ok_and(|body| body.len() <= MAX_FRAME_BYTES) {
        return (StatusCode::OK, Json(body)).into_response();
    }
    (
        StatusCode::OK,
        Json(json!({
            "type": "server-response",
            "rpcId": response_rpc_id,
            "result": {"ok": false, "error": {
                "code": "internal",
                "message": "response frame exceeds the configured limit",
                "details": {},
            }},
        })),
    )
        .into_response()
}

async fn compat_dispatch(
    state: &ApiState,
    method: &str,
    payload: Value,
) -> Result<Value, CompatError> {
    match method {
        "host.describe" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            let data = compat_data(&state.compat);
            Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "cwd": state.compat.cwd,
                "provider": state.compat.provider,
                "model": state.compat.model,
                "attachedSessions": data.sessions.len(),
            }))
        }
        "settings.describe" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            Ok(json!({"writable": false, "hasDocument": false, "namespaces": []}))
        }
        "workspace.list" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            compat_sync_sessions(state).await?;
            let data = compat_data(&state.compat);
            Ok(json!({
                "items": data.workspaces.values().cloned().collect::<Vec<_>>(),
                "archivedSessionIds": [],
            }))
        }
        "workspace.create" => compat_workspace_create(state, compat_decode(payload)?),
        "session.list" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            compat_sync_sessions(state).await?;
            let data = compat_data(&state.compat);
            Ok(json!({"items": data.sessions.values().cloned().collect::<Vec<_>>() }))
        }
        "session.create" => compat_session_create(state, compat_decode(payload)?).await,
        "session.history" => compat_session_history(state, compat_decode(payload)?).await,
        "session.models" => {
            let args: CompatSessionModels = compat_decode(payload)?;
            if let Some(session_id) = args.session_id {
                compat_require_session(&session_id)?;
            }
            Ok(compat_session_models(&state.compat))
        }
        "session.prompt" => compat_session_prompt(state, compat_decode(payload)?).await,
        "session.cancel" => compat_session_cancel(state, compat_decode(payload)?).await,
        "subagent.list" => {
            let args: CompatSubagentList = compat_decode(payload)?;
            compat_require_session(&args.parent_session_id)?;
            Ok(json!({"entries": [], "parentAvailable": true}))
        }
        "command.list" => {
            let _: CompatSessionRef = compat_decode(payload)?;
            Ok(json!({"commands": []}))
        }
        "skill.list" => {
            let _: CompatSessionRef = compat_decode(payload)?;
            Ok(json!({"skills": []}))
        }
        "agentPreset.list" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            Ok(json!({
                "presets": [{
                    "id": "default",
                    "trust": "system",
                    "isDefault": true,
                    "name": "Default",
                }],
                "authorable": false,
                "hasDocument": false,
            }))
        }
        "llm.providers" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            Ok(json!({"providers": [compat_provider(&state.compat)]}))
        }
        "llm.models" => {
            let args: CompatLlmModels = compat_decode(payload)?;
            if let Some(provider) = args.provider {
                compat_require_nonblank("provider", &provider)?;
                if provider != state.compat.provider {
                    return Err(CompatError::invalid("provider is not available"));
                }
            }
            Ok(compat_llm_models(&state.compat))
        }
        "credentials.describe" => {
            let args: CompatCredentialsDescribe = compat_decode(payload)?;
            let credentials = args
                .refs
                .into_iter()
                .map(|reference| {
                    if reference.trim().is_empty() {
                        Err(CompatError::invalid("credential refs must not be blank"))
                    } else {
                        Ok((reference, json!({"configured": false, "writable": false})))
                    }
                })
                .collect::<Result<Map<String, Value>, CompatError>>()?;
            Ok(json!({"credentials": credentials}))
        }
        _ => Err(CompatError::not_found()),
    }
}

fn compat_decode<T: DeserializeOwned>(payload: Value) -> Result<T, CompatError> {
    serde_json::from_value(payload).map_err(|error| CompatError::invalid(error.to_string()))
}

fn compat_require_nonblank(field: &str, value: &str) -> Result<(), CompatError> {
    if value.trim().is_empty() {
        return Err(CompatError::invalid(format!("{field} must not be blank")));
    }
    Ok(())
}

fn compat_require_session(session: &SessionId) -> Result<(), CompatError> {
    compat_require_nonblank("sessionId", session.as_str())
}

fn compat_canonical_directory(path: &str) -> Result<String, CompatError> {
    compat_require_nonblank("path", path)?;
    let path = fs::canonicalize(path)
        .map_err(|error| CompatError::invalid(format!("path cannot be canonicalized: {error}")))?;
    if !path.is_dir() {
        return Err(CompatError::invalid("path must be a directory"));
    }
    Ok(path.to_string_lossy().into_owned())
}

fn compat_workspace_create(
    state: &ApiState,
    args: CompatWorkspaceCreate,
) -> Result<Value, CompatError> {
    let path = compat_canonical_directory(&args.path)?;
    if path != state.compat.cwd {
        return Err(CompatError::invalid(
            "this Host runtime supports only its configured workspace cwd",
        ));
    }
    let workspace = compat_data(&state.compat)
        .workspaces
        .get("default")
        .expect("default workspace exists")
        .clone();
    broadcast_compat(
        &state.compat,
        CompatStream::Host,
        json!({"type": "host/workspace-changed", "workspace": workspace}),
    );
    Ok(json!({"workspace": workspace, "created": false}))
}

async fn compat_initialize(state: &ApiState) -> Result<(), CompatError> {
    let mut initialized = state.compat.initialized.lock().await;
    if *initialized {
        return Ok(());
    }
    state
        .host
        .initialize(InitializeParams {
            cwd: state.compat.cwd.clone(),
            provider: state.compat.provider.clone(),
            model: state.compat.model.clone(),
            max_tokens: state.compat.max_tokens,
        })
        .await
        .map_err(compat_host_error)?;
    *initialized = true;
    Ok(())
}

async fn compat_sync_sessions(state: &ApiState) -> Result<(), CompatError> {
    let sessions = state
        .host
        .list_sessions()
        .await
        .map_err(compat_host_error)?;
    let mut data = compat_data(&state.compat);
    for session in sessions {
        if data.sessions.contains_key(&session.session_id) {
            continue;
        }
        let workspace_id = session
            .cwd
            .as_deref()
            .and_then(|cwd| {
                data.workspaces
                    .values()
                    .find(|workspace| workspace.path == cwd)
                    .map(|workspace| workspace.workspace_id.clone())
            })
            .unwrap_or_else(|| "default".into());
        let session_id = session.session_id.clone();
        data.sessions.insert(
            session_id.clone(),
            CompatSession {
                session_id: session.session_id,
                updated_at: session.created_at.min(MAX_SAFE_INTEGER),
                running: false,
                blank: session.event_count == 0,
                cwd: session.cwd,
            },
        );
        let workspace = data
            .workspaces
            .get_mut(&workspace_id)
            .expect("default workspace exists");
        if !workspace.session_ids.contains(&session_id) {
            workspace.session_ids.push(session_id);
        }
    }
    Ok(())
}

async fn compat_session_create(
    state: &ApiState,
    args: CompatSessionCreate,
) -> Result<Value, CompatError> {
    if args.workspace_id.is_some() && args.cwd.is_some() {
        return Err(CompatError::invalid(
            "workspaceId and cwd are mutually exclusive",
        ));
    }
    if let Some(session_id) = &args.session_id {
        compat_require_session(session_id)?;
    }
    let cwd = args
        .cwd
        .as_deref()
        .map(compat_canonical_directory)
        .transpose()?;
    if args
        .workspace_id
        .as_deref()
        .is_some_and(|id| id != "default")
    {
        return Err(CompatError::invalid("workspaceId is not known"));
    }
    if cwd.as_deref().is_some_and(|cwd| cwd != state.compat.cwd) {
        return Err(CompatError::invalid(
            "this Host runtime supports only its configured workspace cwd",
        ));
    }
    compat_initialize(state).await?;
    compat_sync_sessions(state).await?;
    let session_id = args.session_id.unwrap_or_else(SessionId::random);
    if compat_data(&state.compat)
        .sessions
        .contains_key(&session_id)
    {
        return Ok(json!({"sessionId": session_id}));
    }
    let persisted = state
        .host
        .create_session(session_id.clone())
        .await
        .map_err(compat_host_error)?;
    let session = CompatSession {
        session_id: session_id.clone(),
        updated_at: persisted.created_at.min(MAX_SAFE_INTEGER),
        running: false,
        blank: persisted.event_count == 0,
        cwd: persisted.cwd.or_else(|| Some(state.compat.cwd.clone())),
    };
    let mut data = compat_data(&state.compat);
    data.sessions
        .entry(session_id.clone())
        .or_insert_with(|| session.clone());
    let workspace = data
        .workspaces
        .get_mut("default")
        .expect("default workspace exists");
    if !workspace.session_ids.contains(&session_id) {
        workspace.session_ids.push(session_id.clone());
    }
    workspace.updated_at = compat_timestamp();
    let workspace = workspace.clone();
    drop(data);
    broadcast_compat(
        &state.compat,
        CompatStream::Host,
        json!({"type": "host/workspace-changed", "workspace": workspace}),
    );
    broadcast_compat(
        &state.compat,
        CompatStream::Host,
        json!({
            "type": "host/session-added",
            "sessionId": session.session_id,
            "blank": session.blank,
            "cwd": session.cwd,
        }),
    );
    Ok(json!({"sessionId": session_id}))
}

async fn compat_session_history(
    state: &ApiState,
    args: CompatHistory,
) -> Result<Value, CompatError> {
    compat_require_session(&args.session_id)?;
    if let Some(before_seq) = args.before_seq {
        if before_seq > MAX_SAFE_INTEGER {
            return Err(CompatError::invalid(
                "beforeSeq exceeds the supported range",
            ));
        }
    }
    let max_messages = args.max_messages.unwrap_or(1_000);
    if max_messages > 1_000 {
        return Err(CompatError::invalid(
            "maxMessages exceeds the supported range",
        ));
    }
    let mut events: Vec<_> = match state.host.events(args.session_id, 0).await {
        Ok(events) => events,
        Err(error) if error.code == "SESSION_NOT_FOUND" => Vec::new(),
        Err(error) => return Err(compat_host_error(error)),
    }
    .into_iter()
    .filter(|event| {
        args.before_seq
            .is_none_or(|before_seq| event.seq < before_seq)
    })
    .collect();
    let has_more = events.len() > max_messages;
    if has_more {
        events.drain(..events.len() - max_messages);
    }
    Ok(json!({
        "events": events.into_iter().map(|event| json!({"event": event})).collect::<Vec<_>>(),
        "hasMore": has_more,
    }))
}

fn compat_session_models(state: &CompatibilityState) -> Value {
    json!({
        "current": {"provider": state.provider, "model": state.model},
        "routable": true,
        "groups": compat_model_groups(state),
        "failures": [],
    })
}

fn compat_provider(state: &CompatibilityState) -> Value {
    json!({
        "provider": state.provider,
        "displayName": state.provider,
        "settingsNs": "llm",
        "settingsPath": [],
        "active": true,
        "declared": true,
    })
}

fn compat_model_groups(state: &CompatibilityState) -> Value {
    json!([{
        "id": state.provider,
        "name": state.provider,
        "models": [{"id": state.model, "name": state.model}],
    }])
}

fn compat_llm_models(state: &CompatibilityState) -> Value {
    json!({"groups": compat_model_groups(state), "failures": []})
}

fn compat_prompt_blocks(
    content: Vec<CompatPromptContent>,
) -> Result<Vec<crate::protocol::ContentBlock>, CompatError> {
    if content.is_empty() {
        return Err(CompatError::invalid("content must not be empty"));
    }
    content
        .into_iter()
        .map(|content| match content {
            CompatPromptContent::Text { text } => Ok(crate::protocol::ContentBlock::Text { text }),
            CompatPromptContent::Image {
                media_type,
                data,
                name,
            } => {
                compat_require_nonblank("mediaType", &media_type)?;
                compat_require_nonblank("data", &data)?;
                let mut attachment = Map::new();
                attachment.insert("mediaType".into(), Value::String(media_type));
                attachment.insert("data".into(), Value::String(data));
                if let Some(name) = name {
                    attachment.insert("name".into(), Value::String(name));
                }
                Ok(crate::protocol::ContentBlock::Image {
                    attachment: Value::Object(attachment),
                })
            }
        })
        .collect()
}

async fn compat_session_prompt(state: &ApiState, args: CompatPrompt) -> Result<Value, CompatError> {
    let CompatPrompt {
        session_id,
        mode,
        content,
    } = args;
    compat_require_session(&session_id)?;
    let params = SessionPromptParams {
        session_id: session_id.clone(),
        content_blocks: compat_prompt_blocks(content)?,
    };
    match mode {
        CompatPromptMode::Queue => state.host.prompt(params).await,
        CompatPromptMode::Steer => state.host.steer(params).await,
    }
    .map_err(compat_host_error)?;
    if let Some(session) = compat_data(&state.compat).sessions.get_mut(&session_id) {
        session.blank = false;
        session.updated_at = compat_updated_at();
    }
    Ok(json!({"accepted": true}))
}

async fn compat_session_cancel(
    state: &ApiState,
    args: CompatSessionRef,
) -> Result<Value, CompatError> {
    compat_require_session(&args.session_id)?;
    state
        .host
        .cancel(args.session_id.clone(), AgentCancelCause::User)
        .await
        .map_err(compat_host_error)?;
    if let Some(session) = compat_data(&state.compat)
        .sessions
        .get_mut(&args.session_id)
    {
        session.running = false;
        session.updated_at = compat_updated_at();
    }
    Ok(json!({"accepted": true}))
}

async fn compat_events_mux(
    State(state): State<ApiState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    compat_upgrade(state, headers, upgrade, CompatStream::Mux)
}

async fn compat_events_host(
    State(state): State<ApiState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    compat_upgrade(state, headers, upgrade, CompatStream::Host)
}

fn compat_upgrade(
    state: ApiState,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
    stream_kind: CompatStream,
) -> Response {
    if !websocket_origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    upgrade
        .max_frame_size(MAX_FRAME_BYTES)
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| compat_websocket(socket, state, stream_kind))
}

async fn compat_websocket(mut socket: WebSocket, state: ApiState, stream_kind: CompatStream) {
    let mut frames = state.compat.frames.subscribe();
    let mut notifications = state.host.subscribe();
    let mut shutdown = state.socket_shutdown.subscribe();
    loop {
        tokio::select! {
            _ = shutdown.recv() => return,
            incoming = socket.recv() => match incoming {
                Some(Ok(WsMessage::Ping(payload))) => {
                    if !compat_socket_send(&mut socket, WsMessage::Pong(payload)).await { return; }
                }
                Some(Ok(WsMessage::Pong(_))) => {}
                Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(WsMessage::Text(_) | WsMessage::Binary(_))) => return,
            },
            frame = frames.recv() => match frame {
                Ok(frame) if frame.stream == stream_kind => {
                    let message = match compat_ws_message(frame.payload) {
                        Ok(message) => message,
                        Err(message) => compat_ws_message(compat_stream_error_payload(message))
                            .expect("stream error frame is bounded"),
                    };
                    if !compat_socket_send(&mut socket, message).await { return; }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    let message = compat_ws_message(compat_stream_error_payload(format!(
                        "{dropped} compatibility frames were dropped"
                    ))).expect("stream error frame is bounded");
                    let _ = compat_socket_send(&mut socket, message).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            notification = notifications.recv() => match notification {
                Ok(notification) => {
                    if let Some(frame) = compat_notification(&state, notification) {
                        if frame.stream == stream_kind {
                            let message = match compat_ws_message(frame.payload) {
                                Ok(message) => message,
                                Err(message) => compat_ws_message(compat_stream_error_payload(message))
                                    .expect("stream error frame is bounded"),
                            };
                            if !compat_socket_send(&mut socket, message).await { return; }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    let message = compat_ws_message(compat_stream_error_payload(format!(
                        "{dropped} host notifications were dropped"
                    ))).expect("stream error frame is bounded");
                    let _ = compat_socket_send(&mut socket, message).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

async fn compat_socket_send(socket: &mut WebSocket, message: WsMessage) -> bool {
    tokio::time::timeout(Duration::from_secs(1), socket.send(message))
        .await
        .is_ok_and(|result| result.is_ok())
}

fn compat_notification(state: &ApiState, notification: HostNotification) -> Option<CompatFrame> {
    match notification {
        HostNotification::SessionEvent(notification) => Some(CompatFrame {
            stream: CompatStream::Mux,
            payload: json!({
                "type": "session/event",
                "sessionId": notification.session_id,
                "event": notification.event,
            }),
        }),
        HostNotification::SessionStatus(notification) => {
            if let Some(session) = compat_data(&state.compat)
                .sessions
                .get_mut(&notification.session_id)
            {
                session.running = notification.status == crate::protocol::SessionStatus::Running;
                session.updated_at = compat_updated_at();
            }
            Some(CompatFrame {
                stream: CompatStream::Host,
                payload: json!({
                    "type": "host/session-status",
                    "sessionId": notification.session_id,
                    "running": notification.status == crate::protocol::SessionStatus::Running,
                }),
            })
        }
        HostNotification::SubagentStarted(_) | HostNotification::SubagentFinished(_) => None,
    }
}

fn compat_ws_message(payload: Value) -> Result<WsMessage, String> {
    let method = payload
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "compatibility frame has no type".to_owned())?;
    let data = serde_json::to_string(&json!({
        "type": "server-request",
        "rpcId": Uuid::new_v4().to_string(),
        "method": method,
        "payload": payload,
    }))
    .map_err(|_| "compatibility frame cannot be serialized".to_owned())?;
    if data.len() > MAX_FRAME_BYTES {
        return Err("compatibility frame exceeds the configured limit".into());
    }
    Ok(WsMessage::Text(data.into()))
}

fn compat_stream_error_payload(message: String) -> Value {
    json!({
        "type": "stream/error",
        "error": {"code": "internal", "message": message, "details": {}},
    })
}

fn host_error(error: TessivumError) -> ApiError {
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: error.code,
        message: error.message,
    }
}

fn protocol_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::bad_request(error.to_string())
}

fn response_ok(request_id: String, output: Value) -> Response {
    response(
        StatusCode::OK,
        ResponseEnvelope {
            request_id: Some(request_id),
            ok: true,
            output: Some(output),
            error: None,
        },
    )
}

fn response_error(request_id: Option<String>, error: ApiError) -> Response {
    response(
        error.status,
        ResponseEnvelope {
            request_id,
            ok: false,
            output: None,
            error: Some(ErrorEnvelope {
                code: error.code,
                message: error.message,
            }),
        },
    )
}

fn response(status: StatusCode, body: ResponseEnvelope) -> Response {
    (status, Json(body)).into_response()
}

async fn method_not_allowed() -> Response {
    response_error(
        None,
        ApiError {
            status: StatusCode::METHOD_NOT_ALLOWED,
            code: "METHOD_NOT_ALLOWED".into(),
            message: "only POST is accepted for API methods".into(),
        },
    )
}

async fn unary(
    State(state): State<ApiState>,
    Path((namespace, method)): Path<(String, String)>,
    request: Request,
) -> Response {
    let headers = request.headers();
    if !is_json(headers) {
        return response_error(
            None,
            ApiError::bad_request("Content-Type must be application/json"),
        );
    }
    if content_length_exceeds(headers, MAX_FRAME_BYTES) {
        return response_error(None, ApiError::too_large());
    }

    let body = match to_bytes(request.into_body(), MAX_FRAME_BYTES).await {
        Ok(body) => body,
        Err(_) => return response_error(None, ApiError::too_large()),
    };
    let envelope: RequestEnvelope = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(error) => return response_error(None, ApiError::bad_request(error.to_string())),
    };
    if let Err(error) = validate_request_id(&envelope.request_id) {
        return response_error(None, error);
    }
    if !is_known_method(&namespace, &method) {
        return response_error(Some(envelope.request_id), ApiError::not_found());
    }

    match dispatch(state.host, &namespace, &method, envelope.args).await {
        Ok(output) => response_ok(envelope.request_id, output),
        Err(error) => response_error(Some(envelope.request_id), error),
    }
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("application/json")
            })
        })
        .unwrap_or(false)
}

fn content_length_exceeds(headers: &HeaderMap, max: usize) -> bool {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > max)
}

fn validate_request_id(request_id: &str) -> Result<(), ApiError> {
    if request_id.is_empty()
        || request_id.len() > MAX_REQUEST_ID_BYTES
        || request_id.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(
            "requestId must be a non-empty printable identifier within the size limit",
        ));
    }
    Ok(())
}

fn is_known_method(namespace: &str, method: &str) -> bool {
    matches!(
        (namespace, method),
        ("session", "initialize")
            | ("session", "prompt")
            | ("session", "events")
            | ("session", "status")
            | ("session", "cancel")
            | ("host", "shutdown")
    )
}

async fn dispatch(
    host: Arc<dyn HostApi>,
    namespace: &str,
    method: &str,
    args: Value,
) -> Result<Value, ApiError> {
    match (namespace, method) {
        ("session", "initialize") => {
            let args: InitializeArgs = decode(args)?;
            require_nonblank("cwd", &args.cwd)?;
            require_nonblank("provider", &args.provider)?;
            require_nonblank("model", &args.model)?;
            let params = InitializeParams {
                cwd: args.cwd,
                provider: args.provider,
                model: args.model,
                max_tokens: args.max_tokens,
            };
            params.validate().map_err(protocol_error)?;
            serializable(host.initialize(params).await.map_err(host_error)?)
        }
        ("session", "prompt") => {
            let args: PromptArgs = decode(args)?;
            let params = SessionPromptParams {
                session_id: args.session_id,
                content_blocks: args.content_blocks,
            };
            require_session(&params.session_id)?;
            params.validate().map_err(protocol_error)?;
            serializable(host.prompt(params).await.map_err(host_error)?)
        }
        ("session", "events") => {
            let args: EventsArgs = decode(args)?;
            require_session(&args.session)?;
            require_safe_integer("fromSeq", args.from_seq)?;
            serializable(
                host.events(args.session, args.from_seq)
                    .await
                    .map_err(host_error)?,
            )
        }
        ("session", "status") => {
            let args: SessionArgs = decode(args)?;
            require_session(&args.session)?;
            serializable(host.status(args.session).await.map_err(host_error)?)
        }
        ("session", "cancel") => {
            let args: CancelArgs = decode(args)?;
            require_session(&args.session)?;
            validate_cancel_cause(&args.cause)?;
            serializable(
                host.cancel(args.session, args.cause)
                    .await
                    .map_err(host_error)?,
            )
        }
        ("host", "shutdown") => {
            let _: EmptyArgs = decode(args)?;
            host.shutdown().await.map_err(host_error)?;
            Ok(Value::Null)
        }
        _ => Err(ApiError::not_found()),
    }
}

fn decode<T: DeserializeOwned>(args: Value) -> Result<T, ApiError> {
    serde_json::from_value(args).map_err(|error| ApiError::bad_request(error.to_string()))
}

fn serializable(value: impl Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|_| ApiError::unavailable("host returned an invalid value"))
}

fn require_session(session: &SessionId) -> Result<(), ApiError> {
    require_nonblank("session", session.as_str())
}

fn require_nonblank(field: &str, value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty()
        || value.len() > MAX_FRAME_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(format!(
            "{field} must be a non-empty bounded identifier"
        )));
    }
    Ok(())
}

fn require_safe_integer(name: &str, value: u64) -> Result<(), ApiError> {
    if value > MAX_SAFE_INTEGER {
        return Err(ApiError::bad_request(format!(
            "{name} exceeds the JSON safe integer limit"
        )));
    }
    Ok(())
}

fn validate_cancel_cause(cause: &AgentCancelCause) -> Result<(), ApiError> {
    if let AgentCancelCause::Hook { reason } = cause {
        if reason.trim().is_empty() || reason.len() > MAX_FRAME_BYTES {
            return Err(ApiError::bad_request(
                "hook cancellation reason must be non-empty and bounded",
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InitializeArgs {
    cwd: String,
    provider: String,
    model: String,
    max_tokens: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PromptArgs {
    session_id: SessionId,
    content_blocks: Vec<crate::protocol::ContentBlock>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EventsArgs {
    session: SessionId,
    from_seq: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionArgs {
    session: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelArgs {
    session: SessionId,
    cause: AgentCancelCause,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

fn sse_from_query(uri: &Uri, headers: &HeaderMap) -> Result<u64, ApiError> {
    let mut from = None;
    if let Some(query) = uri.query() {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if key != "from" || from.is_some() {
                return Err(ApiError::bad_request(
                    "only one numeric from query parameter is accepted",
                ));
            }
            from = Some(
                value
                    .parse::<u64>()
                    .map_err(|_| ApiError::bad_request("from must be an unsigned integer"))?,
            );
        }
    }
    sse_from_seq(from, headers)
}

async fn sse_events(
    State(state): State<ApiState>,
    Path(session): Path<String>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let session = SessionId::from(session);
    if let Err(error) = require_session(&session) {
        return response_error(None, error);
    }
    let from_seq = match sse_from_query(&uri, &headers) {
        Ok(from_seq) => from_seq,
        Err(error) => return response_error(None, error),
    };

    // Subscribe before reading durable history. Events admitted in the small
    // window are either in the suffix or delivered by this receiver, never lost.
    let mut notifications = state.host.subscribe();
    let durable = match state.host.events(session.clone(), from_seq).await {
        Ok(events) => events,
        Err(error) => return response_error(None, host_error(error)),
    };
    let mut shutdown = state.socket_shutdown.subscribe();
    let stream = stream! {
        let mut next_seq = from_seq;
        for event in durable {
            let seq = event.seq;
            next_seq = next_seq.max(seq.saturating_add(1));
            let notification = SessionEventNotification {
                session_id: session.clone(),
                event,
            };
            match sse_event("session.event", Some(seq), &notification) {
                Ok(event) => yield Ok::<Event, Infallible>(event),
                Err(error) => {
                    yield Ok(sse_error_event("oversize", error));
                    return;
                }
            }
        }

        loop {
            tokio::select! {
                _ = shutdown.recv() => return,
                notification = notifications.recv() => match notification {
                    Ok(HostNotification::SessionEvent(notification)) if notification.session_id == session => {
                        // It may have already appeared in the durable suffix.
                        if notification.event.seq < next_seq {
                            continue;
                        }
                        next_seq = notification.event.seq.saturating_add(1);
                        match sse_event("session.event", Some(notification.event.seq), &notification) {
                            Ok(event) => yield Ok(event),
                            Err(error) => {
                                yield Ok(sse_error_event("oversize", error));
                                return;
                            }
                        }
                    }
                    Ok(HostNotification::SessionStatus(notification)) if notification.session_id == session => {
                        match sse_event("session.status", None, &notification) {
                            Ok(event) => yield Ok(event),
                            Err(error) => {
                                yield Ok(sse_error_event("oversize", error));
                                return;
                            }
                        }
                    }
                    Ok(HostNotification::SubagentStarted(notification))
                        if notification.parent_session_id == session || notification.child_session_id == session => {
                        match sse_event("subagent.started", None, &notification) {
                            Ok(event) => yield Ok(event),
                            Err(error) => {
                                yield Ok(sse_error_event("oversize", error));
                                return;
                            }
                        }
                    }
                    Ok(HostNotification::SubagentFinished(notification))
                        if notification.parent_session_id == session || notification.child_session_id == session => {
                        match sse_event("subagent.finished", None, &notification) {
                            Ok(event) => yield Ok(event),
                            Err(error) => {
                                yield Ok(sse_error_event("oversize", error));
                                return;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        yield Ok(sse_error_event("lag", ApiError {
                            status: StatusCode::SERVICE_UNAVAILABLE,
                            code: "NOTIFICATION_LAGGED".into(),
                            message: format!("{dropped} notifications were dropped; reconnect using Last-Event-ID"),
                        }));
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default().text("keep-alive"))
        .into_response()
}

fn sse_from_seq(query_from: Option<u64>, headers: &HeaderMap) -> Result<u64, ApiError> {
    if let Some(from) = query_from {
        require_safe_integer("from", from)?;
        return Ok(from);
    }
    let Some(last) = headers.get("last-event-id") else {
        return Ok(0);
    };
    let last = last
        .to_str()
        .map_err(|_| ApiError::bad_request("Last-Event-ID must be ASCII"))?
        .parse::<u64>()
        .map_err(|_| ApiError::bad_request("Last-Event-ID must be an unsigned integer"))?;
    require_safe_integer("Last-Event-ID", last)?;
    last.checked_add(1)
        .ok_or_else(|| ApiError::bad_request("Last-Event-ID exceeds the supported range"))
}

fn sse_event(value_type: &str, id: Option<u64>, value: impl Serialize) -> Result<Event, ApiError> {
    let data = serde_json::to_string(&value)
        .map_err(|_| ApiError::unavailable("host notification cannot be serialized"))?;
    if data.len() > MAX_FRAME_BYTES {
        return Err(ApiError::too_large());
    }
    let event = Event::default().event(value_type).data(data);
    Ok(match id {
        Some(id) => event.id(id.to_string()),
        None => event,
    })
}

fn sse_error_event(event: &str, error: ApiError) -> Event {
    Event::default().event(event).data(
        serde_json::to_string(&ErrorEnvelope {
            code: error.code,
            message: error.message,
        })
        .expect("error envelope is serializable"),
    )
}

async fn websocket_upgrade(
    State(state): State<ApiState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !websocket_origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    upgrade
        .max_frame_size(MAX_FRAME_BYTES)
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| websocket(socket, state))
}

fn websocket_origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let (Some(origin), Some(host)) = (
        origin
            .to_str()
            .ok()
            .and_then(|value| value.parse::<Uri>().ok()),
        headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok()),
    ) else {
        return false;
    };
    origin.scheme_str() == Some("http")
        && origin
            .authority()
            .is_some_and(|authority| authority.as_str() == host)
}

type HostCall =
    Pin<Box<dyn Future<Output = (String, Result<Value, ApiError>, Option<SessionId>)> + Send>>;

async fn websocket(socket: WebSocket, state: ApiState) {
    let (mut writer, mut reader) = socket.split();
    let (outgoing, mut queued) = mpsc::channel::<WsMessage>(MAX_SOCKET_QUEUE);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            if writer.send(message).await.is_err() {
                break;
            }
        }
    });
    let mut notifications = state.host.subscribe();
    let mut shutdown = state.socket_shutdown.subscribe();
    let mut calls = FuturesUnordered::<HostCall>::new();
    let mut cancel_on_disconnect = BTreeMap::<SessionId, usize>::new();

    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            frame = reader.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    if text.len() > MAX_FRAME_BYTES {
                        let _ = queue_ws_error(&outgoing, None, ApiError::too_large());
                        break;
                    }
                    let envelope: WsRequestEnvelope = match serde_json::from_str(&text) {
                        Ok(envelope) => envelope,
                        Err(error) => {
                            if !queue_ws_error(&outgoing, None, ApiError::bad_request(error.to_string())) { break; }
                            continue;
                        }
                    };
                    if let Err(error) = validate_request_id(&envelope.request_id) {
                        if !queue_ws_error(&outgoing, None, error) { break; }
                        continue;
                    }
                    if calls.len() == MAX_SOCKET_QUEUE {
                        if !queue_ws_error(&outgoing, Some(envelope.request_id), ApiError {
                            status: StatusCode::TOO_MANY_REQUESTS,
                            code: "TOO_MANY_REQUESTS".into(),
                            message: "too many in-flight requests".into(),
                        }) { break; }
                        continue;
                    }
                    let cancellation = prompt_session(&envelope.namespace, &envelope.method, &envelope.args);
                    if let Some(session) = &cancellation {
                        *cancel_on_disconnect.entry(session.clone()).or_default() += 1;
                    }
                    let host = state.host.clone();
                    calls.push(Box::pin(async move {
                        let request_id = envelope.request_id;
                        let output = dispatch(host, &envelope.namespace, &envelope.method, envelope.args).await;
                        (request_id, output, cancellation)
                    }));
                }
                Some(Ok(WsMessage::Binary(_))) => {
                    let _ = queue_ws_error(&outgoing, None, ApiError::bad_request("binary WebSocket frames are not accepted"));
                    break;
                }
                Some(Ok(WsMessage::Ping(payload))) => {
                    if outgoing.try_send(WsMessage::Pong(payload)).is_err() { break; }
                }
                Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(WsMessage::Pong(_))) => {}
            },
            notification = notifications.recv() => match notification {
                Ok(notification) => {
                    let message = match ws_notification(&notification) {
                        Ok(message) => message,
                        Err(_) => break,
                    };
                    if outgoing.try_send(message).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    let _ = queue_ws_error(&outgoing, None, ApiError {
                        status: StatusCode::SERVICE_UNAVAILABLE,
                        code: "NOTIFICATION_LAGGED".into(),
                        message: format!("{dropped} notifications were dropped; reconnect"),
                    });
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            completed = calls.next(), if !calls.is_empty() => {
                if let Some((request_id, result, cancellation)) = completed {
                    if let Some(session) = cancellation {
                        if let Some(count) = cancel_on_disconnect.get_mut(&session) {
                            *count -= 1;
                            if *count == 0 {
                                cancel_on_disconnect.remove(&session);
                            }
                        }
                    }
                    let envelope = match result {
                        Ok(output) => ResponseEnvelope { request_id: Some(request_id), ok: true, output: Some(output), error: None },
                        Err(error) => ResponseEnvelope {
                            request_id: Some(request_id), ok: false, output: None,
                            error: Some(ErrorEnvelope { code: error.code, message: error.message }),
                        },
                    };
                    if !queue_ws_response(&outgoing, envelope) { break; }
                }
            },
        }
    }

    // Prompt is durable admission; cancellation only affects the active agent
    // and is intentionally not retried, so it cannot duplicate durable facts.
    for (session, _) in cancel_on_disconnect {
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            state.host.cancel(session, AgentCancelCause::User),
        )
        .await;
    }
    drop(outgoing);
    writer_task.abort();
    let _ = writer_task.await;
}

fn prompt_session(namespace: &str, method: &str, args: &Value) -> Option<SessionId> {
    if (namespace, method) != ("session", "prompt") {
        return None;
    }
    let args = serde_json::from_value::<PromptArgs>(args.clone()).ok()?;
    let params = SessionPromptParams {
        session_id: args.session_id,
        content_blocks: args.content_blocks,
    };
    require_session(&params.session_id).ok()?;
    params.validate().ok()?;
    Some(params.session_id)
}

fn ws_notification(notification: &HostNotification) -> Result<WsMessage, ApiError> {
    let notification = match notification {
        HostNotification::SessionEvent(value) => json!({"kind": "session-event", "payload": value}),
        HostNotification::SessionStatus(value) => {
            json!({"kind": "session-status", "payload": value})
        }
        HostNotification::SubagentStarted(value) => {
            json!({"kind": "subagent-started", "payload": value})
        }
        HostNotification::SubagentFinished(value) => {
            json!({"kind": "subagent-finished", "payload": value})
        }
    };
    ws_json(json!({"type": "notification", "notification": notification}))
}

fn queue_ws_response(outgoing: &mpsc::Sender<WsMessage>, envelope: ResponseEnvelope) -> bool {
    match ws_json(envelope) {
        Ok(message) => outgoing.try_send(message).is_ok(),
        Err(error) => queue_ws_error(outgoing, None, error),
    }
}

fn queue_ws_error(
    outgoing: &mpsc::Sender<WsMessage>,
    request_id: Option<String>,
    error: ApiError,
) -> bool {
    queue_ws_response(
        outgoing,
        ResponseEnvelope {
            request_id,
            ok: false,
            output: None,
            error: Some(ErrorEnvelope {
                code: error.code,
                message: error.message,
            }),
        },
    )
}

fn ws_json(value: impl Serialize) -> Result<WsMessage, ApiError> {
    let payload = serde_json::to_string(&value)
        .map_err(|_| ApiError::unavailable("outgoing frame cannot be serialized"))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ApiError::too_large());
    }
    Ok(WsMessage::Text(payload.into()))
}

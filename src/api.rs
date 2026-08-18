//! HTTP, SSE, and WebSocket transport over the shared host contract.
//!
//! The transport deliberately owns no durable state: `HostApi` remains the only
//! authority for admission, event replay, and cancellation.

use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    fs,
    future::Future,
    io,
    net::{Ipv4Addr, SocketAddr},
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
    http::{header, HeaderMap, HeaderValue, StatusCode, Uri},
    middleware::{self, Next},
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
    approval::{ApprovalId, ApprovalOutcome, ApprovalRequested, RpcReceipt},
    credentials::{CredentialRef, CredentialSource},
    frontend::FrontendStatic,
    host::{HostApi, HostDescriptor, HostNotification},
    protocol::{
        AgentCancelCause, InitializeParams, SessionEventNotification, SessionId,
        SessionPromptParams, MAX_SAFE_INTEGER,
    },
    settings::{SettingsDescriptor, SettingsError, SettingsPathOp},
    workspace::{WorkspaceError, WorkspaceId},
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
        let app = router_with_shutdown(
            host,
            config.frontend,
            socket_shutdown.clone(),
            Some(address),
        );
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

/// Builds the in-memory API router test seam. `frontend` is intentionally
/// accepted here so a frontend profile can compose the same host object without
/// giving static routes any host privileges; network listeners add authority pinning.
pub fn router(host: Arc<dyn HostApi>, frontend: Option<FrontendStatic>) -> Router {
    let (socket_shutdown, _) = broadcast::channel(1);
    router_with_shutdown(host, frontend, socket_shutdown, None)
}

fn router_with_shutdown(
    host: Arc<dyn HostApi>,
    frontend: Option<FrontendStatic>,
    socket_shutdown: broadcast::Sender<()>,
    bound_addr: Option<SocketAddr>,
) -> Router {
    let compat = Arc::new(CompatibilityState::new(host.descriptor()));
    let state = ApiState {
        host,
        socket_shutdown,
        frontend,
        compat,
        workspace_mutation: Arc::new(AsyncMutex::new(())),
        bound_addr,
    };
    let authority_guard = state.bound_addr.map(AuthorityGuard::new);
    let router = Router::new()
        // Static routes are registered before the catch-all unary route.
        .route("/events/{session}", get(sse_events))
        .route("/ws", get(websocket_upgrade))
        .route("/api/events.mux", get(compat_events_mux))
        .route("/api/events.host", get(compat_events_host))
        .route(
            "/api/respond",
            post(compat_approval_response).fallback(method_not_allowed),
        )
        .route(
            "/api/{method}",
            post(compat_unary).fallback(method_not_allowed),
        )
        .route(
            "/api/{namespace}/{method}",
            post(unary).fallback(method_not_allowed),
        )
        .fallback(frontend_fallback)
        .with_state(state);
    if let Some(authority_guard) = authority_guard {
        router.layer(middleware::from_fn(move |request, next| {
            require_bound_authority(authority_guard.clone(), request, next)
        }))
    } else {
        router
    }
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
    workspace_mutation: Arc<AsyncMutex<()>>,
    bound_addr: Option<SocketAddr>,
}

#[derive(Clone)]
struct AuthorityGuard {
    host: HeaderValue,
    origin: HeaderValue,
}

impl AuthorityGuard {
    fn new(address: SocketAddr) -> Self {
        let authority = address.to_string();
        Self {
            host: HeaderValue::from_str(&authority)
                .expect("socket address is a valid HTTP authority"),
            origin: HeaderValue::from_str(&format!("http://{authority}"))
                .expect("loopback origin is a valid HTTP header"),
        }
    }

    fn allows(&self, headers: &HeaderMap) -> bool {
        let mut hosts = headers.get_all(header::HOST).iter();
        if !matches!((hosts.next(), hosts.next()), (Some(host), None) if host == self.host) {
            return false;
        }
        let mut origins = headers.get_all(header::ORIGIN).iter();
        match (origins.next(), origins.next()) {
            (None, None) => true,
            (Some(origin), None) => origin == self.origin,
            _ => false,
        }
    }
}

async fn require_bound_authority(
    authority: AuthorityGuard,
    request: Request,
    next: Next,
) -> Response {
    if !authority.allows(request.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    response
        .headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
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
    // Presentation-only state. Durable session/workspace authority lives in Host.
    sessions: BTreeMap<SessionId, CompatSession>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompatSession {
    session_id: SessionId,
    workspace_id: Option<WorkspaceId>,
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
    rpc_id: Option<String>,
    payload: Value,
}

impl CompatibilityState {
    fn new(descriptor: HostDescriptor) -> Self {
        let (frames, _) = broadcast::channel(MAX_COMPAT_FRAME_QUEUE);
        Self {
            cwd: descriptor.cwd,
            provider: descriptor.provider,
            model: descriptor.model,
            max_tokens: descriptor.max_tokens,
            data: Mutex::new(CompatibilityData {
                sessions: BTreeMap::new(),
            }),
            initialized: AsyncMutex::new(false),
            frames,
        }
    }
}

fn compat_updated_at() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(MAX_SAFE_INTEGER as u128) as u64
}

fn compat_data(state: &CompatibilityState) -> std::sync::MutexGuard<'_, CompatibilityData> {
    state
        .data
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn broadcast_compat(state: &CompatibilityState, stream: CompatStream, payload: Value) {
    let _ = state.frames.send(CompatFrame {
        stream,
        rpc_id: None,
        payload,
    });
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatClientResponse {
    #[serde(rename = "type")]
    response_type: String,
    rpc_id: String,
    result: CompatApprovalResult,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatApprovalResult {
    ok: bool,
    value: CompatApprovalValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatApprovalValue {
    session_id: SessionId,
    approval_id: ApprovalId,
    outcome: ApprovalOutcome,
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
struct CompatWorkspaceRename {
    workspace_id: String,
    title: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatWorkspaceDelete {
    workspace_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSessionMove {
    workspace_id: String,
    session_id: SessionId,
    before_session_id: Option<String>,
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
struct CompatSettingsUpdate {
    ns: String,
    patch: Map<String, Value>,
    expected_revision: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSettingsReplace {
    ns: String,
    section: Map<String, Value>,
    expected_revision: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSettingsMutate {
    ns: String,
    ops: Vec<CompatSettingsPathOp>,
    expected_revision: Option<u64>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum CompatSettingsPathOp {
    Set { path: Vec<String>, value: Value },
    Unset { path: Vec<String> },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatCredentialsSet {
    #[serde(rename = "ref")]
    reference: CredentialRef,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatCredentialsUnset {
    #[serde(rename = "ref")]
    reference: CredentialRef,
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

async fn compat_approval_response(State(state): State<ApiState>, request: Request) -> Response {
    if !is_json(request.headers()) || content_length_exceeds(request.headers(), MAX_FRAME_BYTES) {
        return compat_receipt(RpcReceipt::bad_response());
    }
    let body = match to_bytes(request.into_body(), MAX_FRAME_BYTES).await {
        Ok(body) => body,
        Err(_) => return compat_receipt(RpcReceipt::bad_response()),
    };
    let response: CompatClientResponse = match serde_json::from_slice(&body) {
        Ok(response) => response,
        Err(_) => return compat_receipt(RpcReceipt::bad_response()),
    };
    if response.response_type != "client-response"
        || !response.result.ok
        || validate_request_id(&response.rpc_id).is_err()
    {
        return compat_receipt(RpcReceipt::bad_response());
    }
    let receipt = state
        .host
        .approval_registry()
        .map_or_else(RpcReceipt::not_pending, |registry| {
            registry.respond(
                &response.rpc_id,
                &response.result.value.session_id,
                &response.result.value.approval_id,
                response.result.value.outcome,
            )
        });
    compat_receipt(receipt)
}

fn compat_receipt(receipt: RpcReceipt) -> Response {
    (StatusCode::OK, Json(receipt)).into_response()
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
    let _workspace_mutation = if matches!(
        method,
        "workspace.create"
            | "workspace.rename"
            | "workspace.delete"
            | "workspace.insertSessionBefore"
            | "workspace.archiveSession"
            | "session.create"
    ) {
        Some(state.workspace_mutation.lock().await)
    } else {
        None
    };
    match method {
        "host.describe" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            let attached_sessions = state
                .host
                .list_sessions()
                .await
                .map_err(compat_host_error)?
                .len();
            Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "cwd": state.compat.cwd,
                "provider": state.compat.provider,
                "model": state.compat.model,
                "attachedSessions": attached_sessions,
            }))
        }
        "settings.describe" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            compat_settings_describe(state)
        }
        "settings.update" => compat_settings_update(state, compat_decode(payload)?).await,
        "settings.replace" => compat_settings_replace(state, compat_decode(payload)?).await,
        "settings.mutate" => compat_settings_mutate(state, compat_decode(payload)?).await,
        "workspace.list" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            let snapshot = compat_registry(state)?.snapshot();
            Ok(json!({
                "items": snapshot.items,
                "archivedSessionIds": snapshot.archived_session_ids,
            }))
        }
        "workspace.create" => compat_workspace_create(state, compat_decode(payload)?),
        "workspace.rename" => compat_workspace_rename(state, compat_decode(payload)?),
        "workspace.delete" => compat_workspace_delete(state, compat_decode(payload)?).await,
        "workspace.insertSessionBefore" => compat_session_move(state, compat_decode(payload)?),
        "workspace.archiveSession" => compat_archive_session(state, compat_decode(payload)?),
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
        "credentials.describe" => compat_credentials_describe(state, compat_decode(payload)?).await,
        "credentials.set" => compat_credentials_set(state, compat_decode(payload)?).await,
        "credentials.unset" => compat_credentials_unset(state, compat_decode(payload)?).await,
        _ => Err(CompatError::not_found()),
    }
}

fn compat_settings_describe(state: &ApiState) -> Result<Value, CompatError> {
    let settings = state
        .host
        .settings()
        .ok_or_else(|| CompatError::internal("settings service is unavailable"))?;
    let namespaces = settings
        .describe_all()
        .map_err(|_| CompatError::internal("settings are unavailable"))?
        .into_iter()
        .filter(|descriptor| is_exposed_settings_namespace(&descriptor.namespace))
        .map(compat_settings_view)
        .collect::<Vec<_>>();
    Ok(json!({
        "writable": settings.writable(),
        "hasDocument": false,
        "namespaces": namespaces,
    }))
}

fn compat_settings_view(descriptor: SettingsDescriptor) -> Value {
    let mut view = Map::from_iter([
        ("ns".into(), Value::String(descriptor.namespace)),
        ("schema".into(), descriptor.schema),
        ("value".into(), descriptor.resolved),
        ("applies".into(), Value::String("live".into())),
        (
            "secrets".into(),
            Value::Array(
                descriptor
                    .secret_paths
                    .iter()
                    .zip(descriptor.secret_set)
                    .map(|(path, set)| json!({"path": path, "set": set}))
                    .collect(),
            ),
        ),
        ("revision".into(), Value::from(descriptor.revision)),
    ]);
    if !descriptor.base.as_object().is_some_and(Map::is_empty) {
        view.insert("base".into(), descriptor.base);
    }
    if descriptor.user_present {
        view.insert("user".into(), descriptor.user);
    }
    Value::Object(view)
}
fn is_exposed_settings_namespace(ns: &str) -> bool {
    matches!(
        ns,
        "agent-loop"
            | "shell"
            | "locale"
            | "permission"
            | "ui-conversation"
            | "ui-theme"
            | "web-search-deepseek"
            | "ui-onboarding"
            | "agent-presets"
    ) || ns
        .strip_prefix("llm-")
        .is_some_and(|provider| !provider.is_empty())
}
fn compat_require_ns(ns: &str) -> Result<(), CompatError> {
    if !is_exposed_settings_namespace(ns) {
        Err(CompatError {
            code: "settings-not-exposed".into(),
            message: "settings namespace is not exposed".into(),
            details: json!({"ns": ns}),
        })
    } else {
        Ok(())
    }
}

fn compat_settings_error(ns: &str, error: SettingsError) -> CompatError {
    match error {
        SettingsError::Conflict { expected, actual } => CompatError {
            code: "settings-conflict".into(),
            message: "settings revision conflict".into(),
            details: json!({"ns": ns, "expected": expected, "actual": actual}),
        },
        _ => CompatError {
            code: "settings-rejected".into(),
            message: "settings change was rejected".into(),
            details: json!({"ns": ns}),
        },
    }
}

async fn compat_settings_update(
    state: &ApiState,
    args: CompatSettingsUpdate,
) -> Result<Value, CompatError> {
    compat_require_ns(&args.ns)?;
    let ns = args.ns;
    let Some(settings) = state.host.settings() else {
        return Err(compat_settings_error(&ns, SettingsError::Closed));
    };
    settings
        .update(&ns, Value::Object(args.patch), args.expected_revision)
        .await
        .map_err(|error| compat_settings_error(&ns, error))?;
    settings
        .describe(&ns)
        .map(compat_settings_view)
        .map_err(|error| compat_settings_error(&ns, error))
}

async fn compat_settings_replace(
    state: &ApiState,
    args: CompatSettingsReplace,
) -> Result<Value, CompatError> {
    compat_require_ns(&args.ns)?;
    let ns = args.ns;
    let Some(settings) = state.host.settings() else {
        return Err(compat_settings_error(&ns, SettingsError::Closed));
    };
    settings
        .replace(&ns, Value::Object(args.section), args.expected_revision)
        .await
        .map_err(|error| compat_settings_error(&ns, error))?;
    settings
        .describe(&ns)
        .map(compat_settings_view)
        .map_err(|error| compat_settings_error(&ns, error))
}

async fn compat_settings_mutate(
    state: &ApiState,
    args: CompatSettingsMutate,
) -> Result<Value, CompatError> {
    compat_require_ns(&args.ns)?;
    let ns = args.ns;
    let ops = args
        .ops
        .into_iter()
        .map(|op| match op {
            CompatSettingsPathOp::Set { path, value } => SettingsPathOp::Set { path, value },
            CompatSettingsPathOp::Unset { path } => SettingsPathOp::Unset { path },
        })
        .collect();
    let Some(settings) = state.host.settings() else {
        return Err(compat_settings_error(&ns, SettingsError::Closed));
    };
    settings
        .mutate(&ns, ops, args.expected_revision)
        .await
        .map_err(|error| compat_settings_error(&ns, error))?;
    settings
        .describe(&ns)
        .map(compat_settings_view)
        .map_err(|error| compat_settings_error(&ns, error))
}

fn compat_credential_error(reference: &CredentialRef) -> CompatError {
    CompatError {
        code: "credential-rejected".into(),
        message: "credential change was rejected".into(),
        details: json!({"ref": reference.as_str()}),
    }
}

async fn compat_credentials_describe(
    state: &ApiState,
    args: CompatCredentialsDescribe,
) -> Result<Value, CompatError> {
    if args.refs.len() > 64 {
        return Err(CompatError::invalid("refs must contain at most 64 entries"));
    }
    let references = args
        .refs
        .into_iter()
        .map(CredentialRef::new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| CompatError::invalid(error.to_string()))?;
    let credentials = state
        .host
        .credentials()
        .ok_or_else(|| CompatError::internal("credentials service is unavailable"))?;
    let mut described = Map::new();
    for reference in references {
        let descriptor = credentials
            .describe(&reference)
            .await
            .map_err(|_| compat_credential_error(&reference))?;
        let mut view = Map::from_iter([
            ("configured".into(), Value::Bool(descriptor.configured)),
            ("writable".into(), Value::Bool(descriptor.writable)),
        ]);
        if let Some(source) = descriptor.source {
            let source = match source {
                CredentialSource::Environment => "env",
                CredentialSource::File => "file",
            };
            view.insert("source".into(), Value::String(source.into()));
        }
        described.insert(reference.as_str().to_owned(), Value::Object(view));
    }
    Ok(json!({"credentials": described}))
}

async fn compat_credentials_set(
    state: &ApiState,
    args: CompatCredentialsSet,
) -> Result<Value, CompatError> {
    let Some(credentials) = state.host.credentials() else {
        return Err(compat_credential_error(&args.reference));
    };
    credentials
        .set(args.reference.clone(), args.value)
        .await
        .map_err(|_| compat_credential_error(&args.reference))?;
    Ok(json!({}))
}

async fn compat_credentials_unset(
    state: &ApiState,
    args: CompatCredentialsUnset,
) -> Result<Value, CompatError> {
    let Some(credentials) = state.host.credentials() else {
        return Err(compat_credential_error(&args.reference));
    };
    credentials
        .unset(&args.reference)
        .await
        .map_err(|_| compat_credential_error(&args.reference))?;
    Ok(json!({}))
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

fn compat_workspace_invalid_path(path: &str) -> CompatError {
    CompatError {
        code: "workspace-invalid-path".into(),
        message: "workspace path is invalid".into(),
        details: json!({"path": path}),
    }
}

fn compat_canonical_directory(path: &str) -> Result<String, CompatError> {
    if path.trim().is_empty() {
        return Err(compat_workspace_invalid_path(path));
    }
    let canonical = fs::canonicalize(path).map_err(|_| compat_workspace_invalid_path(path))?;
    if !canonical.is_dir() || fs::read_dir(&canonical).is_err() {
        return Err(compat_workspace_invalid_path(path));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn compat_registry(state: &ApiState) -> Result<crate::workspace::WorkspaceRegistry, CompatError> {
    state
        .host
        .workspace_registry()
        .ok_or_else(|| CompatError::internal("workspace registry is unavailable"))
}

fn compat_workspace_not_found(workspace_id: impl Into<WorkspaceId>) -> CompatError {
    let workspace_id = workspace_id.into();
    CompatError {
        code: "workspace-not-found".into(),
        message: "workspace was not found".into(),
        details: json!({"workspaceId": workspace_id}),
    }
}

fn compat_workspace_name_conflict(name: &str) -> CompatError {
    CompatError {
        code: "workspace-name-conflict".into(),
        message: "workspace name is already used".into(),
        details: json!({"name": name}),
    }
}

fn compat_workspace_error(error: WorkspaceError) -> CompatError {
    match error {
        WorkspaceError::InvalidPath(path) => compat_workspace_invalid_path(&path),
        WorkspaceError::TitleConflict => compat_workspace_name_conflict(""),
        WorkspaceError::InvalidTitle => CompatError::invalid("title must not be blank"),
        WorkspaceError::NotFound(workspace_id) => compat_workspace_not_found(workspace_id),
        WorkspaceError::InvalidPosition => CompatError {
            code: "workspace-move-invalid".into(),
            message: "workspace move is invalid".into(),
            details: json!({}),
        },
        WorkspaceError::UnknownSession(session_id)
        | WorkspaceError::UnaccountedSession(session_id) => CompatError {
            code: "session-not-found".into(),
            message: "session was not found".into(),
            details: json!({"sessionId": session_id}),
        },
        WorkspaceError::SessionBelongsElsewhere(session_id) => CompatError {
            code: "session-conflict".into(),
            message: "session belongs to another workspace".into(),
            details: json!({"sessionId": session_id}),
        },
        WorkspaceError::PathConflict => CompatError {
            code: "workspace-name-conflict".into(),
            message: "workspace name is already used".into(),
            details: json!({}),
        },
        other => CompatError::internal(format!("workspace operation failed: {}", other.code())),
    }
}

fn compat_session_move_error(
    error: WorkspaceError,
    workspace_id: &str,
    session_id: &SessionId,
    before_session_id: Option<&str>,
) -> CompatError {
    if let WorkspaceError::NotFound(workspace) = &error {
        return compat_workspace_not_found(workspace.clone());
    }
    if matches!(
        &error,
        WorkspaceError::InvalidPosition
            | WorkspaceError::UnknownSession(_)
            | WorkspaceError::UnaccountedSession(_)
            | WorkspaceError::SessionBelongsElsewhere(_)
    ) {
        let mut details = Map::from_iter([
            ("workspaceId".into(), Value::String(workspace_id.to_owned())),
            (
                "sessionId".into(),
                serde_json::to_value(session_id).expect("session id serializes"),
            ),
        ]);
        if let Some(before_session_id) = before_session_id {
            details.insert(
                "beforeSessionId".into(),
                Value::String(before_session_id.to_owned()),
            );
        }
        return CompatError {
            code: "workspace-move-invalid".into(),
            message: "workspace move is invalid".into(),
            details: Value::Object(details),
        };
    }
    compat_workspace_error(error)
}

fn compat_workspace_host_error(error: TessivumError, workspace_id: &str) -> CompatError {
    if error.code == "WORKSPACE_NOT_FOUND" {
        compat_workspace_not_found(WorkspaceId::from(workspace_id))
    } else {
        compat_host_error(error)
    }
}

fn compat_workspace_create(
    state: &ApiState,
    args: CompatWorkspaceCreate,
) -> Result<Value, CompatError> {
    let path = compat_canonical_directory(&args.path)?;
    let registry = compat_registry(state)?;
    let result = registry.create(path, None).map_err(|error| match error {
        WorkspaceError::InvalidPath(_) => compat_workspace_invalid_path(&args.path),
        error => compat_workspace_error(error),
    })?;
    if result.created {
        broadcast_compat(
            &state.compat,
            CompatStream::Host,
            json!({"type": "host/workspace-changed", "workspace": result.workspace.clone()}),
        );
    }
    Ok(json!({"workspace": result.workspace, "created": result.created}))
}

fn compat_workspace_rename(
    state: &ApiState,
    args: CompatWorkspaceRename,
) -> Result<Value, CompatError> {
    compat_require_nonblank("workspaceId", &args.workspace_id)?;
    compat_require_nonblank("title", &args.title)?;
    let title = args.title.trim().to_owned();
    let registry = compat_registry(state)?;
    let before = registry.snapshot();
    let workspace = registry
        .rename(&args.workspace_id, title.clone(), None)
        .map_err(|error| match error {
            WorkspaceError::TitleConflict => compat_workspace_name_conflict(&title),
            WorkspaceError::InvalidTitle => CompatError::invalid("title must not be blank"),
            error => compat_workspace_error(error),
        })?;
    let changed = before
        .items
        .iter()
        .find(|item| item.workspace_id == workspace.workspace_id)
        != Some(&workspace);
    if changed {
        broadcast_compat(
            &state.compat,
            CompatStream::Host,
            json!({"type": "host/workspace-changed", "workspace": workspace.clone()}),
        );
    }
    Ok(json!({"workspace": workspace}))
}

async fn compat_workspace_delete(
    state: &ApiState,
    args: CompatWorkspaceDelete,
) -> Result<Value, CompatError> {
    compat_require_nonblank("workspaceId", &args.workspace_id)?;
    let deleted = state
        .host
        .delete_workspace(WorkspaceId::from(args.workspace_id.clone()))
        .await
        .map_err(|error| compat_workspace_host_error(error, &args.workspace_id))?;
    if deleted {
        broadcast_compat(
            &state.compat,
            CompatStream::Host,
            json!({"type": "host/workspace-removed", "workspaceId": args.workspace_id.clone()}),
        );
    }
    Ok(json!({"deleted": true}))
}

fn compat_session_move(state: &ApiState, args: CompatSessionMove) -> Result<Value, CompatError> {
    compat_require_nonblank("workspaceId", &args.workspace_id)?;
    compat_require_session(&args.session_id)?;
    let registry = compat_registry(state)?;
    let before = registry.snapshot();
    registry
        .insert_session_before(
            &args.workspace_id,
            &args.session_id,
            args.before_session_id.as_deref(),
            None,
        )
        .map_err(|error| {
            compat_session_move_error(
                error,
                &args.workspace_id,
                &args.session_id,
                args.before_session_id.as_deref(),
            )
        })?;
    let after = registry.snapshot();
    let workspace = after
        .items
        .iter()
        .find(|workspace| workspace.workspace_id.as_str() == args.workspace_id)
        .cloned()
        .ok_or_else(|| {
            compat_workspace_error(WorkspaceError::NotFound(WorkspaceId::from(
                args.workspace_id,
            )))
        })?;
    if before != after {
        broadcast_compat(
            &state.compat,
            CompatStream::Host,
            json!({"type": "host/workspace-changed", "workspace": workspace.clone()}),
        );
    }
    Ok(json!({"workspace": workspace}))
}

fn compat_archive_session(state: &ApiState, args: CompatSessionRef) -> Result<Value, CompatError> {
    compat_require_session(&args.session_id)?;
    let registry = compat_registry(state)?;
    let before = registry.snapshot();
    registry
        .archive_session(&args.session_id, None)
        .map_err(compat_workspace_error)?;
    let after = registry.snapshot();
    if before.archived_session_ids != after.archived_session_ids {
        broadcast_compat(
            &state.compat,
            CompatStream::Host,
            json!({
                "type": "host/archived-sessions-changed",
                "archivedSessionIds": after.archived_session_ids.clone(),
            }),
        );
    }
    Ok(json!({"archivedSessionIds": after.archived_session_ids}))
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
    let live_ids: BTreeSet<_> = sessions
        .iter()
        .map(|session| session.session_id.clone())
        .collect();
    data.sessions.retain(|id, _| live_ids.contains(id));
    for session in sessions {
        let entry = data
            .sessions
            .entry(session.session_id.clone())
            .or_insert_with(|| CompatSession {
                session_id: session.session_id.clone(),
                workspace_id: session.workspace_id.clone(),
                updated_at: session.created_at.min(MAX_SAFE_INTEGER),
                running: false,
                blank: session.event_count == 0,
                cwd: session.cwd.clone(),
            });
        entry.workspace_id = session.workspace_id;
        entry.updated_at = entry
            .updated_at
            .max(session.created_at.min(MAX_SAFE_INTEGER));
        entry.blank = session.event_count == 0;
        entry.cwd = session.cwd;
    }
    Ok(())
}

fn compat_session_conflict(
    session_id: &SessionId,
    requested_cwd: Option<&str>,
    existing_cwd: Option<&str>,
) -> CompatError {
    let mut details = Map::from_iter([(
        "sessionId".into(),
        serde_json::to_value(session_id).expect("session id serializes"),
    )]);
    details.insert(
        "requestedCwd".into(),
        requested_cwd.map_or(Value::Null, |cwd| Value::String(cwd.to_owned())),
    );
    if let Some(cwd) = existing_cwd {
        details.insert("existingCwd".into(), Value::String(cwd.to_owned()));
    }
    CompatError {
        code: "session-conflict".into(),
        message: "session conflicts with an existing durable session".into(),
        details: Value::Object(details),
    }
}

fn compat_session_host_error(
    error: TessivumError,
    session_id: &SessionId,
    workspace_id: &WorkspaceId,
    requested_cwd: Option<&str>,
    existing_cwd: Option<&str>,
) -> CompatError {
    match error.code.as_str() {
        "SESSION_CONFLICT" => compat_session_conflict(session_id, requested_cwd, existing_cwd),
        "WORKSPACE_ATTACH_FAILED" => CompatError {
            code: "workspace-attach-failed".into(),
            message: "session workspace attachment failed".into(),
            details: json!({"sessionId": session_id, "workspaceId": workspace_id}),
        },
        "STALE_WORKSPACE_LEASE" | "WORKSPACE_REGISTRY_LOCKED" | "WORKSPACE_NOT_FOUND" => {
            compat_workspace_not_found(workspace_id.clone())
        }
        "INVALID_WORKSPACE_PATH" => compat_workspace_invalid_path(requested_cwd.unwrap_or("")),
        _ => compat_host_error(error),
    }
}

async fn compat_session_create(
    state: &ApiState,
    args: CompatSessionCreate,
) -> Result<Value, CompatError> {
    if args.cwd.is_some() {
        return Err(CompatError::invalid(
            "session.create requires workspaceId; register paths with workspace.create",
        ));
    }
    if let Some(session_id) = &args.session_id {
        compat_require_session(session_id)?;
    }
    compat_initialize(state).await?;
    let session_id = args.session_id.unwrap_or_else(SessionId::random);
    let existing = state
        .host
        .list_sessions()
        .await
        .map_err(compat_host_error)?
        .into_iter()
        .find(|session| session.session_id == session_id);

    let registry = state.host.workspace_registry();
    let Some(registry) = registry else {
        let persisted = state
            .host
            .create_session(session_id.clone())
            .await
            .map_err(compat_host_error)?;
        let session = CompatSession {
            session_id: session_id.clone(),
            workspace_id: persisted.workspace_id.clone(),
            updated_at: persisted.created_at.min(MAX_SAFE_INTEGER),
            running: false,
            blank: persisted.event_count == 0,
            cwd: persisted.cwd,
        };
        compat_data(&state.compat)
            .sessions
            .insert(session_id.clone(), session.clone());
        if existing.is_none() {
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
        }
        return Ok(json!({"sessionId": session_id}));
    };

    let requested_cwd = if let Some(workspace_id) = args.workspace_id {
        let workspace = registry
            .list()
            .into_iter()
            .find(|workspace| workspace.workspace_id.as_str() == workspace_id)
            .ok_or_else(|| {
                compat_workspace_error(WorkspaceError::NotFound(WorkspaceId::from(workspace_id)))
            })?;
        (workspace.workspace_id, workspace.path)
    } else if args.cwd.is_some() {
        unreachable!("cwd was rejected before workspace resolution")
    } else {
        let workspace_id = state
            .host
            .default_workspace_id()
            .ok_or_else(|| compat_workspace_not_found(state.compat.cwd.clone()))?;
        let workspace = registry
            .list()
            .into_iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
            .ok_or_else(|| compat_workspace_not_found(workspace_id.to_string()))?;
        (workspace.workspace_id, workspace.path)
    };

    let persisted = state
        .host
        .create_session_in(session_id.clone(), requested_cwd.0.clone())
        .await
        .map_err(|error| {
            compat_session_host_error(
                error,
                &session_id,
                &requested_cwd.0,
                Some(&requested_cwd.1),
                existing.as_ref().and_then(|session| session.cwd.as_deref()),
            )
        })?;
    let session = CompatSession {
        session_id: session_id.clone(),
        workspace_id: persisted
            .workspace_id
            .clone()
            .or_else(|| Some(requested_cwd.0.clone())),
        updated_at: persisted.created_at.min(MAX_SAFE_INTEGER),
        running: false,
        blank: persisted.event_count == 0,
        cwd: persisted.cwd.or_else(|| Some(requested_cwd.1.clone())),
    };
    compat_data(&state.compat)
        .sessions
        .insert(session_id.clone(), session.clone());
    let snapshot = registry.snapshot();
    let workspace = snapshot
        .items
        .iter()
        .find(|workspace| workspace.workspace_id == requested_cwd.0)
        .cloned()
        .ok_or_else(|| CompatError::internal("workspace disappeared after session admission"))?;
    let changed = existing
        .as_ref()
        .is_none_or(|session| session.workspace_id != Some(requested_cwd.0.clone()));
    if changed {
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
                "workspaceId": requested_cwd.0,
            }),
        );
    }
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

async fn compat_events_mux(State(state): State<ApiState>, upgrade: WebSocketUpgrade) -> Response {
    compat_upgrade(state, upgrade, CompatStream::Mux)
}

async fn compat_events_host(State(state): State<ApiState>, upgrade: WebSocketUpgrade) -> Response {
    compat_upgrade(state, upgrade, CompatStream::Host)
}

fn compat_upgrade(
    state: ApiState,
    upgrade: WebSocketUpgrade,
    stream_kind: CompatStream,
) -> Response {
    upgrade
        .max_frame_size(MAX_FRAME_BYTES)
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| compat_websocket(socket, state, stream_kind))
}

async fn compat_websocket(mut socket: WebSocket, state: ApiState, stream_kind: CompatStream) {
    let mut frames = state.compat.frames.subscribe();
    let mut notifications = state.host.subscribe();
    let mut shutdown = state.socket_shutdown.subscribe();
    let mut replayed_approvals = BTreeSet::new();
    if stream_kind == CompatStream::Mux {
        if let Some(registry) = state.host.approval_registry() {
            for requested in registry.snapshots() {
                if !replayed_approvals.insert(requested.rpc_id.clone()) {
                    continue;
                }
                let message = match compat_ws_message(
                    approval_requested_payload(&requested),
                    Some(&requested.rpc_id),
                ) {
                    Ok(message) => message,
                    Err(_) => return,
                };
                if !compat_socket_send(&mut socket, message).await {
                    return;
                }
            }
        }
    }
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
                    let message = match compat_ws_message(frame.payload, frame.rpc_id.as_deref()) {
                        Ok(message) => message,
                        Err(message) => compat_ws_message(compat_stream_error_payload(message), None)
                            .expect("stream error frame is bounded"),
                    };
                    if !compat_socket_send(&mut socket, message).await { return; }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    let message = compat_ws_message(compat_stream_error_payload(format!(
                        "{dropped} compatibility frames were dropped"
                    )), None).expect("stream error frame is bounded");
                    let _ = compat_socket_send(&mut socket, message).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            notification = notifications.recv() => match notification {
                Ok(notification) => {
                    match &notification {
                        HostNotification::ApprovalRequested(requested) if stream_kind == CompatStream::Mux => {
                            if !replayed_approvals.insert(requested.rpc_id.clone()) {
                                continue;
                            }
                        }
                        HostNotification::ApprovalResolved(resolved) if stream_kind == CompatStream::Mux => {
                            replayed_approvals.remove(&resolved.rpc_id);
                        }
                        _ => {}
                    }
                    if let Some(frame) = compat_notification(&state, notification) {
                        if frame.stream == stream_kind {
                            let message = match compat_ws_message(frame.payload, frame.rpc_id.as_deref()) {
                                Ok(message) => message,
                                Err(message) => compat_ws_message(compat_stream_error_payload(message), None)
                                    .expect("stream error frame is bounded"),
                            };
                            if !compat_socket_send(&mut socket, message).await { return; }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    let message = compat_ws_message(compat_stream_error_payload(format!(
                        "{dropped} host notifications were dropped"
                    )), None).expect("stream error frame is bounded");
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
            rpc_id: None,
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
                rpc_id: None,
                payload: json!({
                    "type": "host/session-status",
                    "sessionId": notification.session_id,
                    "running": notification.status == crate::protocol::SessionStatus::Running,
                }),
            })
        }
        HostNotification::ApprovalRequested(requested) => Some(CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: Some(requested.rpc_id.clone()),
            payload: approval_requested_payload(&requested),
        }),
        HostNotification::ApprovalResolved(resolved) => Some(CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "approval/resolved",
                "sessionId": resolved.session_id,
                "approvalId": resolved.approval_id,
                "outcome": resolved.outcome,
            }),
        }),
        HostNotification::SettingsChanged(event) => Some(CompatFrame {
            stream: CompatStream::Host,
            rpc_id: None,
            payload: json!({"type": "host/settings-changed", "ns": event.namespace}),
        }),
        HostNotification::CredentialsChanged(event) => Some(CompatFrame {
            stream: CompatStream::Host,
            rpc_id: None,
            payload: json!({"type": "host/credentials-changed", "ref": event.reference}),
        }),
        HostNotification::SubagentStarted(_) | HostNotification::SubagentFinished(_) => None,
    }
}

fn approval_requested_payload(requested: &ApprovalRequested) -> Value {
    let mut payload = Map::from_iter([
        ("type".into(), Value::String("approval/requested".into())),
        (
            "sessionId".into(),
            serde_json::to_value(&requested.session_id).expect("session id serializes"),
        ),
        (
            "approvalId".into(),
            serde_json::to_value(&requested.approval_id).expect("approval id serializes"),
        ),
        (
            "toolName".into(),
            Value::String(requested.tool_name.clone()),
        ),
    ]);
    if let Some(call_id) = &requested.call_id {
        payload.insert(
            "callId".into(),
            serde_json::to_value(call_id).expect("call id serializes"),
        );
    }
    if let Some(reason) = &requested.reason {
        payload.insert("reason".into(), Value::String(reason.clone()));
    }
    Value::Object(payload)
}

fn compat_ws_message(payload: Value, rpc_id: Option<&str>) -> Result<WsMessage, String> {
    let method = payload
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "compatibility frame has no type".to_owned())?;
    let data = serde_json::to_string(&json!({
        "type": "server-request",
        "rpcId": rpc_id.map_or_else(|| Uuid::new_v4().to_string(), str::to_owned),
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

async fn websocket_upgrade(State(state): State<ApiState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_frame_size(MAX_FRAME_BYTES)
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| websocket(socket, state))
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
        HostNotification::ApprovalRequested(value) => {
            let mut payload = approval_requested_payload(value);
            payload
                .as_object_mut()
                .expect("approval payload is an object")
                .remove("type");
            json!({"kind": "approval-requested", "payload": payload})
        }
        HostNotification::ApprovalResolved(value) => {
            json!({"kind": "approval-resolved", "payload": {
                "sessionId": value.session_id,
                "approvalId": value.approval_id,
                "outcome": value.outcome,
            }})
        }
        HostNotification::SettingsChanged(value) => {
            json!({"kind": "settings-changed", "payload": value})
        }
        HostNotification::CredentialsChanged(value) => {
            json!({"kind": "credentials-changed", "payload": value})
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

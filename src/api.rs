//! HTTP, SSE, and WebSocket transport over the shared host contract.
//!
//! The transport deliberately owns no durable state: `HostApi` remains the only
//! authority for admission, event replay, and cancellation.

use std::{
    collections::BTreeSet,
    convert::Infallible,
    future::Future,
    io,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::Duration,
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
use serde_json::{json, Value};
use tokio::{
    net::TcpListener,
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    frontend::FrontendStatic,
    host::{HostApi, HostNotification},
    protocol::{
        AgentCancelCause, InitializeParams, SessionId, SessionPromptParams, MAX_SAFE_INTEGER,
    },
    TessivumError,
};
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
    let state = ApiState {
        host,
        socket_shutdown,
        frontend,
    };
    Router::new()
        // Static routes are registered before the catch-all unary route.
        .route("/events/{session}", get(sse_events))
        .route("/ws", get(websocket_upgrade))
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
            next_seq = next_seq.max(event.seq.saturating_add(1));
            match sse_event("session.event", Some(event.seq), &event) {
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
    let mut cancel_on_disconnect = BTreeSet::<SessionId>::new();

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
                        cancel_on_disconnect.insert(session.clone());
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
                        cancel_on_disconnect.remove(&session);
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
    for session in cancel_on_disconnect {
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            state.host.cancel(session, AgentCancelCause::User),
        )
        .await;
    }
    drop(outgoing);
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

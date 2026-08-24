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
    body::{to_bytes, Body},
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
use bytes::Bytes;
use crc32fast::hash as crc32;
use futures_util::{
    stream::{FuturesUnordered, StreamExt},
    SinkExt,
};
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use serde_json::{json, Map, Value};
use tokio::{
    net::TcpListener,
    sync::{broadcast, mpsc, oneshot, Mutex as AsyncMutex},
    task::JoinHandle,
};

#[cfg(test)]
use crate::protocol::SurfaceOp;
use crate::{
    agent_preset::composition_contains_plugin,
    approval::{ApprovalId, ApprovalOutcome, ApprovalRequested, RpcReceipt},
    attachments::{AttachmentId, AttachmentLimits, AttachmentRef},
    credentials::{CredentialRef, CredentialSource},
    frontend::FrontendStatic,
    goal::{GoalError, GoalRef, GoalService},
    host::{
        HostApi, HostDescriptor, HostModelGroup, HostModelInfo, HostNotification,
        HostProviderDirectoryEntry, HostSessionModels, HostSettingsMutation, SessionQueueAction,
        SessionUpdateQueueParams,
    },
    permissions::{
        fold as fold_permission_events, select as permission_select, PERMISSION_SETTINGS_NAMESPACE,
    },
    protocol::{
        AgentCancelCause, ContentBlock, InitializeParams, SessionEvent, SessionEventNotification,
        SessionId, SessionOrigin, SessionPromptParams, MAX_SAFE_INTEGER,
    },
    question::AskUserQuestionAnswer,
    session::SessionRawArtifact,
    session_query::SESSION_SEARCH_QUERY_MAX_CHARS,
    settings::{SettingsDescriptor, SettingsError, SettingsPathOp, LLM_PI_AI_NAMESPACE},
    skills::{FilesystemSkillProvider, SkillProvider},
    subagent::{
        SessionProjectionsBlock, SubagentDeleteRequest, SubagentHistoryRequest,
        SubagentInterruptRequest, SubagentMode, SubagentPromptRequest,
    },
    workspace::{WorkspaceError, WorkspaceId},
    TessivumError,
};
use reqwest::redirect::Policy;
use tessivum_core::ContextHandle;
use url::Url;
use uuid::Uuid;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_PROMPT_FRAME_BYTES: usize = 64 * 1024 * 1024;
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

    /// Binds the configured listener with loopback-only browser authority.
    pub async fn bind_with_config(
        host: Arc<dyn HostApi>,
        config: ApiServerConfig,
    ) -> io::Result<Self> {
        Self::bind_with_trusted_authorities(host, config, Vec::new()).await
    }

    /// Binds with an explicit list of exact non-loopback Web authorities.
    pub async fn bind_with_trusted_authorities(
        host: Arc<dyn HostApi>,
        config: ApiServerConfig,
        trusted_authorities: Vec<String>,
    ) -> io::Result<Self> {
        if !config.bind_addr.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "API listeners require a loopback bind address",
            ));
        }
        let listener = TcpListener::bind(config.bind_addr).await?;
        let address = listener.local_addr()?;
        let authority_guard = AuthorityGuard::new(address, trusted_authorities)?;
        let (socket_shutdown, _) = broadcast::channel(1);
        let (listener_shutdown, listener_stopped) = oneshot::channel();
        let app = router_with_shutdown(
            host,
            config.frontend,
            socket_shutdown.clone(),
            Some(authority_guard),
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
    authority_guard: Option<AuthorityGuard>,
) -> Router {
    let compat = Arc::new(CompatibilityState::new(host.descriptor()));
    let state = ApiState {
        host,
        socket_shutdown,
        frontend,
        compat,
        workspace_mutation: Arc::new(AsyncMutex::new(())),
    };
    let router = Router::new()
        // Static routes are registered before the catch-all unary route.
        .route("/events/{session}", get(sse_events))
        .route("/ws", get(websocket_upgrade))
        .route("/api/events.mux", get(compat_events_mux))
        .route(
            "/api/attachments",
            post(upload_attachment).fallback(method_not_allowed),
        )
        .route("/api/events.host", get(compat_events_host))
        .route(
            "/api/session.export",
            get(session_export)
                .head(session_export_head)
                .fallback(method_not_allowed),
        )
        .route(
            "/api/respond",
            post(compat_approval_response).fallback(method_not_allowed),
        )
        .route("/api/commands/list", post(compat_commands_list))
        .route("/plugins/events", get(plugin_events))
        .route("/api/commands/execute", post(compat_commands_execute))
        .route(
            "/api/dynamicCordisRunner/{method}",
            post(compat_dynamic_cordis),
        )
        .route(
            "/api/pluginInventory/{method}",
            post(compat_plugin_inventory),
        )
        .route("/api/goals/{method}", post(compat_goals))
        .route(
            "/api/messageFeedback/{method}",
            post(compat_message_feedback),
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
    if let Some(authority_guard) = authority_guard.map(Arc::new) {
        router.layer(middleware::from_fn(move |request, next| {
            require_bound_authority(Arc::clone(&authority_guard), request, next)
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
}

struct ExactAuthority {
    host: HeaderValue,
    origin: HeaderValue,
}

impl ExactAuthority {
    fn bound(address: SocketAddr) -> Self {
        let authority = address.to_string();
        Self {
            host: HeaderValue::try_from(authority.as_str())
                .expect("socket address is a valid HTTP authority"),
            origin: HeaderValue::try_from(format!("http://{authority}"))
                .expect("loopback origin is a valid HTTP header"),
        }
    }

    fn trusted(authority: &str) -> io::Result<Self> {
        let serialized = format!("http://{authority}/");
        let parsed = Url::parse(&serialized).map_err(|_| invalid_trusted_authority(authority))?;
        if authority.is_empty()
            || authority.trim() != authority
            || parsed.as_str() != serialized
            || parsed.port().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(invalid_trusted_authority(authority));
        }
        Ok(Self {
            host: HeaderValue::try_from(authority)
                .map_err(|_| invalid_trusted_authority(authority))?,
            origin: HeaderValue::try_from(serialized.trim_end_matches('/'))
                .map_err(|_| invalid_trusted_authority(authority))?,
        })
    }

    fn matches(&self, host: &HeaderValue, origin: Option<&HeaderValue>) -> bool {
        host == self.host && origin.is_none_or(|origin| origin == self.origin)
    }
}

struct AuthorityGuard {
    bound: ExactAuthority,
    trusted: Vec<ExactAuthority>,
}

impl AuthorityGuard {
    fn new(address: SocketAddr, trusted: Vec<String>) -> io::Result<Self> {
        Ok(Self {
            bound: ExactAuthority::bound(address),
            trusted: trusted
                .iter()
                .map(|authority| ExactAuthority::trusted(authority))
                .collect::<io::Result<Vec<_>>>()?,
        })
    }

    fn allows(&self, headers: &HeaderMap, path: &str) -> bool {
        let mut hosts = headers.get_all(header::HOST).iter();
        let (Some(host), None) = (hosts.next(), hosts.next()) else {
            return false;
        };
        let mut origins = headers.get_all(header::ORIGIN).iter();
        let origin = match (origins.next(), origins.next()) {
            (None, None) => None,
            (Some(origin), None) => Some(origin),
            _ => return false,
        };
        self.bound.matches(host, origin)
            || (!loopback_only_path(path)
                && self
                    .trusted
                    .iter()
                    .any(|authority| authority.matches(host, origin)))
    }
}

fn invalid_trusted_authority(authority: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("trusted Web authority must be an exact canonical host:port: {authority:?}"),
    )
}

fn loopback_only_path(path: &str) -> bool {
    let Some(method) = path.strip_prefix("/api/") else {
        return false;
    };
    if method.contains('%') {
        return true;
    }
    matches!(
        method,
        "agentPreset.read"
            | "agentPreset/read"
            | "agentPreset.copy"
            | "agentPreset/copy"
            | "agentPreset.openDocument"
            | "agentPreset/openDocument"
            | "agentPreset.remove"
            | "agentPreset/remove"
            | "host.pickDirectory"
            | "host/pickDirectory"
            | "host.openPath"
            | "host/openPath"
            | "settings.describe"
            | "settings/describe"
            | "settings.openDocument"
            | "settings/openDocument"
            | "settings.update"
            | "settings/update"
            | "settings.replace"
            | "settings/replace"
            | "settings.mutate"
            | "settings/mutate"
            | "credentials.describe"
            | "credentials/describe"
            | "credentials.set"
            | "credentials/set"
            | "credentials.unset"
            | "credentials/unset"
            | "llm.discoverModels"
            | "llm/discoverModels"
    )
}

async fn require_bound_authority(
    authority: Arc<AuthorityGuard>,
    request: Request,
    next: Next,
) -> Response {
    if !authority.allows(request.headers(), request.uri().path()) {
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
const MAX_DISCOVERY_BYTES: usize = 4 * 1024 * 1024;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_DIRECTORY_ENTRIES: usize = 1_000;

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
    tool_calls: BTreeMap<(SessionId, String), CompatToolCall>,
}

#[derive(Clone)]
struct CompatToolCall {
    name: String,
    arguments: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<SessionOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    projections: Option<CompatSessionProjections>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompatSessionProjections {
    as_of_seq: u64,
    values: BTreeMap<String, Value>,
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
                tool_calls: BTreeMap::new(),
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
    match error.code.as_str() {
        "MODEL_SELECTION_RESTART_REQUIRED" => CompatError {
            code: "model-selection-restart-required".into(),
            message: error.message,
            details: error.details,
        },
        "CANCELLED" => CompatError {
            code: "cancelled".into(),
            message: error.message,
            details: json!({}),
        },
        "DIRECTORY_PICKER_UNAVAILABLE" => CompatError {
            code: "directory-picker-unavailable".into(),
            message: error.message,
            details: error.details,
        },
        "queue-item-not-found"
        | "steer-unavailable"
        | "subagent-ownership"
        | "attachment-error" => CompatError {
            code: error.code,
            message: error.message,
            details: error.details,
        },
        _ => CompatError::internal(error.message),
    }
}

fn compat_provider_models_error(error: TessivumError) -> CompatError {
    CompatError {
        code: error.code,
        message: error.message,
        details: error.details,
    }
}
fn compat_preset_error(error: TessivumError) -> CompatError {
    if error.code == "CANCELLED" {
        return compat_host_error(error);
    }
    if error.code.starts_with("agent-preset-") {
        return CompatError {
            code: error.code,
            message: error.message,
            details: error.details,
        };
    }
    CompatError::internal(error.message)
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
    result: CompatResponseResult,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatResponseResult {
    ok: bool,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    error: Option<CompatResponseError>,
}

#[derive(Deserialize)]
struct CompatResponseError {
    code: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatApprovalValue {
    session_id: SessionId,
    approval_id: ApprovalId,
    outcome: ApprovalOutcome,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatQuestionValue {
    session_id: SessionId,
    answer: AskUserQuestionAnswer,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompatEmptyPayload {}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatAgentPresetRef {
    agent_preset: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatAgentPresetCopy {
    from: String,
    agent_preset: String,
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatAgentPresetSelect {
    session_id: SessionId,
    agent_preset: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompatOpenPath {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompatListDirectory {
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompatCreateDirectory {
    path: String,
    name: String,
}

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
struct CompatWorkspaceInsertBefore {
    workspace_id: String,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    before_workspace_id: Option<String>,
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
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
#[serde(deny_unknown_fields)]
struct CompatRemotePayload<T> {
    args: T,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatCordisInspectManifest {
    providers: Vec<CompatCordisInspectProviderManifest>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatCordisInspectProviderManifest {
    id: String,
    description: String,
    methods: Vec<CompatCordisInspectMethodManifest>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatCordisInspectMethodManifest {
    name: String,
    description: String,
    input_schema: Value,
    output_schema: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatCommandList {
    agent_id: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatCommandExecute {
    agent_id: SessionId,
    line: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompatMessageFeedbackCall<T> {
    request: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatMessageFeedbackList {
    session_id: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatMessageFeedbackPut {
    session_id: SessionId,
    message_id: String,
    rating: String,
    note: Option<String>,
    #[serde(deserialize_with = "deserialize_nullable_string")]
    if_version: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatMessageFeedbackDelete {
    session_id: SessionId,
    message_id: String,
    if_version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatGoalCreate {
    session_id: SessionId,
    objective: String,
    max_goal_rounds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatGoalEdit {
    session_id: SessionId,
    #[serde(rename = "ref")]
    reference: GoalRef,
    objective: Option<String>,
    max_goal_rounds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatGoalRef {
    session_id: SessionId,
    #[serde(rename = "ref")]
    reference: GoalRef,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatRemoteGoalCreate {
    agent_id: SessionId,
    request: CompatGoalCreateRequest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatGoalCreateRequest {
    objective: String,
    max_goal_rounds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatRemoteGoalEdit {
    agent_id: SessionId,
    #[serde(rename = "ref")]
    reference: GoalRef,
    request: CompatGoalEditRequest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatGoalEditRequest {
    objective: Option<String>,
    max_goal_rounds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatRemoteGoalRef {
    agent_id: SessionId,
    #[serde(rename = "ref")]
    reference: GoalRef,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSessionSearch {
    query: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSessionRename {
    session_id: SessionId,
    title: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSessionFork {
    session_id: SessionId,
    at_seq: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSubagentList {
    parent_session_id: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSubagentDelete {
    parent_session_id: SessionId,
    child_session_id: SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSubagentHistory {
    parent_session_id: SessionId,
    child_session_id: SessionId,
    mode: SubagentMode,
    before_seq: Option<u64>,
    max_messages: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSubagentPrompt {
    parent_session_id: SessionId,
    child_session_id: SessionId,
    mode: ContinuableMode,
    content: Vec<ContentBlock>,
    client_time_zone: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSubagentInterrupt {
    parent_session_id: SessionId,
    child_session_id: SessionId,
    mode: ContinuableMode,
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
struct CompatSessionAttachment {
    session_id: SessionId,
    attachment_id: AttachmentId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSessionSelectModel {
    session_id: SessionId,
    provider: String,
    model: String,
    reasoning_effort: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatDiscoverModels {
    settings_ns: String,
    provider: Option<String>,
    #[serde(rename = "baseURL")]
    base_url: Option<String>,
    api: Option<String>,
    api_key: Option<String>,
}

#[derive(Deserialize)]
struct CompatDiscoveryResponse {
    data: Vec<CompatDiscoveredModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompatDiscoveredModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, alias = "context_length")]
    context_window: Option<u64>,
    #[serde(default, alias = "max_output_tokens")]
    max_output_tokens: Option<u64>,
    #[serde(default, alias = "max_tokens")]
    max_tokens: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompatLlmModels {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatProviderModels {
    provider: String,
    #[serde(default = "empty_object")]
    config: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatSetProviderEnabled {
    provider: String,
    enabled: bool,
}

fn empty_object() -> Value {
    json!({})
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
    #[serde(default)]
    client_time_zone: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompatUpdateQueue {
    session_id: SessionId,
    item_id: crate::protocol::MessageId,
    action: CompatQueueAction,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum CompatQueueAction {
    Edit {
        content: Vec<crate::protocol::ContentBlock>,
    },
    Remove {},
    Steer {},
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
        media_type: Option<String>,
        data: Option<String>,
        name: Option<String>,
        attachment: Option<Value>,
    },
}

async fn upload_attachment(State(state): State<ApiState>, request: Request) -> Response {
    let headers = request.headers();
    let limits = state.host.attachment_limits();
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("");
    if !limits
        .media_types
        .iter()
        .any(|admitted| admitted.as_str().eq_ignore_ascii_case(media_type))
    {
        return response_error(
            None,
            ApiError::bad_request("Content-Type must be an admitted image media type"),
        );
    }
    let max_bytes = usize::try_from(limits.max_image_bytes).unwrap_or(usize::MAX);
    if content_length_exceeds(headers, max_bytes) {
        return response_error(None, ApiError::too_large());
    }
    let name = attachment_name(headers);
    let body = match to_bytes(request.into_body(), max_bytes).await {
        Ok(body) => body,
        Err(_) => return response_error(None, ApiError::too_large()),
    };
    match state.host.upload_attachment(body.to_vec(), name).await {
        Ok(reference) => (StatusCode::OK, Json(reference.safe_metadata())).into_response(),
        Err(error) => response_error(None, attachment_error(error)),
    }
}

fn attachment_name(headers: &HeaderMap) -> Option<String> {
    ["x-attachment-name", "x-attachment-filename", "x-filename"]
        .into_iter()
        .find_map(|key| headers.get(key))
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 128
                && !name.starts_with('.')
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        })
        .map(str::to_owned)
}

fn attachment_error(error: TessivumError) -> ApiError {
    let status = match error.code.as_str() {
        "ATTACHMENT_BYTE_LIMIT" => StatusCode::PAYLOAD_TOO_LARGE,
        "INVALID_ATTACHMENT_IMAGE"
        | "INVALID_ATTACHMENT_REFERENCE"
        | "UNSUPPORTED_ATTACHMENT_MEDIA_TYPE"
        | "ATTACHMENT_PIXEL_LIMIT"
        | "ATTACHMENT_BATCH_COUNT_LIMIT"
        | "ATTACHMENT_BATCH_BYTE_LIMIT" => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError {
        status,
        code: error.code,
        message: error.message,
    }
}

#[derive(Clone, Copy)]
enum SessionExportError {
    NotFound,
    RawUnavailable,
    Failed,
}

struct SessionExportQuery {
    session_id: SessionId,
    include_descendants: bool,
}

struct SessionExportEntry {
    path: String,
    bytes: Vec<u8>,
}

struct SessionExportMedia {
    owner: SessionId,
    reference: AttachmentRef,
}

async fn session_export(State(state): State<ApiState>, uri: Uri) -> Response {
    let query = match session_export_query(&uri) {
        Ok(query) => query,
        Err(()) => {
            return session_export_response(
                StatusCode::BAD_REQUEST,
                "missing or invalid sessionId query parameter",
            )
        }
    };
    let filename = session_log_zip_filename(&query.session_id);
    let entries = match prepare_session_export(&state, query).await {
        Ok(entries) => entries,
        Err(error) => return session_export_error(error),
    };
    let archive = match stored_zip(entries) {
        Ok(archive) => archive,
        Err(()) => return session_export_error(SessionExportError::Failed),
    };
    let mut response = Response::new(Body::from(Bytes::from(archive)));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .extend(session_export_headers(&filename));
    response
}

async fn session_export_head(State(state): State<ApiState>, uri: Uri) -> Response {
    let query = match session_export_query(&uri) {
        Ok(query) => query,
        Err(()) => {
            return session_export_response(
                StatusCode::BAD_REQUEST,
                "missing or invalid sessionId query parameter",
            )
        }
    };
    if let Err(error) = prepare_session_export_root(&state, query.session_id.clone()).await {
        return session_export_head_error(error);
    }
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .extend(session_export_headers(&session_log_zip_filename(
            &query.session_id,
        )));
    response
}

fn session_export_query(uri: &Uri) -> Result<SessionExportQuery, ()> {
    let mut session_id = None;
    let mut include_descendants = None;
    for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        match key.as_ref() {
            "sessionId" => session_id = Some(value.into_owned()),
            "includeDescendants" => include_descendants = Some(value.into_owned()),
            _ => {}
        }
    }
    let session_id = session_id.filter(|value| !value.is_empty()).ok_or(())?;
    let include_descendants = match include_descendants.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(()),
    };
    Ok(SessionExportQuery {
        session_id: SessionId::from(session_id),
        include_descendants,
    })
}

async fn prepare_session_export(
    state: &ApiState,
    query: SessionExportQuery,
) -> Result<Vec<SessionExportEntry>, SessionExportError> {
    let root = prepare_session_export_root(state, query.session_id.clone()).await?;
    let mut entries = Vec::new();
    let mut paths = BTreeSet::new();
    if root.bytes.len() > MAX_PROMPT_FRAME_BYTES {
        return Err(SessionExportError::Failed);
    }
    let mut total = 0usize;
    let mut media = Vec::new();
    let mut media_indices = BTreeMap::new();
    remember_artifact_media(
        &root.bytes,
        &query.session_id,
        &mut media,
        &mut media_indices,
    )?;
    let root_path = safe_artifact_basename(&root.filename).ok_or(SessionExportError::Failed)?;
    push_session_export_entry(
        &mut entries,
        &mut paths,
        &mut total,
        root_path.to_owned(),
        root.bytes,
    )?;

    if query.include_descendants {
        let sessions = state
            .host
            .list_sessions()
            .await
            .map_err(|_| SessionExportError::Failed)?;
        for descendant in session_export_descendants(sessions, &query.session_id)? {
            let raw = state
                .host
                .read_raw_session(descendant.clone())
                .await
                .map_err(|_| SessionExportError::Failed)?
                .ok_or(SessionExportError::Failed)?;
            if raw.bytes.len() > MAX_PROMPT_FRAME_BYTES {
                return Err(SessionExportError::Failed);
            }
            remember_artifact_media(&raw.bytes, &descendant, &mut media, &mut media_indices)?;
            let basename =
                safe_artifact_basename(&raw.filename).ok_or(SessionExportError::Failed)?;
            let path = format!(
                "subagents/{}/{basename}",
                safe_session_id_segment(descendant.as_str())
            );
            push_session_export_entry(&mut entries, &mut paths, &mut total, path, raw.bytes)?;
        }
    }

    for media in media {
        let attachment = state
            .host
            .read_attachment(media.owner, media.reference.attachment_id.clone())
            .await
            .map_err(|_| SessionExportError::Failed)?;
        let path = format!(
            "media/{}.{}",
            media.reference.attachment_id,
            media_extension(&media.reference)
        );
        push_session_export_entry(&mut entries, &mut paths, &mut total, path, attachment.data)?;
    }
    Ok(entries)
}

async fn prepare_session_export_root(
    state: &ApiState,
    session_id: SessionId,
) -> Result<SessionRawArtifact, SessionExportError> {
    match state.host.read_raw_session(session_id).await {
        Ok(Some(raw)) => Ok(raw),
        Ok(None) => Err(SessionExportError::NotFound),
        Err(error) if error.code == "SESSION_RAW_ARTIFACTS_UNSUPPORTED" => {
            Err(SessionExportError::RawUnavailable)
        }
        Err(_) => Err(SessionExportError::Failed),
    }
}

fn session_export_descendants(
    sessions: Vec<crate::host::HostSessionInfo>,
    root: &SessionId,
) -> Result<Vec<SessionId>, SessionExportError> {
    if sessions.len() > MAX_DIRECTORY_ENTRIES {
        return Err(SessionExportError::Failed);
    }
    let mut children = BTreeMap::<SessionId, Vec<(u64, SessionId)>>::new();
    for session in sessions {
        if let Some(parent) = session.parent_session {
            children
                .entry(parent)
                .or_default()
                .push((session.created_at, session.session_id));
        }
    }
    for entries in children.values_mut() {
        entries.sort();
    }
    let mut seen = BTreeSet::from([root.clone()]);
    let mut pending = children.get(root).cloned().unwrap_or_default();
    pending.reverse();
    let mut descendants = Vec::new();
    while let Some((_, session_id)) = pending.pop() {
        if !seen.insert(session_id.clone()) || descendants.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(SessionExportError::Failed);
        }
        descendants.push(session_id.clone());
        if let Some(children) = children.get(&session_id) {
            pending.extend(children.iter().rev().cloned());
        }
    }
    Ok(descendants)
}

fn remember_artifact_media(
    bytes: &[u8],
    owner: &SessionId,
    media: &mut Vec<SessionExportMedia>,
    indices: &mut BTreeMap<String, usize>,
) -> Result<(), SessionExportError> {
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(data) = event.get("data") else {
            continue;
        };
        remember_content_media(data.get("content"), owner, media, indices)?;
        remember_content_media(
            data.get("message")
                .and_then(|message| message.get("content")),
            owner,
            media,
            indices,
        )?;
        if let Some(inserted) = data.get("inserted").and_then(Value::as_array) {
            for message in inserted {
                remember_content_media(message.get("content"), owner, media, indices)?;
            }
        }
        if let Some(block) = data.get("chunk").and_then(|chunk| {
            (chunk.get("type").and_then(Value::as_str) == Some("block-end"))
                .then(|| chunk.get("block"))
                .flatten()
        }) {
            remember_blocks_media(vec![block], owner, media, indices)?;
        }
    }
    Ok(())
}

fn remember_content_media(
    content: Option<&Value>,
    owner: &SessionId,
    media: &mut Vec<SessionExportMedia>,
    indices: &mut BTreeMap<String, usize>,
) -> Result<(), SessionExportError> {
    let Some(content) = content.and_then(Value::as_array) else {
        return Ok(());
    };
    remember_blocks_media(content.iter().collect(), owner, media, indices)
}

fn remember_blocks_media(
    mut pending: Vec<&Value>,
    owner: &SessionId,
    media: &mut Vec<SessionExportMedia>,
    indices: &mut BTreeMap<String, usize>,
) -> Result<(), SessionExportError> {
    while let Some(block) = pending.pop() {
        let Some(object) = block.as_object() else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) == Some("image") {
            let reference = object
                .get("attachment")
                .ok_or(SessionExportError::Failed)
                .and_then(|value| {
                    AttachmentRef::from_value(value).map_err(|_| SessionExportError::Failed)
                })?;
            let id = reference.attachment_id.to_string();
            if let Some(index) = indices.get(&id).copied() {
                media[index] = SessionExportMedia {
                    owner: owner.clone(),
                    reference,
                };
            } else {
                indices.insert(id, media.len());
                media.push(SessionExportMedia {
                    owner: owner.clone(),
                    reference,
                });
            }
        }
        if let Some(nested) = object.get("content").and_then(Value::as_array) {
            pending.extend(nested);
        }
    }
    Ok(())
}

fn push_session_export_entry(
    entries: &mut Vec<SessionExportEntry>,
    paths: &mut BTreeSet<String>,
    total: &mut usize,
    path: String,
    bytes: Vec<u8>,
) -> Result<(), SessionExportError> {
    if entries.len() >= MAX_DIRECTORY_ENTRIES
        || !safe_archive_path(&path)
        || !paths.insert(path.clone())
    {
        return Err(SessionExportError::Failed);
    }
    *total = total
        .checked_add(bytes.len())
        .filter(|total| *total <= MAX_PROMPT_FRAME_BYTES)
        .ok_or(SessionExportError::Failed)?;
    entries.push(SessionExportEntry { path, bytes });
    Ok(())
}

fn safe_artifact_basename(filename: &str) -> Option<&str> {
    (!filename.is_empty()
        && !filename.contains('/')
        && !filename.contains('\\')
        && !matches!(filename, "." | "..")
        && filename.bytes().all(|byte| byte >= 0x20 && byte != 0x7f))
    .then_some(filename)
}

fn safe_archive_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && !matches!(segment, "." | "..")
                && segment.bytes().all(|byte| byte >= 0x20 && byte != 0x7f)
        })
}

fn safe_session_id_segment(session_id: &str) -> String {
    session_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn media_extension(reference: &AttachmentRef) -> &'static str {
    match reference.media_type_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => unreachable!("AttachmentRef only permits admitted image media types"),
    }
}

fn session_log_zip_filename(session_id: &SessionId) -> String {
    format!(
        "dsh-session-{}.zip",
        safe_session_id_segment(session_id.as_str())
    )
}

fn session_export_headers(filename: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_bytes(format!("attachment; filename=\"{filename}\"").as_bytes())
            .expect("session export filename is ASCII"),
    );
    headers
}

fn session_export_error(error: SessionExportError) -> Response {
    match error {
        SessionExportError::NotFound => session_export_response(StatusCode::NOT_FOUND, "session not found"),
        SessionExportError::RawUnavailable => session_export_response(
            StatusCode::NOT_IMPLEMENTED,
            "session log export is unavailable: the persistence backend does not expose per-session raw artifacts",
        ),
        SessionExportError::Failed => session_export_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "session log export failed to prepare the stored artifact",
        ),
    }
}

fn session_export_head_error(error: SessionExportError) -> Response {
    let status = match error {
        SessionExportError::NotFound => StatusCode::NOT_FOUND,
        SessionExportError::RawUnavailable => StatusCode::NOT_IMPLEMENTED,
        SessionExportError::Failed => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn session_export_response(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

fn stored_zip(entries: Vec<SessionExportEntry>) -> Result<Vec<u8>, ()> {
    let entry_count = u16::try_from(entries.len()).map_err(|_| ())?;
    let mut archive = Vec::new();
    let mut index = Vec::with_capacity(entries.len());
    for entry in entries {
        if !safe_archive_path(&entry.path) {
            return Err(());
        }
        let path = entry.path.into_bytes();
        let path_len = u16::try_from(path.len()).map_err(|_| ())?;
        let bytes_len = u32::try_from(entry.bytes.len()).map_err(|_| ())?;
        let offset = u32::try_from(archive.len()).map_err(|_| ())?;
        let checksum = crc32(&entry.bytes);
        zip_u32(&mut archive, 0x0403_4b50);
        zip_u16(&mut archive, 20);
        zip_u16(&mut archive, 0x0800);
        zip_u16(&mut archive, 0);
        zip_u16(&mut archive, 0);
        zip_u16(&mut archive, 0);
        zip_u32(&mut archive, checksum);
        zip_u32(&mut archive, bytes_len);
        zip_u32(&mut archive, bytes_len);
        zip_u16(&mut archive, path_len);
        zip_u16(&mut archive, 0);
        archive.extend_from_slice(&path);
        archive.extend_from_slice(&entry.bytes);
        index.push((path, checksum, bytes_len, offset));
        if archive.len() > MAX_PROMPT_FRAME_BYTES {
            return Err(());
        }
    }
    let central_offset = u32::try_from(archive.len()).map_err(|_| ())?;
    for (path, checksum, bytes_len, offset) in index {
        let path_len = u16::try_from(path.len()).map_err(|_| ())?;
        zip_u32(&mut archive, 0x0201_4b50);
        zip_u16(&mut archive, 0x0314);
        zip_u16(&mut archive, 20);
        zip_u16(&mut archive, 0x0800);
        zip_u16(&mut archive, 0);
        zip_u16(&mut archive, 0);
        zip_u16(&mut archive, 0);
        zip_u32(&mut archive, checksum);
        zip_u32(&mut archive, bytes_len);
        zip_u32(&mut archive, bytes_len);
        zip_u16(&mut archive, path_len);
        zip_u16(&mut archive, 0);
        zip_u16(&mut archive, 0);
        zip_u16(&mut archive, 0);
        zip_u16(&mut archive, 0);
        zip_u32(&mut archive, 0);
        zip_u32(&mut archive, offset);
        archive.extend_from_slice(&path);
        if archive.len() > MAX_PROMPT_FRAME_BYTES {
            return Err(());
        }
    }
    let central_len = u32::try_from(archive.len()).map_err(|_| ())? - central_offset;
    zip_u32(&mut archive, 0x0605_4b50);
    zip_u16(&mut archive, 0);
    zip_u16(&mut archive, 0);
    zip_u16(&mut archive, entry_count);
    zip_u16(&mut archive, entry_count);
    zip_u32(&mut archive, central_len);
    zip_u32(&mut archive, central_offset);
    zip_u16(&mut archive, 0);
    (archive.len() <= MAX_PROMPT_FRAME_BYTES)
        .then_some(archive)
        .ok_or(())
}

fn zip_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn zip_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
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
    if response.response_type != "client-response" || validate_request_id(&response.rpc_id).is_err()
    {
        return compat_receipt(RpcReceipt::bad_response());
    }
    let questions = state.host.question_registry();
    if questions
        .as_ref()
        .is_some_and(|registry| registry.is_pending(&response.rpc_id))
    {
        let Some(registry) = questions else {
            return compat_receipt(RpcReceipt::not_pending());
        };
        if !response.result.ok {
            let receipt = response
                .result
                .error
                .as_ref()
                .filter(|error| error.code == "cancelled")
                .map_or_else(RpcReceipt::bad_response, |_| {
                    registry.respond_cancelled(&response.rpc_id)
                });
            return compat_receipt(receipt);
        }
        let Some(value) = response.result.value else {
            return compat_receipt(RpcReceipt::bad_response());
        };
        let value: CompatQuestionValue = match serde_json::from_value(value) {
            Ok(value) => value,
            Err(_) => return compat_receipt(RpcReceipt::bad_response()),
        };
        return compat_receipt(registry.respond_answer(
            &response.rpc_id,
            &value.session_id,
            value.answer,
        ));
    }
    let Some(registry) = state.host.approval_registry() else {
        return compat_receipt(RpcReceipt::not_pending());
    };
    if !response.result.ok {
        return compat_receipt(RpcReceipt::bad_response());
    }
    let Some(value) = response.result.value else {
        return compat_receipt(RpcReceipt::bad_response());
    };
    let value: CompatApprovalValue = match serde_json::from_value(value) {
        Ok(value) => value,
        Err(_) => return compat_receipt(RpcReceipt::bad_response()),
    };
    compat_receipt(registry.respond(
        &response.rpc_id,
        &value.session_id,
        &value.approval_id,
        value.outcome,
    ))
}

fn compat_receipt(receipt: RpcReceipt) -> Response {
    (StatusCode::OK, Json(receipt)).into_response()
}

async fn plugin_events(State(state): State<ApiState>) -> Response {
    let events = stream! {
        if let Some(frontend) = state.frontend {
            if let Some(mut updates) = frontend.subscribe_hmr() {
                let mut previous = frontend.graph();
                yield Ok::<Event, Infallible>(Event::default().data(json!({
                    "type": "graph",
                    "graph": previous,
                }).to_string()));
                let mut poll = tokio::time::interval(Duration::from_millis(500));
                poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                poll.tick().await;
                loop {
                    tokio::select! {
                        _ = poll.tick() => {
                            let frontend = frontend.clone();
                            let _ = tokio::task::spawn_blocking(move || frontend.rebuild()).await;
                        }
                        update = updates.recv() => {
                            let next = match update {
                                Ok(update) => update.graph,
                                Err(broadcast::error::RecvError::Lagged(_)) => frontend.graph(),
                                Err(broadcast::error::RecvError::Closed) => break,
                            };
                            let previous_revs = previous.entries.iter()
                                .map(|entry| (entry.id.as_str(), entry.rev.as_str()))
                                .collect::<BTreeMap<_, _>>();
                            for entry in &next.entries {
                                if previous_revs.get(entry.id.as_str()).copied()
                                    .is_some_and(|rev| rev != entry.rev.as_str())
                                {
                                    yield Ok(Event::default().data(json!({
                                        "type": "rebuilt",
                                        "id": entry.id,
                                        "rev": entry.rev,
                                    }).to_string()));
                                }
                            }
                            previous = next;
                        }
                    }
                }
            } else {
                futures_util::future::pending::<()>().await;
            }
        } else {
            futures_util::future::pending::<()>().await;
        }
    };
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn compat_dynamic_cordis(
    State(state): State<ApiState>,
    Path(method): Path<String>,
    request: Request,
) -> Response {
    compat_unary_method(state, format!("dynamicCordisRunner/{method}"), request).await
}
async fn compat_plugin_inventory(
    State(state): State<ApiState>,
    Path(method): Path<String>,
    request: Request,
) -> Response {
    compat_unary_method(state, format!("pluginInventory/{method}"), request).await
}
async fn compat_goals(
    State(state): State<ApiState>,
    Path(method): Path<String>,
    request: Request,
) -> Response {
    compat_unary_method(state, format!("goals/{method}"), request).await
}

async fn compat_message_feedback(
    State(state): State<ApiState>,
    Path(method): Path<String>,
    request: Request,
) -> Response {
    compat_unary_method(state, format!("messageFeedback/{method}"), request).await
}

async fn compat_unary(
    State(state): State<ApiState>,
    Path(method): Path<String>,
    request: Request,
) -> Response {
    compat_unary_method(state, method, request).await
}

async fn compat_commands_list(State(state): State<ApiState>, request: Request) -> Response {
    compat_unary_method(state, "commands/list".into(), request).await
}

async fn compat_commands_execute(State(state): State<ApiState>, request: Request) -> Response {
    compat_unary_method(state, "commands/execute".into(), request).await
}

async fn compat_unary_method(state: ApiState, method: String, request: Request) -> Response {
    if !is_json(request.headers()) {
        return compat_response_error(
            Value::Null,
            CompatError::invalid("Content-Type must be application/json"),
        );
    }
    let body_limit = if method == "session.prompt" {
        prompt_body_limit(&state)
    } else {
        MAX_FRAME_BYTES
    };
    if content_length_exceeds(request.headers(), body_limit) {
        return compat_response_error(Value::Null, CompatError::too_large());
    }
    let body = match to_bytes(request.into_body(), body_limit).await {
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
    let response_limit = compat_response_limit(&state, &method);
    match compat_dispatch(&state, &method, envelope.payload).await {
        Ok(value) => compat_response_ok(rpc_id, value, response_limit),
        Err(error) => compat_response_error(rpc_id, error),
    }
}

fn compat_api_error(error: ApiError) -> CompatError {
    CompatError::internal(error.message)
}

fn compat_response_ok(rpc_id: Value, value: Value, response_limit: usize) -> Response {
    compat_response(rpc_id, json!({"ok": true, "value": value}), response_limit)
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
        MAX_FRAME_BYTES,
    )
}
fn prompt_body_limit(state: &ApiState) -> usize {
    let encoded = base64_encoded_len(state.host.attachment_limits().max_message_image_bytes);
    usize::try_from(encoded.saturating_add(MAX_FRAME_BYTES as u64))
        .unwrap_or(MAX_PROMPT_FRAME_BYTES)
        .clamp(MAX_FRAME_BYTES, MAX_PROMPT_FRAME_BYTES)
}
fn canonical_client_time_zone(value: &str) -> Option<String> {
    if value == "UTC" {
        return Some("UTC".into());
    }
    if value.is_empty()
        || value.trim() != value
        || !value.contains('/')
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'+' | b'.' | b'-' | b'/')
        })
    {
        return None;
    }
    let root = fs::canonicalize("/usr/share/zoneinfo").ok()?;
    let zone = fs::canonicalize(root.join(value)).ok()?;
    zone.strip_prefix(root)
        .ok()?
        .to_str()
        .filter(|zone| {
            zone.contains('/') && !zone.starts_with("posix/") && !zone.starts_with("right/")
        })
        .map(str::to_owned)
}

fn compat_response_limit(state: &ApiState, method: &str) -> usize {
    match method {
        "session.attachment" => {
            compat_attachment_response_limit(state.host.attachment_limits().max_image_bytes)
        }
        "session.history" | "subagent.history" => MAX_PROMPT_FRAME_BYTES,
        _ => MAX_FRAME_BYTES,
    }
}

fn compat_attachment_response_limit(max_image_bytes: u64) -> usize {
    let encoded = base64_encoded_len(max_image_bytes);
    usize::try_from(encoded.saturating_add(MAX_FRAME_BYTES as u64))
        .unwrap_or(MAX_PROMPT_FRAME_BYTES)
        .clamp(MAX_FRAME_BYTES, MAX_PROMPT_FRAME_BYTES)
}

fn base64_encoded_len(bytes: u64) -> u64 {
    bytes
        .saturating_add(2)
        .checked_div(3)
        .unwrap_or(u64::MAX)
        .saturating_mul(4)
}

fn compat_response(rpc_id: Value, result: Value, response_limit: usize) -> Response {
    let response_rpc_id = rpc_id.clone();
    let body = json!({
        "type": "server-response",
        "rpcId": rpc_id,
        "result": result,
    });
    if serde_json::to_vec(&body).is_ok_and(|body| body.len() <= response_limit) {
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
            | "workspace.insertBefore"
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
                "canOpenPath": state.host.can_open_path(),
            }))
        }
        "settings.describe" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            compat_settings_describe(state)
        }
        "settings.openDocument" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            compat_settings_open_document(state).await
        }
        "settings.update" => compat_settings_update(state, compat_decode(payload)?).await,
        "settings.replace" => compat_settings_replace(state, compat_decode(payload)?).await,
        "settings.mutate" => compat_settings_mutate(state, compat_decode(payload)?).await,
        "host.pickDirectory" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            let path = state
                .host
                .pick_directory()
                .await
                .map_err(compat_host_error)?;
            Ok(json!({"path": path}))
        }
        "host.openPath" => compat_open_path(state, compat_decode(payload)?).await,
        "host.listDirectory" => compat_list_directory(compat_decode(payload)?),
        "host.createDirectory" => compat_create_directory(compat_decode(payload)?),
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
        "workspace.insertBefore" => compat_workspace_insert_before(state, compat_decode(payload)?),
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
        "session.search" => compat_session_search(state, compat_decode(payload)?).await,
        "session.rename" => compat_session_rename(state, compat_decode(payload)?).await,
        "session.fork" => compat_session_fork(state, compat_decode(payload)?).await,
        "session.models" => {
            let args: CompatSessionModels = compat_decode(payload)?;
            let session_id = args
                .session_id
                .ok_or_else(|| CompatError::invalid("sessionId is required"))?;
            compat_require_session(&session_id)?;
            compat_session_models(state, session_id).await
        }
        "session.attachment" => compat_session_attachment(state, compat_decode(payload)?).await,
        "session.selectModel" => {
            let args: CompatSessionSelectModel = compat_decode(payload)?;
            compat_require_session(&args.session_id)?;
            compat_require_nonblank("provider", &args.provider)?;
            compat_require_nonblank("model", &args.model)?;
            if let Some(reasoning_effort) = args.reasoning_effort.as_deref() {
                compat_require_nonblank("reasoningEffort", reasoning_effort)?;
            }
            let selected = state
                .host
                .select_model(
                    args.session_id,
                    args.provider,
                    args.model,
                    args.reasoning_effort,
                )
                .await
                .map_err(compat_host_error)?;
            Ok(json!({"selected": selected}))
        }
        "session.prompt" => compat_session_prompt(state, compat_decode(payload)?).await,
        "session.updateQueue" => compat_session_update_queue(state, compat_decode(payload)?).await,
        "session.cancel" => compat_session_cancel(state, compat_decode(payload)?).await,
        "goal.create" => compat_goal_create(state, compat_decode(payload)?).await,
        "goal.edit" => compat_goal_edit(state, compat_decode(payload)?).await,
        "goal.pause" => compat_goal_pause(state, compat_decode(payload)?).await,
        "goal.resume" => compat_goal_resume(state, compat_decode(payload)?).await,
        "goal.complete" => compat_goal_complete(state, compat_decode(payload)?).await,
        "goal.clear" => compat_goal_clear(state, compat_decode(payload)?).await,
        "goals/create" => {
            let payload: CompatRemotePayload<CompatRemoteGoalCreate> = compat_decode(payload)?;
            compat_goal_create(
                state,
                CompatGoalCreate {
                    session_id: payload.args.agent_id,
                    objective: payload.args.request.objective,
                    max_goal_rounds: payload.args.request.max_goal_rounds,
                },
            )
            .await
        }
        "goals/edit" => {
            let payload: CompatRemotePayload<CompatRemoteGoalEdit> = compat_decode(payload)?;
            let session_id = payload.args.agent_id;
            compat_goal_edit(
                state,
                CompatGoalEdit {
                    session_id: session_id.clone(),
                    reference: payload.args.reference,
                    objective: payload.args.request.objective,
                    max_goal_rounds: payload.args.request.max_goal_rounds,
                },
            )
            .await?;
            compat_remote_goal_view(state, &session_id).await
        }
        "goals/pause" | "goals/resume" | "goals/complete" => {
            let action = method;
            let payload: CompatRemotePayload<CompatRemoteGoalRef> = compat_decode(payload)?;
            let session_id = payload.args.agent_id;
            let args = CompatGoalRef {
                session_id: session_id.clone(),
                reference: payload.args.reference,
            };
            match action {
                "goals/pause" => compat_goal_pause(state, args).await?,
                "goals/resume" => compat_goal_resume(state, args).await?,
                _ => compat_goal_complete(state, args).await?,
            };
            compat_remote_goal_view(state, &session_id).await
        }
        "goals/clear" => {
            let payload: CompatRemotePayload<CompatRemoteGoalRef> = compat_decode(payload)?;
            let reference = payload.args.reference;
            compat_goal_clear(
                state,
                CompatGoalRef {
                    session_id: payload.args.agent_id,
                    reference: reference.clone(),
                },
            )
            .await?;
            serde_json::to_value(reference)
                .map_err(|error| CompatError::internal(error.to_string()))
        }
        "subagent.list" => {
            let args: CompatSubagentList = compat_decode(payload)?;
            compat_require_session(&args.parent_session_id)?;
            let catalog = state
                .host
                .subagent_list(args.parent_session_id)
                .await
                .map_err(compat_host_error)?;
            serde_json::to_value(catalog).map_err(|error| CompatError::internal(error.to_string()))
        }
        "subagent.history" => compat_subagent_history(state, compat_decode(payload)?).await,
        "subagent.prompt" => compat_subagent_prompt(state, compat_decode(payload)?).await,
        "subagent.interrupt" => compat_subagent_interrupt(state, compat_decode(payload)?).await,
        "subagent.delete" => compat_subagent_delete(state, compat_decode(payload)?).await,
        "command.list" => {
            let args: CompatSessionRef = compat_decode(payload)?;
            let commands = state
                .host
                .command_list(args.session_id.clone())
                .await
                .map_err(|error| compat_session_rpc_error(error, &args.session_id))?;
            Ok(json!({"commands": commands}))
        }
        "commands/list" => {
            let payload: CompatRemotePayload<CompatCommandList> = compat_decode(payload)?;
            serde_json::to_value(
                state
                    .host
                    .command_list(payload.args.agent_id.clone())
                    .await
                    .map_err(|error| compat_session_rpc_error(error, &payload.args.agent_id))?,
            )
            .map_err(|error| CompatError::internal(error.to_string()))
        }
        "commands/execute" => {
            let payload: CompatRemotePayload<CompatCommandExecute> = compat_decode(payload)?;
            serde_json::to_value(
                state
                    .host
                    .command_execute(payload.args.agent_id.clone(), payload.args.line)
                    .await
                    .map_err(|error| compat_session_rpc_error(error, &payload.args.agent_id))?,
            )
            .map_err(|error| CompatError::internal(error.to_string()))
        }
        "messageFeedback/list" => {
            let payload: CompatRemotePayload<CompatMessageFeedbackCall<CompatMessageFeedbackList>> =
                compat_decode(payload)?;
            compat_message_feedback_list(state, payload.args.request).await
        }
        "messageFeedback/put" => {
            let payload: CompatRemotePayload<CompatMessageFeedbackCall<CompatMessageFeedbackPut>> =
                compat_decode(payload)?;
            compat_message_feedback_put(state, payload.args.request).await
        }
        "messageFeedback/delete" => {
            let payload: CompatRemotePayload<
                CompatMessageFeedbackCall<CompatMessageFeedbackDelete>,
            > = compat_decode(payload)?;
            compat_message_feedback_delete(state, payload.args.request).await
        }
        "pluginInventory/list" => {
            let _: CompatRemotePayload<CompatEmptyPayload> = compat_decode(payload)?;
            let entries = state.frontend.as_ref().map_or_else(Vec::new, |frontend| {
                frontend
                    .graph()
                    .entries
                    .into_iter()
                    .map(|entry| {
                        json!({
                            "entryId": entry.id,
                            "moduleName": entry.id,
                            "enabled": true,
                            "fiberPhase": "active",
                        })
                    })
                    .collect()
            });
            Ok(json!({"entries": entries}))
        }
        "dynamicCordisRunner/syncInspectManifest" => {
            let payload: CompatRemotePayload<CompatCordisInspectManifest> = compat_decode(payload)?;
            let args = serde_json::to_value(payload.args)
                .map_err(|error| CompatError::invalid(error.to_string()))?;
            state
                .host
                .dynamic_cordis_call("syncInspectManifest", args)
                .await
                .map_err(compat_host_error)
        }
        "dynamicCordisRunner/inventory" => {
            let _: CompatRemotePayload<CompatEmptyPayload> = compat_decode(payload)?;
            state
                .host
                .dynamic_cordis_inventory()
                .map_err(compat_host_error)
        }
        "dynamicCordisRunner/runHostHalf" => {
            let payload: CompatRemotePayload<Value> = compat_decode(payload)?;
            state
                .host
                .dynamic_cordis_run_host_half(payload.args)
                .await
                .map_err(compat_host_error)
        }
        "dynamicCordisRunner/getClientCode"
        | "dynamicCordisRunner/resolveRequestRun"
        | "dynamicCordisRunner/undefineFromPanel"
        | "dynamicCordisRunner/settleUserRun"
        | "dynamicCordisRunner/stopFromPanel"
        | "dynamicCordisRunner/resolveInspectQuery"
        | "dynamicCordisRunner/reportRenderFailure"
        | "dynamicCordisRunner/reportClientGuardFailure"
        | "dynamicCordisRunner/invoke" => {
            let payload: CompatRemotePayload<Value> = compat_decode(payload)?;
            let method = method
                .rsplit('/')
                .next()
                .expect("dynamic Cordis method has a suffix");
            state
                .host
                .dynamic_cordis_call(method, payload.args)
                .await
                .map_err(compat_host_error)
        }
        "skill.list" => {
            let args: CompatSessionRef = compat_decode(payload)?;
            let session = state
                .host
                .list_sessions()
                .await
                .map_err(compat_host_error)?
                .into_iter()
                .find(|session| session.session_id == args.session_id)
                .ok_or_else(|| CompatError {
                    code: "session-not-found".into(),
                    message: "session was not found".into(),
                    details: json!({"sessionId": args.session_id}),
                })?;
            let supports_filesystem = match session.agent_preset {
                Some(preset) => match state.host.agent_preset_read(preset).await {
                    Ok(document) => composition_contains_plugin(
                        &document.content,
                        "@deepseek-ai/dsh-skill-filesystem",
                    )
                    .unwrap_or(false),
                    Err(_) => false,
                },
                None => true,
            };
            let Some(cwd) = session.cwd.filter(|_| supports_filesystem) else {
                return Ok(json!({"skills": []}));
            };
            let root = std::path::PathBuf::from(cwd).join(".agents/skills");
            if !root.is_dir() {
                return Ok(json!({"skills": []}));
            }
            let provider = FilesystemSkillProvider::from_root(root)
                .map_err(|error| CompatError::internal(error.to_string()))?;
            let skills = provider
                .list(ContextHandle::root().scope().cancellation())
                .await
                .map_err(|error| CompatError::internal(error.to_string()))?
                .into_iter()
                .filter(|skill| skill.invocation.user_invocable)
                .map(|skill| {
                    let mut entry = json!({
                        "name": skill.name,
                        "description": skill.description,
                        "modelInvocable": skill.invocation.model_invocable,
                    });
                    if let Some(when_to_use) = skill.when_to_use {
                        entry["whenToUse"] = Value::String(when_to_use);
                    }
                    entry
                })
                .collect::<Vec<_>>();
            Ok(json!({"skills": skills}))
        }
        "agentPreset.list" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            let presets = state
                .host
                .agent_preset_list()
                .await
                .map_err(compat_preset_error)?;
            let (authorable, has_document) = state.host.agent_preset_capabilities();
            Ok(json!({"presets": presets, "authorable": authorable, "hasDocument": has_document}))
        }
        "agentPreset.read" => {
            let args: CompatAgentPresetRef = compat_decode(payload)?;
            compat_require_nonblank("agentPreset", &args.agent_preset)?;
            serde_json::to_value(
                state
                    .host
                    .agent_preset_read(args.agent_preset)
                    .await
                    .map_err(compat_preset_error)?,
            )
            .map_err(|error| CompatError::internal(error.to_string()))
        }
        "agentPreset.copy" => {
            let args: CompatAgentPresetCopy = compat_decode(payload)?;
            compat_require_nonblank("from", &args.from)?;
            compat_require_nonblank("agentPreset", &args.agent_preset)?;
            let agent_preset = state
                .host
                .agent_preset_copy(args.from, args.agent_preset, args.name)
                .await
                .map_err(compat_preset_error)?;
            Ok(json!({"agentPreset": agent_preset}))
        }
        "agentPreset.remove" => {
            let args: CompatAgentPresetRef = compat_decode(payload)?;
            compat_require_nonblank("agentPreset", &args.agent_preset)?;
            state
                .host
                .agent_preset_remove(args.agent_preset)
                .await
                .map_err(compat_preset_error)?;
            Ok(json!({}))
        }
        "agentPreset.openDocument" => {
            let args: CompatAgentPresetRef = compat_decode(payload)?;
            compat_require_nonblank("agentPreset", &args.agent_preset)?;
            serde_json::to_value(
                state
                    .host
                    .agent_preset_open_document(args.agent_preset)
                    .await
                    .map_err(compat_preset_error)?,
            )
            .map_err(|error| CompatError::internal(error.to_string()))
        }
        "agentPreset.select" => {
            let args: CompatAgentPresetSelect = compat_decode(payload)?;
            compat_require_session(&args.session_id)?;
            compat_require_nonblank("agentPreset", &args.agent_preset)?;
            let session_id = args.session_id.clone();
            let agent_preset = state
                .host
                .agent_preset_select(args.session_id, args.agent_preset)
                .await
                .map_err(compat_preset_error)?;
            if let Some(session) = compat_data(&state.compat).sessions.get_mut(&session_id) {
                session.agent_preset = Some(agent_preset.clone());
            }
            broadcast_compat(
                &state.compat,
                CompatStream::Host,
                json!({
                    "type": "host/remote-event",
                    "event": "agent-preset/selected",
                    "args": [session_id, agent_preset],
                }),
            );
            Ok(json!({"agentPreset": agent_preset}))
        }
        "llm.providers" => {
            let _: CompatEmptyPayload = compat_decode(payload)?;
            Ok(json!({
                "providers": state
                    .host
                    .provider_directory()
                    .into_iter()
                    .map(compat_provider)
                    .collect::<Vec<_>>(),
            }))
        }
        "llm.models" => compat_llm_models(state, compat_decode(payload)?).await,
        "llm.discoverModels" => compat_discover_models(state, compat_decode(payload)?).await,
        "llm.providerModels" => {
            let args: CompatProviderModels = compat_decode(payload)?;
            compat_require_nonblank("provider", &args.provider)?;
            if !args.config.is_object() {
                return Err(CompatError::invalid("config must be an object"));
            }
            serde_json::to_value(
                state
                    .host
                    .provider_models(args.provider, args.config)
                    .await
                    .map_err(compat_provider_models_error)?,
            )
            .map_err(|error| CompatError::internal(error.to_string()))
        }
        "llm.setProviderEnabled" => {
            let args: CompatSetProviderEnabled = compat_decode(payload)?;
            compat_require_nonblank("provider", &args.provider)?;
            serde_json::to_value(
                state
                    .host
                    .set_provider_enabled(args.provider, args.enabled)
                    .await
                    .map_err(compat_provider_models_error)?,
            )
            .map_err(|error| CompatError::internal(error.to_string()))
        }
        "credentials.describe" => compat_credentials_describe(state, compat_decode(payload)?).await,
        "credentials.set" => compat_credentials_set(state, compat_decode(payload)?).await,
        "credentials.unset" => compat_credentials_unset(state, compat_decode(payload)?).await,
        _ => Err(CompatError::not_found()),
    }
}

async fn compat_goal_service(
    state: &ApiState,
    session_id: &SessionId,
) -> Result<GoalService, CompatError> {
    compat_require_session(session_id)?;
    state
        .host
        .goal_service(session_id.clone())
        .await
        .map_err(|error| {
            if error.code == "SESSION_NOT_FOUND" {
                CompatError {
                    code: "session-not-found".into(),
                    message: error.message,
                    details: json!({"sessionId": session_id}),
                }
            } else {
                compat_host_error(error)
            }
        })
}

fn compat_goal_error(error: GoalError) -> CompatError {
    if error.code() == "CANCELLED" {
        CompatError {
            code: "cancelled".into(),
            message: error.to_string(),
            details: json!({}),
        }
    } else {
        CompatError {
            code: "internal".into(),
            message: error.to_string(),
            details: json!({"goalCode": error.code()}),
        }
    }
}

fn compat_validate_goal_create(args: &CompatGoalCreate) -> Result<(), CompatError> {
    if args.objective.is_empty() {
        return Err(CompatError::invalid("objective must be a non-empty string"));
    }
    if args
        .max_goal_rounds
        .is_some_and(|rounds| rounds == 0 || rounds > 9_007_199_254_740_991)
    {
        return Err(CompatError::invalid(
            "maxGoalRounds must be a positive safe integer",
        ));
    }
    Ok(())
}

fn compat_validate_goal_ref(reference: &GoalRef) -> Result<(), CompatError> {
    if reference.revision == 0 {
        return Err(CompatError::invalid(
            "ref.revision must be a positive integer",
        ));
    }
    Ok(())
}

async fn compat_goal_create(
    state: &ApiState,
    args: CompatGoalCreate,
) -> Result<Value, CompatError> {
    compat_validate_goal_create(&args)?;
    let goals = compat_goal_service(state, &args.session_id).await?;
    let cancellation = goals.cancellation();
    let snapshot = goals
        .create(args.objective, args.max_goal_rounds, cancellation)
        .await
        .map_err(compat_goal_error)?;
    goals.drive().await.map_err(compat_goal_error)?;
    Ok(json!({"ref": snapshot.reference}))
}

async fn compat_goal_edit(state: &ApiState, args: CompatGoalEdit) -> Result<Value, CompatError> {
    if args.objective.is_none() && args.max_goal_rounds.is_none() {
        return Err(CompatError::invalid(
            "goal.edit requires objective or maxGoalRounds",
        ));
    }
    if args.objective.as_deref() == Some("")
        || args
            .max_goal_rounds
            .is_some_and(|rounds| rounds == 0 || rounds > 9_007_199_254_740_991)
    {
        return Err(CompatError::invalid(
            "goal edit fields must be safe, positive, and non-empty",
        ));
    }
    compat_validate_goal_ref(&args.reference)?;
    let goals = compat_goal_service(state, &args.session_id).await?;
    let cancellation = goals.cancellation();
    let snapshot = goals
        .edit(
            args.reference,
            args.objective,
            args.max_goal_rounds,
            cancellation,
        )
        .await
        .map_err(compat_goal_error)?;
    Ok(json!({"ref": snapshot.reference}))
}

async fn compat_goal_pause(state: &ApiState, args: CompatGoalRef) -> Result<Value, CompatError> {
    compat_validate_goal_ref(&args.reference)?;
    let goals = compat_goal_service(state, &args.session_id).await?;
    let cancellation = goals.cancellation();
    let snapshot = goals
        .pause(args.reference, cancellation)
        .await
        .map_err(compat_goal_error)?;
    Ok(json!({"ref": snapshot.reference}))
}

async fn compat_goal_resume(state: &ApiState, args: CompatGoalRef) -> Result<Value, CompatError> {
    compat_validate_goal_ref(&args.reference)?;
    let goals = compat_goal_service(state, &args.session_id).await?;
    let cancellation = goals.cancellation();
    let snapshot = goals
        .resume(args.reference, cancellation)
        .await
        .map_err(compat_goal_error)?;
    goals.drive().await.map_err(compat_goal_error)?;
    Ok(json!({"ref": snapshot.reference}))
}

async fn compat_goal_complete(state: &ApiState, args: CompatGoalRef) -> Result<Value, CompatError> {
    compat_validate_goal_ref(&args.reference)?;
    let goals = compat_goal_service(state, &args.session_id).await?;
    let cancellation = goals.cancellation();
    let snapshot = goals
        .complete(args.reference, cancellation)
        .await
        .map_err(compat_goal_error)?;
    Ok(json!({"ref": snapshot.reference}))
}

async fn compat_goal_clear(state: &ApiState, args: CompatGoalRef) -> Result<Value, CompatError> {
    compat_validate_goal_ref(&args.reference)?;
    let goals = compat_goal_service(state, &args.session_id).await?;
    let cancellation = goals.cancellation();
    goals
        .clear(args.reference, cancellation)
        .await
        .map_err(compat_goal_error)?;
    Ok(json!({"cleared": true}))
}

async fn compat_remote_goal_view(
    state: &ApiState,
    session_id: &SessionId,
) -> Result<Value, CompatError> {
    let goals = compat_goal_service(state, session_id).await?;
    let projection = goals
        .projection()
        .await
        .ok_or_else(|| CompatError::internal("goal projection is unavailable"))?;
    let mut goal = projection
        .get("goal")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| CompatError::internal("goal projection is malformed"))?;
    let activation = if goal.get("phase").and_then(Value::as_str) == Some("active") {
        "armed"
    } else {
        "disarmed"
    };
    goal.insert("activation".into(), Value::String(activation.into()));
    for key in ["roundsStarted", "createdAt", "updatedAt"] {
        goal.insert(key.into(), projection[key].clone());
    }
    Ok(Value::Object(goal))
}

async fn compat_message_feedback_list(
    state: &ApiState,
    args: CompatMessageFeedbackList,
) -> Result<Value, CompatError> {
    state
        .host
        .message_feedback_list(args.session_id)
        .await
        .map_err(compat_host_error)
}

async fn compat_message_feedback_put(
    state: &ApiState,
    args: CompatMessageFeedbackPut,
) -> Result<Value, CompatError> {
    state
        .host
        .message_feedback_put(
            args.session_id,
            args.message_id,
            args.rating,
            args.note,
            args.if_version,
        )
        .await
        .map_err(compat_host_error)
}

async fn compat_message_feedback_delete(
    state: &ApiState,
    args: CompatMessageFeedbackDelete,
) -> Result<Value, CompatError> {
    state
        .host
        .message_feedback_delete(args.session_id, args.message_id, args.if_version)
        .await
        .map_err(compat_host_error)
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
        "hasDocument": state.host.has_settings_document(),
        "namespaces": namespaces,
    }))
}

async fn compat_settings_open_document(state: &ApiState) -> Result<Value, CompatError> {
    if state.host.settings().is_none() {
        return Err(CompatError::internal(
            "settings service is absent: this deployment does not mount a settings provider",
        ));
    }
    state
        .host
        .open_settings_document()
        .await
        .map_err(compat_host_error)?;
    Ok(json!({"opened": true}))
}

fn compat_settings_view(descriptor: SettingsDescriptor) -> Value {
    let mut view = Map::from_iter([
        ("ns".into(), Value::String(descriptor.namespace)),
        ("schema".into(), descriptor.schema),
        ("value".into(), descriptor.resolved),
        (
            "applies".into(),
            serde_json::to_value(descriptor.applies).expect("settings applies is serializable"),
        ),
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
    ns == PERMISSION_SETTINGS_NAMESPACE
        || matches!(
            ns,
            "agent-loop"
                | "agent-default-model"
                | "shell"
                | "locale"
                | "ui-conversation"
                | "ui-theme"
                | "web-search-deepseek"
                | "ui-onboarding"
                | "agent-presets"
        )
        || ns
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
    let message = error.to_string();
    match error {
        SettingsError::Conflict { expected, actual } => CompatError {
            code: "settings-conflict".into(),
            message: "settings revision conflict".into(),
            details: json!({"ns": ns, "expected": expected, "actual": actual}),
        },
        _ => CompatError {
            code: "settings-rejected".into(),
            message,
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
    state
        .host
        .mutate_settings(
            ns.clone(),
            HostSettingsMutation::Update {
                patch: Value::Object(args.patch),
                expected_revision: args.expected_revision,
            },
        )
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
    state
        .host
        .mutate_settings(
            ns.clone(),
            HostSettingsMutation::Replace {
                user: Value::Object(args.section),
                expected_revision: args.expected_revision,
            },
        )
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
    state
        .host
        .mutate_settings(
            ns.clone(),
            HostSettingsMutation::Mutate {
                ops,
                expected_revision: args.expected_revision,
            },
        )
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

fn compat_directory_error(message: &'static str) -> CompatError {
    CompatError {
        code: "directory-invalid-path".into(),
        message: message.into(),
        details: json!({}),
    }
}

fn compat_list_directory(args: CompatListDirectory) -> Result<Value, CompatError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| compat_directory_error("home directory is unavailable"))?;
    let home = fs::canonicalize(home)
        .map_err(|_| compat_directory_error("home directory is unavailable"))?;
    let requested = args
        .path
        .unwrap_or_else(|| home.to_string_lossy().into_owned());
    let path = fs::canonicalize(&requested)
        .map_err(|_| compat_directory_error("directory does not exist"))?;
    if !path.is_dir() {
        return Err(compat_directory_error("path is not a directory"));
    }

    let mut entries = fs::read_dir(&path)
        .map_err(|_| compat_directory_error("directory is not readable"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some(json!({
                "hidden": name.starts_with('.'),
                "name": name,
                "path": entry.path().to_string_lossy(),
            }))
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    let truncated = entries.len() > MAX_DIRECTORY_ENTRIES;
    entries.truncate(MAX_DIRECTORY_ENTRIES);

    let mut crumbs = path
        .ancestors()
        .map(|ancestor| {
            let name = ancestor
                .file_name()
                .unwrap_or(ancestor.as_os_str())
                .to_string_lossy();
            json!({"hidden": false, "name": name, "path": ancestor.to_string_lossy()})
        })
        .collect::<Vec<_>>();
    crumbs.reverse();

    Ok(json!({
        "path": path.to_string_lossy(),
        "home": home.to_string_lossy(),
        "crumbs": crumbs,
        "entries": entries,
        "truncated": truncated,
    }))
}

fn compat_create_directory(args: CompatCreateDirectory) -> Result<Value, CompatError> {
    let name = args.name.trim();
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\']) {
        return Err(CompatError::invalid(
            "directory name must be one non-blank path segment",
        ));
    }
    let parent = fs::canonicalize(&args.path)
        .map_err(|_| compat_directory_error("parent directory does not exist"))?;
    if !parent.is_dir() {
        return Err(compat_directory_error("parent path is not a directory"));
    }
    let path = parent.join(name);
    fs::create_dir(&path).map_err(|_| compat_directory_error("directory could not be created"))?;
    let path = fs::canonicalize(path)
        .map_err(|_| compat_directory_error("created directory is unavailable"))?;
    Ok(json!({"path": path.to_string_lossy()}))
}

async fn compat_open_path(state: &ApiState, args: CompatOpenPath) -> Result<Value, CompatError> {
    if args.path.trim().is_empty() {
        return Err(CompatError::invalid("path must not be empty"));
    }
    state
        .host
        .open_path(args.path)
        .await
        .map_err(compat_host_error)?;
    Ok(json!({"opened": true}))
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

fn compat_workspace_insert_before(
    state: &ApiState,
    args: CompatWorkspaceInsertBefore,
) -> Result<Value, CompatError> {
    if args.workspace_id.is_empty() {
        return Err(CompatError::invalid("workspaceId must not be empty"));
    }
    if args
        .before_workspace_id
        .as_deref()
        .is_some_and(str::is_empty)
    {
        return Err(CompatError::invalid("beforeWorkspaceId must not be empty"));
    }
    let registry = compat_registry(state)?;
    let before = registry
        .snapshot()
        .items
        .into_iter()
        .map(|workspace| workspace.workspace_id)
        .collect::<Vec<_>>();
    let workspace_ids = registry
        .insert_before(
            &args.workspace_id,
            args.before_workspace_id.as_deref(),
            None,
        )
        .map_err(compat_workspace_error)?;
    if before != workspace_ids {
        broadcast_compat(
            &state.compat,
            CompatStream::Host,
            json!({"type": "host/workspace-order-changed", "workspaceIds": workspace_ids.clone()}),
        );
    }
    Ok(json!({"workspaceIds": workspace_ids}))
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
    let mut projections = BTreeMap::new();
    let attachment_limits = state.host.attachment_limits();
    for session in &sessions {
        let events = state
            .host
            .events(session.session_id.clone(), 0)
            .await
            .map_err(compat_host_error)?;
        let mut values = compat_derived_projection_values(&events, &attachment_limits);
        for projection in state
            .host
            .session_projections(session.session_id.clone())
            .await
            .map_err(compat_host_error)?
        {
            values.insert(projection.key, projection.value);
        }
        projections.insert(
            session.session_id.clone(),
            (!values.is_empty()).then_some(CompatSessionProjections {
                as_of_seq: events.last().map_or(0, |event| event.seq),
                values,
            }),
        );
    }
    let mut data = compat_data(&state.compat);
    let live_ids: BTreeSet<_> = sessions
        .iter()
        .map(|session| session.session_id.clone())
        .collect();
    data.sessions.retain(|id, _| live_ids.contains(id));
    for session in sessions {
        let projection = projections.remove(&session.session_id).flatten();
        let agent_preset = session.agent_preset.clone();
        let entry = data
            .sessions
            .entry(session.session_id.clone())
            .or_insert_with(|| CompatSession {
                session_id: session.session_id.clone(),
                workspace_id: session.workspace_id.clone(),
                updated_at: session.updated_at.min(MAX_SAFE_INTEGER),
                running: session.running,
                blank: session.blank,
                cwd: session.cwd.clone(),
                parent_session_id: session.parent_session.clone(),
                origin: session.origin,
                agent_preset: agent_preset.clone(),
                projections: projection.clone(),
            });
        entry.workspace_id = session.workspace_id;
        entry.updated_at = entry
            .updated_at
            .max(session.updated_at.min(MAX_SAFE_INTEGER));
        entry.running = session.running;
        entry.blank = session.blank;
        entry.cwd = session.cwd;
        entry.parent_session_id = session.parent_session;
        entry.origin = session.origin;
        entry.agent_preset = agent_preset;
        entry.projections = projection;
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

fn compat_session_added_payload(session: &CompatSession) -> Value {
    let mut payload = json!({
        "type": "host/session-added",
        "sessionId": session.session_id,
        "blank": session.blank,
    });
    let fields = payload
        .as_object_mut()
        .expect("session-added payload is an object");
    for (name, value) in [
        (
            "cwd",
            serde_json::to_value(&session.cwd).expect("cwd serializes"),
        ),
        (
            "workspaceId",
            serde_json::to_value(&session.workspace_id).expect("workspace id serializes"),
        ),
        (
            "parentSessionId",
            serde_json::to_value(&session.parent_session_id).expect("parent session serializes"),
        ),
        (
            "origin",
            serde_json::to_value(session.origin).expect("origin serializes"),
        ),
        (
            "agentPreset",
            serde_json::to_value(&session.agent_preset).expect("agent preset serializes"),
        ),
    ] {
        if !value.is_null() {
            fields.insert(name.into(), value);
        }
    }
    payload
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
            updated_at: persisted.updated_at.min(MAX_SAFE_INTEGER),
            running: persisted.running,
            blank: persisted.blank,
            cwd: persisted.cwd,
            parent_session_id: persisted.parent_session,
            origin: persisted.origin,
            agent_preset: persisted.agent_preset,
            projections: None,
        };
        compat_data(&state.compat)
            .sessions
            .insert(session_id.clone(), session.clone());
        if existing.is_none() {
            broadcast_compat(
                &state.compat,
                CompatStream::Host,
                compat_session_added_payload(&session),
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
        updated_at: persisted.updated_at.min(MAX_SAFE_INTEGER),
        running: persisted.running,
        blank: persisted.blank,
        cwd: persisted.cwd.or_else(|| Some(requested_cwd.1.clone())),
        parent_session_id: persisted.parent_session,
        origin: persisted.origin,
        agent_preset: persisted.agent_preset,
        projections: None,
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
            compat_session_added_payload(&session),
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
    let max_messages = args.max_messages.unwrap_or(50);
    if max_messages == 0 || max_messages > 1_000 {
        return Err(CompatError::invalid(
            "maxMessages must be between 1 and 1000",
        ));
    }
    let events: Vec<_> = match state.host.events(args.session_id.clone(), 0).await {
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
    let tool_calls = compat_tool_calls(&events);
    let attachment_limits = state.host.attachment_limits();
    let projections = if args.before_seq.is_none() {
        let mut values = compat_derived_projection_values(&events, &attachment_limits);
        for projection in state
            .host
            .session_projections(args.session_id.clone())
            .await
            .map_err(compat_host_error)?
        {
            values.insert(projection.key, projection.value);
        }
        let as_of_seq = events.last().map_or(-1, |event| event.seq as i64);
        Some(json!({"asOfSeq": as_of_seq, "values": values}))
    } else {
        None
    };
    let (events, has_more) = compat_paginate_history(events, max_messages);
    let cwd = compat_data(&state.compat)
        .sessions
        .get(&args.session_id)
        .and_then(|session| session.cwd.clone())
        .unwrap_or_else(|| state.compat.cwd.clone());
    let entries = events
        .iter()
        .map(|event| {
            let mut entry = json!({"event": event});
            if let Some(view) = compat_history_tool_view(event, &tool_calls, &cwd) {
                entry["view"] = view;
            }
            entry
        })
        .collect::<Vec<_>>();
    let mut output = json!({
        "events": entries,
        "hasMore": has_more,
    });
    if let Some(projections) = projections {
        output["projections"] = projections;
    }
    Ok(output)
}

fn compat_paginate_history(
    mut events: Vec<SessionEvent>,
    max_messages: usize,
) -> (Vec<SessionEvent>, bool) {
    let mut messages = 0;
    let mut cut = 0;
    for (index, event) in events.iter().enumerate().rev() {
        if matches!(
            event.event_type.as_str(),
            "user/message" | "assistant/message"
        ) {
            messages += 1;
        }
        if event.event_type == "turn/start" && messages >= max_messages {
            cut = index;
            break;
        }
    }
    events.drain(..cut);
    (events, cut > 0)
}

fn compat_tool_calls(events: &[SessionEvent]) -> BTreeMap<String, CompatToolCall> {
    events
        .iter()
        .filter_map(|event| {
            (event.event_type == "tool/call")
                .then(|| {
                    Some((
                        event.data.get("callId")?.as_str()?.to_owned(),
                        compat_tool_call(event)?,
                    ))
                })
                .flatten()
        })
        .collect()
}

fn compat_history_tool_view(
    event: &SessionEvent,
    calls: &BTreeMap<String, CompatToolCall>,
    cwd: &str,
) -> Option<Value> {
    if event.event_type == "tool/call" {
        return compat_tool_call(event)
            .and_then(|call| compat_present_tool_call(&call, cwd))
            .map(|view| json!({"for": "call", "view": view}));
    }
    let call = calls.get(compat_tool_result_call_id(event)?)?;
    compat_present_tool_result(call, event).map(|view| json!({"for": "result", "view": view}))
}

async fn compat_live_tool_view(
    state: &ApiState,
    session_id: &SessionId,
    event: &SessionEvent,
) -> Option<Value> {
    if event.event_type == "tool/call" {
        let call_id = event.data.get("callId")?.as_str()?.to_owned();
        let call = compat_tool_call(event)?;
        let mut data = compat_data(&state.compat);
        let cwd = data
            .sessions
            .get(session_id)
            .and_then(|session| session.cwd.as_deref())
            .unwrap_or(&state.compat.cwd)
            .to_owned();
        // ponytail: bounded presentation cache; use an LRU only if concurrent pending calls exceed this ceiling.
        if data.tool_calls.len() >= 4_096 {
            data.tool_calls.pop_first();
        }
        data.tool_calls
            .insert((session_id.clone(), call_id), call.clone());
        return compat_present_tool_call(&call, &cwd)
            .map(|view| json!({"for": "call", "view": view}));
    }
    let call_id = compat_tool_result_call_id(event)?;
    let cached = compat_data(&state.compat)
        .tool_calls
        .get(&(session_id.clone(), call_id.to_owned()))
        .cloned();
    let call = match cached {
        Some(call) => call,
        None => state
            .host
            .events(session_id.clone(), 0)
            .await
            .ok()?
            .into_iter()
            .rev()
            .find_map(|candidate| {
                (candidate.event_type == "tool/call"
                    && candidate.data.get("callId").and_then(Value::as_str) == Some(call_id))
                .then(|| compat_tool_call(&candidate))
                .flatten()
            })?,
    };
    compat_present_tool_result(&call, event).map(|view| json!({"for": "result", "view": view}))
}

fn compat_tool_call(event: &SessionEvent) -> Option<CompatToolCall> {
    Some(CompatToolCall {
        name: event.data.get("name")?.as_str()?.to_owned(),
        arguments: event.data.get("arguments")?.as_str()?.to_owned(),
    })
}

fn compat_present_tool_call(call: &CompatToolCall, cwd: &str) -> Option<Value> {
    let arguments: Value = serde_json::from_str(&call.arguments).ok()?;
    if call.name == "bash" {
        let command = arguments.get("command")?.as_str()?;
        let mut view = json!({"card": "terminal", "title": command, "cwd": cwd});
        if let Some(description) = arguments.get("description").and_then(Value::as_str) {
            view["description"] = json!(description);
        }
        return Some(view);
    }
    if matches!(call.name.as_str(), "write" | "edit" | "apply_patch") {
        let path = arguments
            .get("file_path")
            .or_else(|| arguments.get("path"))
            .and_then(Value::as_str)?;
        let (old_text, new_text) = match call.name.as_str() {
            "write" => (
                Value::Null,
                arguments.get("content").cloned().unwrap_or(Value::Null),
            ),
            "edit" => (
                arguments.get("old_string").cloned().unwrap_or(Value::Null),
                arguments.get("new_string").cloned().unwrap_or(Value::Null),
            ),
            _ => (Value::Null, Value::Null),
        };
        return Some(json!({
            "card": "diff",
            "title": format!("{} {}", call.name, path),
            "diffs": [{"path": path, "oldText": old_text, "newText": new_text}],
            "locations": [{"path": path}],
        }));
    }
    Some(json!({
        "card": "generic",
        "title": call.name.as_str(),
        "kind": compat_tool_kind(&call.name),
        "rawInput": arguments,
    }))
}

fn compat_tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "read_image" => "read",
        "edit" | "write" | "apply_patch" => "edit",
        "delete" => "delete",
        "move" | "rename" => "move",
        "grep" | "glob" | "web_search" => "search",
        "eval" | "execute" => "execute",
        "web_fetch" => "fetch",
        _ => "other",
    }
}

fn compat_present_tool_result(call: &CompatToolCall, event: &SessionEvent) -> Option<Value> {
    if event
        .data
        .pointer("/error/code")
        .or_else(|| event.data.pointer("/message/error/code"))
        .or_else(|| event.data.pointer("/meta/code"))
        .and_then(Value::as_str)
        .is_some_and(|code| matches!(code, "ABORTED" | "ABORTED_BEFORE_DISPATCH"))
    {
        return None;
    }
    match call.name.as_str() {
        "write" | "edit" | "apply_patch" => {
            let metadata = event.data.get("meta").cloned().unwrap_or_else(|| json!({}));
            Some(json!({
                "card": "diff",
                "title": call.name.as_str(),
                "diffs": metadata.get("diffs").cloned().unwrap_or_else(|| json!([])),
                "locations": metadata.get("locations").cloned().unwrap_or_else(|| json!([])),
            }))
        }
        "web_search" => {
            compat_web_search_result(call, event).or_else(|| Some(json!({"card": "generic"})))
        }
        "web_fetch" => compat_web_fetch_result(event).or_else(|| Some(json!({"card": "generic"}))),
        "grep" | "glob" => compat_search_result(event).or_else(|| Some(json!({"card": "generic"}))),
        "bash" => {
            let output = compat_tool_result_text(event)?;
            let mut view = json!({
                "card": "terminal",
                "output": output,
            });
            if let Some(signal) = event.data.pointer("/meta/signal").and_then(Value::as_str) {
                view["signal"] = json!(signal);
            } else {
                view["exitCode"] = json!(compat_terminal_exit_code(&output).unwrap_or(0));
            }
            Some(view)
        }
        _ => Some(json!({"card": "generic"})),
    }
}

fn compat_search_result(event: &SessionEvent) -> Option<Value> {
    if event.data.pointer("/message/content/0/isError") == Some(&Value::Bool(true)) {
        return None;
    }
    let meta = event.data.get("meta")?;
    let shape = meta.get("shape")?.as_str()?;
    let truncated = meta.get("truncated")?.as_bool()?;
    let total = meta.get("total")?.as_u64()?;
    match shape {
        "matches" if meta.get("files").is_some_and(Value::is_array) => Some(json!({
            "card": "search",
            "shape": "matches",
            "files": meta.get("files")?,
            "truncated": truncated,
            "total": total,
        })),
        "paths" if meta.get("paths").is_some_and(Value::is_array) => Some(json!({
            "card": "search",
            "shape": "paths",
            "paths": meta.get("paths")?,
            "truncated": truncated,
            "total": total,
        })),
        _ => None,
    }
}

fn compat_web_search_result(call: &CompatToolCall, event: &SessionEvent) -> Option<Value> {
    if event.data.pointer("/message/content/0/isError") == Some(&Value::Bool(true)) {
        return None;
    }
    let query = serde_json::from_str::<Value>(&call.arguments)
        .ok()?
        .get("query")?
        .as_str()?
        .to_owned();
    let meta = event.data.get("meta")?;
    let sources = meta
        .get("sources")?
        .as_array()?
        .iter()
        .filter_map(|source| {
            let url = source.get("url")?.as_str()?.trim();
            (!url.is_empty()).then(|| {
                let mut projected = serde_json::Map::new();
                projected.insert("url".into(), Value::String(url.to_owned()));
                for key in ["title", "snippet", "publishedAt"] {
                    if let Some(value) = source
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                    {
                        projected.insert(key.into(), Value::String(value.to_owned()));
                    }
                }
                Value::Object(projected)
            })
        })
        .collect::<Vec<_>>();
    Some(json!({
        "card": "web",
        "kind": "search",
        "title": query,
        "sources": sources,
        "truncated": meta.get("truncated").and_then(Value::as_bool).unwrap_or(false),
    }))
}

fn compat_web_fetch_result(event: &SessionEvent) -> Option<Value> {
    if event.data.pointer("/message/content/0/isError") == Some(&Value::Bool(true)) {
        return None;
    }
    let meta = event.data.get("meta")?;
    let url = meta.get("url")?.as_str()?.trim();
    let status_code = meta.get("statusCode")?.as_u64()?;
    (!url.is_empty() && u16::try_from(status_code).is_ok()).then(|| {
        json!({
            "card": "web",
            "kind": "fetch",
            "url": url,
            "statusCode": status_code,
            "truncated": meta.get("truncated").and_then(Value::as_bool).unwrap_or(false),
        })
    })
}

fn compat_tool_result_text(event: &SessionEvent) -> Option<String> {
    Some(
        event
            .data
            .pointer("/message/content")?
            .as_array()?
            .iter()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("tool-result"))?
            .get("content")?
            .as_array()?
            .iter()
            .filter_map(|block| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| block.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .collect(),
    )
}

fn compat_tool_result_call_id(event: &SessionEvent) -> Option<&str> {
    (event.event_type == "tool/result")
        .then(|| {
            event
                .data
                .pointer("/message/source/callId")
                .and_then(Value::as_str)
        })
        .flatten()
}

fn compat_terminal_exit_code(output: &str) -> Option<i64> {
    let marker = output.rsplit_once("[exit code: ")?.1;
    marker.split_once(']')?.0.parse().ok()
}

#[derive(Clone, Copy, Default)]
struct CompatTokenBuckets {
    uncached_input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
}

#[derive(Clone, Copy)]
struct CompatUsageSample {
    turn: u64,
    step: u64,
    buckets: CompatTokenBuckets,
}

#[derive(Clone, Copy)]
struct CompatOpenStep {
    turn: u64,
    step: u64,
    start_time: u64,
    first_token_time: Option<u64>,
}

#[derive(Default)]
struct CompatSessionStats {
    turns: u64,
    steps: u64,
    llm_ms: u64,
    tool_ms: u64,
    ttft_ms: u64,
    ttft_steps: u64,
    decode_ms: u64,
    decode_tokens: u64,
    last_turn: Option<u64>,
    open_step: Option<CompatOpenStep>,
    pending_calls: BTreeMap<String, u64>,
}

fn compat_token_usage(events: &[SessionEvent]) -> Value {
    let mut totals = CompatTokenBuckets::default();
    let mut last = None;
    for event in events {
        let Some((turn, step, buckets)) = compat_token_usage_sample(event) else {
            continue;
        };
        let previous = last
            .filter(|sample: &CompatUsageSample| sample.turn == turn && sample.step == step)
            .map(|sample| sample.buckets)
            .unwrap_or_default();
        totals.uncached_input_tokens = totals
            .uncached_input_tokens
            .saturating_sub(previous.uncached_input_tokens)
            .saturating_add(buckets.uncached_input_tokens);
        totals.output_tokens = totals
            .output_tokens
            .saturating_sub(previous.output_tokens)
            .saturating_add(buckets.output_tokens);
        totals.cache_read_tokens = totals
            .cache_read_tokens
            .saturating_sub(previous.cache_read_tokens)
            .saturating_add(buckets.cache_read_tokens);
        totals.cache_write_tokens = totals
            .cache_write_tokens
            .saturating_sub(previous.cache_write_tokens)
            .saturating_add(buckets.cache_write_tokens);
        last = Some(CompatUsageSample {
            turn,
            step,
            buckets,
        });
    }
    json!({
        "uncachedInputTokens": totals.uncached_input_tokens,
        "outputTokens": totals.output_tokens,
        "cacheReadTokens": totals.cache_read_tokens,
        "cacheWriteTokens": totals.cache_write_tokens,
    })
}
fn compat_context_pressure(events: &[SessionEvent]) -> Value {
    let context_window = events.iter().rev().find_map(|event| {
        (event.event_type == "request/context")
            .then(|| event.data.get("contextWindow").and_then(Value::as_u64))
            .flatten()
    });
    let pressure_tokens = events.iter().rev().find_map(|event| {
        compat_token_usage_sample(event).map(|(_, _, buckets)| {
            buckets
                .uncached_input_tokens
                .saturating_add(buckets.cache_read_tokens)
                .saturating_add(buckets.cache_write_tokens)
        })
    });
    let mut value = Map::new();
    if let Some(context_window) = context_window {
        value.insert("contextWindow".into(), Value::from(context_window));
    }
    if let Some(pressure_tokens) = pressure_tokens {
        value.insert("pressureTokens".into(), Value::from(pressure_tokens));
    }
    Value::Object(value)
}

fn compat_token_usage_sample(event: &SessionEvent) -> Option<(u64, u64, CompatTokenBuckets)> {
    let (turn, step) = compat_turn_step(event)?;
    let usage = match event.event_type.as_str() {
        "assistant/chunk"
            if event.data.pointer("/chunk/type").and_then(Value::as_str) == Some("usage") =>
        {
            event.data.pointer("/chunk/usage")
        }
        "assistant/message" => event.data.get("usage"),
        _ => None,
    }?;
    Some((
        turn,
        step,
        CompatTokenBuckets {
            uncached_input_tokens: usage.get("inputTokens")?.as_u64()?,
            output_tokens: usage.get("outputTokens")?.as_u64()?,
            cache_read_tokens: usage
                .get("cacheReadTokens")
                .map_or(Some(0), Value::as_u64)?,
            cache_write_tokens: usage
                .get("cacheWriteTokens")
                .map_or(Some(0), Value::as_u64)?,
        },
    ))
}

fn compat_session_stats(events: &[SessionEvent]) -> Value {
    let mut stats = CompatSessionStats::default();
    for event in events {
        match event.event_type.as_str() {
            "step/start" => {
                if let Some((turn, step)) = compat_turn_step(event) {
                    stats.open_step = Some(CompatOpenStep {
                        turn,
                        step,
                        start_time: event.time,
                        first_token_time: None,
                    });
                }
            }
            "assistant/chunk" => {
                if !compat_chunk_is_token_delta(event) {
                    continue;
                }
                let Some((turn, step)) = compat_turn_step(event) else {
                    continue;
                };
                if let Some(open) = stats.open_step.as_mut() {
                    if open.turn == turn && open.step == step && open.first_token_time.is_none() {
                        open.first_token_time = Some(event.time);
                    }
                }
            }
            "assistant/message" => {
                let Some((turn, step)) = compat_turn_step(event) else {
                    continue;
                };
                let Some(open) = stats.open_step else {
                    continue;
                };
                if open.turn != turn || open.step != step {
                    continue;
                }
                stats.llm_ms = stats
                    .llm_ms
                    .saturating_add(event.time.saturating_sub(open.start_time));
                stats.open_step = None;
                if let Some(first_token_time) = open.first_token_time {
                    stats.ttft_ms = stats
                        .ttft_ms
                        .saturating_add(first_token_time.saturating_sub(open.start_time));
                    stats.ttft_steps = stats.ttft_steps.saturating_add(1);
                    if let Some(output_tokens) = event
                        .data
                        .get("usage")
                        .and_then(|usage| usage.get("outputTokens"))
                        .and_then(Value::as_u64)
                    {
                        stats.decode_ms = stats
                            .decode_ms
                            .saturating_add(event.time.saturating_sub(first_token_time));
                        stats.decode_tokens = stats.decode_tokens.saturating_add(output_tokens);
                    }
                }
            }
            "tool/call" => {
                if let Some(call_id) = event.data.get("callId").and_then(Value::as_str) {
                    stats.pending_calls.insert(call_id.to_owned(), event.time);
                }
            }
            "tool/result" => {
                if let Some(call_id) = compat_tool_result_call_id(event) {
                    if let Some(dispatched) = stats.pending_calls.remove(call_id) {
                        stats.tool_ms = stats
                            .tool_ms
                            .saturating_add(event.time.saturating_sub(dispatched));
                    }
                }
            }
            "step/end" => {
                let Some((turn, _)) = compat_turn_step(event) else {
                    continue;
                };
                stats.turns = stats
                    .turns
                    .saturating_add(u64::from(stats.last_turn != Some(turn)));
                stats.steps = stats.steps.saturating_add(1);
                stats.last_turn = Some(turn);
                stats.open_step = None;
            }
            "turn/end" => stats.pending_calls.clear(),
            _ => {}
        }
    }
    json!({
        "turns": stats.turns,
        "steps": stats.steps,
        "llmMs": stats.llm_ms,
        "toolMs": stats.tool_ms,
        "ttftMs": stats.ttft_ms,
        "ttftSteps": stats.ttft_steps,
        "decodeMs": stats.decode_ms,
        "decodeTokens": stats.decode_tokens,
    })
}

fn compat_turn_step(event: &SessionEvent) -> Option<(u64, u64)> {
    Some((
        event.data.get("turn")?.as_u64()?,
        event.data.get("step")?.as_u64()?,
    ))
}

fn compat_chunk_is_token_delta(event: &SessionEvent) -> bool {
    let Some(chunk) = event.data.get("chunk") else {
        return false;
    };
    match chunk.get("type").and_then(Value::as_str) {
        Some("text-delta") | Some("reasoning-delta") => chunk
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty()),
        Some("tool-call-delta") => {
            chunk
                .get("argumentsDelta")
                .and_then(Value::as_str)
                .is_some_and(|arguments| !arguments.is_empty())
                || chunk.get("name").is_some_and(|name| !name.is_null())
        }
        _ => false,
    }
}

fn compat_subagent_error(
    error: TessivumError,
    parent_session_id: &SessionId,
    child_session_id: &SessionId,
) -> CompatError {
    let details = json!({
        "parentSessionId": parent_session_id,
        "childSessionId": child_session_id,
    });
    match error.code.as_str() {
        "CANCELLED" => CompatError {
            code: "cancelled".into(),
            message: error.message,
            details: json!({}),
        },
        "SESSION_NOT_FOUND" | "SUBAGENT_MODE_MISMATCH" => CompatError {
            code: "subagent-not-found".into(),
            message: "subagent is not a direct child with the requested mode".into(),
            details,
        },
        "SUBAGENT_PARENT_MISMATCH" => CompatError {
            code: "subagent-unauthorized".into(),
            message: "subagent does not belong to this parent".into(),
            details: json!({"childSessionId": child_session_id}),
        },
        "SUBAGENT_PARENT_REQUIRED" => CompatError {
            code: "subagent-parent-unavailable".into(),
            message: format!("parent session {parent_session_id:?} is not live"),
            details: json!({"parentSessionId": parent_session_id}),
        },
        "SUBAGENT_NOT_RESUMABLE" => CompatError {
            code: "subagent-not-resumable".into(),
            message: "subagent cannot be resumed".into(),
            details: json!({"childSessionId": child_session_id}),
        },
        "SUBAGENT_DELIVERY_UNAVAILABLE" | "AGENT_BUSY" => CompatError {
            code: "subagent-delivery-unavailable".into(),
            message: "subagent follow-up is temporarily unavailable".into(),
            details: json!({"childSessionId": child_session_id}),
        },
        "SUBAGENT_DELETE_ACTIVE" => CompatError {
            code: "subagent-active".into(),
            message: "subagent is still active".into(),
            details,
        },
        "SUBAGENT_DELETE_HAS_CHILDREN" => CompatError {
            code: "subagent-has-children".into(),
            message: "subagent has descendants; delete them first".into(),
            details,
        },
        "SUBAGENT_DELETE_UNAVAILABLE" => CompatError {
            code: "subagent-delete-unavailable".into(),
            message: "subagent is not a deletable direct child".into(),
            details,
        },
        _ => CompatError::internal(error.message),
    }
}

async fn compat_subagent_history(
    state: &ApiState,
    args: CompatSubagentHistory,
) -> Result<Value, CompatError> {
    compat_require_session(&args.parent_session_id)?;
    compat_require_session(&args.child_session_id)?;
    if let Some(before_seq) = args.before_seq {
        if before_seq > MAX_SAFE_INTEGER {
            return Err(CompatError::invalid(
                "beforeSeq exceeds the supported range",
            ));
        }
    }
    let max_messages = args
        .max_messages
        .map(|max_messages| {
            if max_messages == 0 || max_messages > MAX_SAFE_INTEGER {
                return Err(CompatError::invalid(
                    "maxMessages must be a positive supported integer",
                ));
            }
            usize::try_from(max_messages)
                .map_err(|_| CompatError::invalid("maxMessages exceeds the platform size limit"))
        })
        .transpose()?;
    let parent_session_id = args.parent_session_id.clone();
    let child_session_id = args.child_session_id.clone();
    let tail = args.before_seq.is_none();
    let mut result = state
        .host
        .subagent_history(SubagentHistoryRequest {
            parent_session_id: args.parent_session_id,
            child_session_id: args.child_session_id,
            mode: args.mode,
            before_seq: args.before_seq,
            max_messages,
        })
        .await
        .map_err(|error| compat_subagent_error(error, &parent_session_id, &child_session_id))?;
    if tail {
        let events = state
            .host
            .events(child_session_id.clone(), 0)
            .await
            .map_err(compat_host_error)?;
        let mut values = compat_derived_projection_values(&events, &state.host.attachment_limits());
        for projection in state
            .host
            .session_projections(child_session_id)
            .await
            .map_err(compat_host_error)?
        {
            values.insert(projection.key, projection.value);
        }
        result.projections = Some(SessionProjectionsBlock {
            as_of_seq: events.last().map_or(-1, |event| event.seq as i64),
            values,
        });
    }
    serde_json::to_value(result).map_err(|error| CompatError::internal(error.to_string()))
}

async fn compat_subagent_prompt(
    state: &ApiState,
    args: CompatSubagentPrompt,
) -> Result<Value, CompatError> {
    compat_require_session(&args.parent_session_id)?;
    compat_require_session(&args.child_session_id)?;
    for block in &args.content {
        block
            .validate()
            .map_err(|error| CompatError::invalid(error.to_string()))?;
    }
    let client_time_zone = args.client_time_zone.as_deref().map_or(Ok(None), |value| {
        canonical_client_time_zone(value)
            .map(Some)
            .ok_or_else(|| CompatError {
                code: "invalid-time-zone".into(),
                message: "clientTimeZone must be UTC or a valid IANA Area/Location name".into(),
                details: json!({"value": value}),
            })
    })?;
    let parent_session_id = args.parent_session_id.clone();
    let child_session_id = args.child_session_id.clone();
    let mode = match args.mode {
        ContinuableMode::Continuable => SubagentMode::Continuable,
    };
    let result = state
        .host
        .subagent_prompt(SubagentPromptRequest {
            parent_session_id: args.parent_session_id,
            child_session_id: args.child_session_id,
            mode,
            content: args.content,
            client_time_zone,
        })
        .await
        .map_err(|error| compat_subagent_error(error, &parent_session_id, &child_session_id))?;
    serde_json::to_value(result).map_err(|error| CompatError::internal(error.to_string()))
}

async fn compat_subagent_interrupt(
    state: &ApiState,
    args: CompatSubagentInterrupt,
) -> Result<Value, CompatError> {
    compat_require_session(&args.parent_session_id)?;
    compat_require_session(&args.child_session_id)?;
    let parent_session_id = args.parent_session_id.clone();
    let child_session_id = args.child_session_id.clone();
    let mode = match args.mode {
        ContinuableMode::Continuable => SubagentMode::Continuable,
    };
    let result = state
        .host
        .subagent_interrupt(SubagentInterruptRequest {
            parent_session_id: args.parent_session_id,
            child_session_id: args.child_session_id,
            mode,
        })
        .await
        .map_err(|error| compat_subagent_error(error, &parent_session_id, &child_session_id))?;
    serde_json::to_value(result).map_err(|error| CompatError::internal(error.to_string()))
}
async fn compat_subagent_delete(
    state: &ApiState,
    args: CompatSubagentDelete,
) -> Result<Value, CompatError> {
    compat_require_session(&args.parent_session_id)?;
    compat_require_session(&args.child_session_id)?;
    let parent_session_id = args.parent_session_id.clone();
    let child_session_id = args.child_session_id.clone();
    let result = state
        .host
        .subagent_delete(SubagentDeleteRequest {
            parent_session_id: args.parent_session_id,
            child_session_id: args.child_session_id,
        })
        .await
        .map_err(|error| compat_subagent_error(error, &parent_session_id, &child_session_id))?;
    serde_json::to_value(result).map_err(|error| CompatError::internal(error.to_string()))
}

async fn compat_session_models(
    state: &ApiState,
    session_id: SessionId,
) -> Result<Value, CompatError> {
    let models: HostSessionModels = state
        .host
        .session_models(session_id)
        .await
        .map_err(compat_host_error)?;
    let active = state
        .host
        .provider_directory()
        .into_iter()
        .filter(|entry| entry.active)
        .map(|entry| entry.route.id)
        .collect::<BTreeSet<_>>();
    Ok(json!({
        "current": models.current,
        "routable": models.routable,
        "groups": models.groups.into_iter().filter(|group| active.contains(&group.provider)).map(compat_model_group).collect::<Vec<_>>(),
        "failures": [],
    }))
}

async fn compat_session_attachment(
    state: &ApiState,
    args: CompatSessionAttachment,
) -> Result<Value, CompatError> {
    compat_require_session(&args.session_id)?;
    let data = state
        .host
        .read_attachment(args.session_id, args.attachment_id)
        .await
        .map_err(compat_host_error)?;
    Ok(json!({
        "attachment": data.reference.safe_metadata(),
        "data": base64_encode(&data.data),
    }))
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        output.push(TABLE[(first >> 2) as usize] as char);
        let second = chunk.get(1).copied().unwrap_or(0);
        output.push(TABLE[((first & 0x03) << 4 | second >> 4) as usize] as char);
        if chunk.len() > 1 {
            output.push(
                TABLE[((second & 0x0f) << 2 | chunk.get(2).copied().unwrap_or(0) >> 6) as usize]
                    as char,
            );
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(chunk[2] & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

fn compat_provider(entry: HostProviderDirectoryEntry) -> Value {
    json!({
        "provider": entry.route.id,
        "displayName": entry.route.display_name,
        "settingsNs": entry.namespace,
        "settingsPath": entry.settings_path,
        "active": entry.active,
        "declared": entry.declared,
    })
}

fn compat_model_info(info: HostModelInfo) -> Value {
    let mut model = Map::from_iter([
        ("id".into(), Value::String(info.id.clone())),
        (
            "name".into(),
            Value::String(info.name.unwrap_or_else(|| info.id.clone())),
        ),
    ]);
    if let Some(description) = info.description {
        model.insert("description".into(), Value::String(description));
    }
    if let Some(reasoning) = info.reasoning {
        model.insert(
            "reasoning".into(),
            serde_json::to_value(reasoning).expect("reasoning metadata serializes"),
        );
    }
    Value::Object(model)
}

fn compat_model_group(group: HostModelGroup) -> Value {
    json!({
        "id": group.provider,
        "name": group.display_name,
        "models": group.models.into_iter().map(compat_model_info).collect::<Vec<_>>(),
    })
}

async fn compat_llm_models(state: &ApiState, _args: CompatLlmModels) -> Result<Value, CompatError> {
    let groups = state
        .host
        .provider_directory()
        .into_iter()
        .filter(|entry| entry.active)
        .flat_map(|entry| state.host.model_groups(&entry.route.id))
        .map(compat_model_group)
        .collect::<Vec<_>>();
    Ok(json!({"groups": groups, "failures": []}))
}
fn compat_discovery_error(code: &str, message: &str) -> CompatError {
    CompatError {
        code: code.into(),
        message: message.into(),
        details: json!({}),
    }
}

fn discovery_url(base_url: &str) -> Result<Url, CompatError> {
    let mut base = Url::parse(base_url.trim())
        .map_err(|_| compat_discovery_error("bad-request", "baseURL is invalid"))?;
    if !matches!(base.scheme(), "http" | "https")
        || base.username() != ""
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(compat_discovery_error(
            "bad-request",
            "baseURL must be an HTTP(S) URL without userinfo, query, or fragment",
        ));
    }
    let path = format!("{}/models", base.path().trim_end_matches('/'));
    base.set_path(&path);
    Ok(base)
}

fn discovery_value(value: Option<u64>) -> Option<Value> {
    value
        .filter(|value| *value > 0 && *value <= MAX_SAFE_INTEGER)
        .map(Value::from)
}

async fn compat_discover_models(
    state: &ApiState,
    args: CompatDiscoverModels,
) -> Result<Value, CompatError> {
    if args.settings_ns != LLM_PI_AI_NAMESPACE {
        return Err(compat_discovery_error(
            "discovery-unsupported",
            "model discovery is only supported for llm-pi-ai",
        ));
    }
    if let Some(api) = args.api.as_deref() {
        if api != "openai-responses" {
            return Err(compat_discovery_error(
                "discovery-unsupported",
                "model discovery only supports openai-responses",
            ));
        }
    }

    let entry = args.provider.as_deref().and_then(|provider| {
        state
            .host
            .provider_directory()
            .into_iter()
            .find(|entry| entry.route.id == provider)
    });
    let base_override = args.base_url.is_some();
    let base_url = args
        .base_url
        .or_else(|| entry.as_ref().map(|entry| entry.route.base_url.clone()))
        .ok_or_else(|| compat_discovery_error("bad-request", "baseURL is required"))?;
    let url = discovery_url(&base_url)?;
    let route_matches = entry.as_ref().is_some_and(|entry| {
        discovery_url(&entry.route.base_url).is_ok_and(|registered| registered == url)
    });

    // A typed draft key always wins over a stored credential. The value never
    // enters an error, response, or diagnostic path.
    let api_key = if let Some(api_key) = args.api_key {
        if api_key.trim().is_empty() {
            return Err(compat_discovery_error(
                "credential-rejected",
                "apiKey must not be blank",
            ));
        }
        Some(api_key)
    } else if !base_override || route_matches {
        if let (Some(entry), Some(credentials)) = (entry.as_ref(), state.host.credentials()) {
            if entry.route.credential_ref.is_empty() {
                None
            } else {
                let reference =
                    CredentialRef::new(entry.route.credential_ref.clone()).map_err(|_| {
                        compat_discovery_error("credential-rejected", "credential is unavailable")
                    })?;
                credentials.resolve(&reference).await.map_err(|_| {
                    compat_discovery_error("credential-rejected", "credential is unavailable")
                })?
            }
        } else {
            None
        }
    } else {
        None
    };

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .connect_timeout(DISCOVERY_TIMEOUT)
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .map_err(|_| compat_discovery_error("model-discovery-failed", "provider request failed"))?;
    let mut request = client.get(url);
    if let Some(api_key) = api_key {
        request = request.header(reqwest::header::AUTHORIZATION, format!("Bearer {api_key}"));
    }
    let response = tokio::time::timeout(DISCOVERY_TIMEOUT, request.send())
        .await
        .map_err(|_| {
            compat_discovery_error("model-discovery-failed", "provider request timed out")
        })?
        .map_err(|_| compat_discovery_error("model-discovery-failed", "provider request failed"))?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(compat_discovery_error(
            "credential-rejected",
            "provider rejected the credential",
        ));
    }
    if !status.is_success() {
        return Err(compat_discovery_error(
            "model-discovery-failed",
            "provider returned an unsuccessful response",
        ));
    }
    let body = tokio::time::timeout(DISCOVERY_TIMEOUT, async {
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| ())?;
            if body.len().saturating_add(chunk.len()) > MAX_DISCOVERY_BYTES {
                return Err(());
            }
            body.extend_from_slice(&chunk);
        }
        Ok::<_, ()>(body)
    })
    .await
    .map_err(|_| compat_discovery_error("model-discovery-failed", "provider response timed out"))?
    .map_err(|_| {
        compat_discovery_error("model-discovery-failed", "provider response is too large")
    })?;
    let response: CompatDiscoveryResponse = serde_json::from_slice(&body).map_err(|_| {
        compat_discovery_error(
            "model-discovery-failed",
            "provider returned an invalid model list",
        )
    })?;
    let mut seen = BTreeSet::new();
    let models = response
        .data
        .into_iter()
        .filter_map(|model| {
            if model.id.trim().is_empty() || !seen.insert(model.id.clone()) {
                return None;
            }
            let context_window = model.context_window;
            let max_tokens = [model.max_output_tokens, model.max_tokens]
                .into_iter()
                .flatten()
                .find(|value| *value > 0 && *value <= MAX_SAFE_INTEGER);
            let mut value = Map::from_iter([("id".into(), Value::String(model.id))]);
            if let Some(name) = model.name.filter(|name| !name.trim().is_empty()) {
                value.insert("name".into(), Value::String(name));
            }
            if let Some(context_window) = discovery_value(context_window) {
                value.insert("contextWindow".into(), context_window);
            }
            if let Some(max_tokens) = discovery_value(max_tokens) {
                value.insert("maxTokens".into(), max_tokens);
            }
            Some(Value::Object(value))
        })
        .collect::<Vec<_>>();
    Ok(json!({"models": models}))
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
                attachment,
            } => match (attachment, media_type, data) {
                (Some(attachment), None, None) => {
                    AttachmentRef::from_value(&attachment)
                        .map_err(|_| CompatError::invalid("image attachment is invalid"))?;
                    if name.is_some() {
                        return Err(CompatError::invalid(
                            "canonical image must not include inline fields",
                        ));
                    }
                    Ok(crate::protocol::ContentBlock::Image { attachment })
                }
                (None, Some(media_type), Some(data)) => {
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
                _ => Err(CompatError::invalid(
                    "image must be exactly inline mediaType/data or canonical attachment",
                )),
            },
        })
        .collect()
}

async fn compat_session_prompt(state: &ApiState, args: CompatPrompt) -> Result<Value, CompatError> {
    let CompatPrompt {
        session_id,
        mode,
        content,
        client_time_zone,
    } = args;
    compat_require_session(&session_id)?;
    let client_time_zone = client_time_zone.as_deref().map_or(Ok(None), |value| {
        canonical_client_time_zone(value)
            .map(Some)
            .ok_or_else(|| CompatError {
                code: "invalid-time-zone".into(),
                message: "clientTimeZone must be UTC or a valid IANA Area/Location name".into(),
                details: json!({"value": value}),
            })
    })?;
    let params = SessionPromptParams {
        session_id: session_id.clone(),
        content_blocks: compat_prompt_blocks(content)?,
        client_time_zone,
    };
    match mode {
        CompatPromptMode::Queue => state.host.prompt(params).await,
        CompatPromptMode::Steer => state.host.steer(params).await,
    }
    .map_err(|error| compat_session_rpc_error(error, &session_id))?;
    if let Some(session) = compat_data(&state.compat).sessions.get_mut(&session_id) {
        session.blank = false;
        session.updated_at = compat_updated_at();
    }
    Ok(json!({"accepted": true}))
}

async fn compat_session_update_queue(
    state: &ApiState,
    args: CompatUpdateQueue,
) -> Result<Value, CompatError> {
    compat_require_session(&args.session_id)?;
    compat_require_nonblank("itemId", args.item_id.as_str())?;
    let action = match args.action {
        CompatQueueAction::Edit { content } => SessionQueueAction::Edit { content },
        CompatQueueAction::Remove {} => SessionQueueAction::Remove,
        CompatQueueAction::Steer {} => SessionQueueAction::Steer,
    };
    let receipt = state
        .host
        .update_queue(SessionUpdateQueueParams {
            session_id: args.session_id,
            item_id: args.item_id,
            action,
        })
        .await
        .map_err(compat_host_error)?;
    Ok(json!({"accepted": receipt.accepted}))
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

async fn compat_session_search(
    state: &ApiState,
    args: CompatSessionSearch,
) -> Result<Value, CompatError> {
    let query = args.query.trim();
    if query.is_empty()
        || query.contains('\0')
        || query.encode_utf16().count() > SESSION_SEARCH_QUERY_MAX_CHARS
    {
        return Err(CompatError::invalid("query is invalid"));
    }
    let result = state
        .host
        .search_sessions(query.to_owned())
        .await
        .map_err(compat_host_error)?;
    Ok(json!({
        "items": result.items.into_iter().map(|item| json!({
            "sessionId": item.session_id,
            "snippet": item.snippet,
        })).collect::<Vec<_>>(),
        "hasMore": result.has_more,
    }))
}

async fn compat_session_rename(
    state: &ApiState,
    args: CompatSessionRename,
) -> Result<Value, CompatError> {
    compat_require_session(&args.session_id)?;
    if args.title.is_empty() {
        return Err(CompatError::invalid("title must not be empty"));
    }
    let renamed = state
        .host
        .rename_session(args.session_id.clone(), args.title)
        .await
        .map_err(|error| compat_session_rpc_error(error, &args.session_id))?;
    Ok(json!({"title": renamed.title, "seq": renamed.seq}))
}

async fn compat_session_fork(
    state: &ApiState,
    args: CompatSessionFork,
) -> Result<Value, CompatError> {
    compat_require_session(&args.session_id)?;
    if args
        .at_seq
        .is_some_and(|sequence| sequence > MAX_SAFE_INTEGER)
    {
        return Err(CompatError::invalid("atSeq exceeds the safe integer range"));
    }
    let session_id = state
        .host
        .fork_session(args.session_id.clone(), args.at_seq)
        .await
        .map_err(|error| compat_session_rpc_error(error, &args.session_id))?;
    if compat_sync_sessions(state).await.is_ok() {
        let session = compat_data(&state.compat)
            .sessions
            .get(&session_id)
            .cloned();
        if let Some(session) = session {
            let workspace_id = session.workspace_id.clone();
            let payload = compat_session_added_payload(&session);
            broadcast_compat(&state.compat, CompatStream::Host, payload);
            if let (Some(registry), Some(workspace_id)) =
                (state.host.workspace_registry(), workspace_id)
            {
                if let Some(workspace) = registry
                    .list()
                    .into_iter()
                    .find(|workspace| workspace.workspace_id == workspace_id)
                {
                    broadcast_compat(
                        &state.compat,
                        CompatStream::Host,
                        json!({"type": "host/workspace-changed", "workspace": workspace}),
                    );
                }
            }
        }
    }
    Ok(json!({"sessionId": session_id}))
}

fn compat_session_rpc_error(error: TessivumError, session_id: &SessionId) -> CompatError {
    match error.code.as_str() {
        "SESSION_NOT_FOUND" => CompatError {
            code: "session-not-found".into(),
            message: error.message,
            details: json!({"sessionId": session_id}),
        },
        "TITLE_INVALID" => CompatError {
            code: "title-invalid".into(),
            message: error.message,
            details: json!({"sessionId": session_id}),
        },
        "FORK_UNAVAILABLE" => CompatError {
            code: "fork-unavailable".into(),
            message: error.message,
            details: json!({"sessionId": session_id}),
        },
        "AGENT_BUSY" => CompatError {
            code: "agent-busy".into(),
            message: error.message,
            details: json!({"sessionId": session_id}),
        },
        "LLM_PROVIDER_NOT_FOUND" | "LLM_MODEL_NOT_FOUND" | "MISSING_CREDENTIAL" => CompatError {
            code: "model-unavailable".into(),
            message: error.message,
            details: json!({"sessionId": session_id}),
        },
        _ => compat_host_error(error),
    }
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
        if let Some(registry) = state.host.question_registry() {
            for requested in registry.snapshots() {
                if !replayed_approvals.insert(requested.rpc_id.clone()) {
                    continue;
                }
                let message = match compat_ws_message(
                    question_requested_payload(&requested),
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
                    let message = compat_ws_message(compat_stream_error_payload(format!("{dropped} compatibility frames were dropped")), None).expect("stream error frame is bounded");
                    let _ = compat_socket_send(&mut socket, message).await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            notification = notifications.recv() => match notification {
                Ok(notification) => {
                    if compat_notification_stream(&notification) != Some(stream_kind) {
                        continue;
                    }
                    let duplicate = match &notification {
                        HostNotification::ApprovalRequested(requested) => {
                            !replayed_approvals.insert(requested.rpc_id.clone())
                        }
                        HostNotification::QuestionRequested(requested) => {
                            !replayed_approvals.insert(requested.rpc_id.clone())
                        }
                        HostNotification::ApprovalResolved(resolved) => {
                            replayed_approvals.remove(&resolved.rpc_id);
                            false
                        }
                        HostNotification::QuestionResolved(resolved) => {
                            replayed_approvals.remove(&resolved.rpc_id);
                            false
                        }
                        _ => false,
                    };
                    if duplicate {
                        continue;
                    }
                    for frame in compat_notifications(&state, notification).await {
                        let message = match compat_ws_message(frame.payload, frame.rpc_id.as_deref()) {
                            Ok(message) => message,
                            Err(message) => compat_ws_message(compat_stream_error_payload(message), None)
                                .expect("stream error frame is bounded"),
                        };
                        if !compat_socket_send(&mut socket, message).await {
                            return;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    let message = compat_ws_message(compat_stream_error_payload(format!("{dropped} host notifications were dropped")), None).expect("stream error frame is bounded");
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

fn compat_notification_stream(notification: &HostNotification) -> Option<CompatStream> {
    match notification {
        HostNotification::SessionEvent(_)
        | HostNotification::SessionQueue(_)
        | HostNotification::SessionJobs(_)
        | HostNotification::ApprovalRequested(_)
        | HostNotification::ApprovalResolved(_)
        | HostNotification::QuestionRequested(_)
        | HostNotification::SessionProjection(_)
        | HostNotification::QuestionResolved(_) => Some(CompatStream::Mux),
        HostNotification::SessionStatus(_)
        | HostNotification::RemoteEvent(_)
        | HostNotification::SettingsChanged(_)
        | HostNotification::CredentialsChanged(_)
        | HostNotification::ModelsChanged
        | HostNotification::AdaptersUpdated
        | HostNotification::SubagentStarted(_) => Some(CompatStream::Host),
        HostNotification::SubagentFinished(_) => None,
    }
}

async fn compat_notifications(
    state: &ApiState,
    notification: HostNotification,
) -> Vec<CompatFrame> {
    match notification {
        HostNotification::SessionEvent(notification) => {
            let view =
                compat_live_tool_view(state, &notification.session_id, &notification.event).await;
            let mut payload = json!({
                "type": "session/event",
                "sessionId": notification.session_id,
                "event": notification.event,
            });
            if let Some(view) = view {
                payload["view"] = view;
            }
            let mut frames = vec![CompatFrame {
                stream: CompatStream::Mux,
                rpc_id: None,
                payload,
            }];
            frames.extend(compat_projection_frames(state, &notification).await);
            frames
        }
        HostNotification::SessionProjection(notification) => vec![CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "session/projection",
                "sessionId": notification.session_id,
                "key": notification.key,
                "value": notification.value,
                "seq": notification.seq,
            }),
        }],
        HostNotification::SessionStatus(notification) => {
            if let Some(session) = compat_data(&state.compat)
                .sessions
                .get_mut(&notification.session_id)
            {
                session.running = notification.status == crate::protocol::SessionStatus::Running;
                session.updated_at = compat_updated_at();
            }
            vec![CompatFrame {
                stream: CompatStream::Host,
                rpc_id: None,
                payload: json!({
                    "type": "host/session-status",
                    "sessionId": notification.session_id,
                    "running": notification.status == crate::protocol::SessionStatus::Running,
                }),
            }]
        }
        HostNotification::SessionQueue(notification) => vec![CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({"type": "session/queue", "sessionId": notification.session_id, "items": notification.items}),
        }],
        HostNotification::SessionJobs(notification) => vec![CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({"type": "session/jobs", "sessionId": notification.session_id, "jobs": notification.jobs}),
        }],
        HostNotification::ApprovalRequested(requested) => vec![CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: Some(requested.rpc_id.clone()),
            payload: approval_requested_payload(&requested),
        }],
        HostNotification::ApprovalResolved(resolved) => vec![CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "approval/resolved",
                "sessionId": resolved.session_id,
                "approvalId": resolved.approval_id,
                "outcome": resolved.outcome,
            }),
        }],
        HostNotification::QuestionRequested(requested) => vec![CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: Some(requested.rpc_id.clone()),
            payload: question_requested_payload(&requested),
        }],
        HostNotification::QuestionResolved(resolved) => vec![CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "question/resolved",
                "sessionId": resolved.session_id,
                "questionRpcId": resolved.rpc_id,
                "outcome": resolved.outcome,
            }),
        }],
        HostNotification::RemoteEvent(event) => vec![CompatFrame {
            stream: CompatStream::Host,
            rpc_id: None,
            payload: json!({"type": "host/remote-event", "event": event.event, "args": event.args}),
        }],
        HostNotification::SettingsChanged(event) => vec![CompatFrame {
            stream: CompatStream::Host,
            rpc_id: None,
            payload: json!({"type": "host/remote-event", "event": "settings/document-updated", "args": [event.namespace, event.revision]}),
        }],
        HostNotification::CredentialsChanged(event) => vec![CompatFrame {
            stream: CompatStream::Host,
            rpc_id: None,
            payload: json!({"type": "host/remote-event", "event": "credentials/updated", "args": [event.reference]}),
        }],
        HostNotification::ModelsChanged | HostNotification::AdaptersUpdated => vec![CompatFrame {
            stream: CompatStream::Host,
            rpc_id: None,
            payload: json!({"type": "host/remote-event", "event": "llm/adapters-updated", "args": []}),
        }],
        HostNotification::SubagentStarted(notification) => vec![CompatFrame {
            stream: CompatStream::Host,
            rpc_id: None,
            payload: json!({
                "type": "host/session-added",
                "sessionId": notification.child_session_id,
                "blank": false,
                "parentSessionId": notification.parent_session_id,
                "origin": "subagent",
            }),
        }],
        HostNotification::SubagentFinished(_) => Vec::new(),
    }
}

async fn compat_projection_frames(
    state: &ApiState,
    notification: &SessionEventNotification,
) -> Vec<CompatFrame> {
    let permission_changed = compat_permission_changed(&notification.event);
    let metrics_changed = compat_metrics_changed(&notification.event);
    let timing_boundary = matches!(
        notification.event.event_type.as_str(),
        "turn/start" | "turn/end" | "subagent/descriptor"
    );
    let mut frames = compat_projection_frame(state, notification)
        .into_iter()
        .collect();
    if !permission_changed && !metrics_changed && !timing_boundary {
        return frames;
    }
    let Ok(events) = state.host.events(notification.session_id.clone(), 0).await else {
        return frames;
    };
    let events = events
        .into_iter()
        .filter(|event| event.seq <= notification.event.seq)
        .collect::<Vec<_>>();
    let seq = notification.event.seq;
    let timing = compat_subagent_timing(&events);
    if timing != compat_subagent_timing(&events[..events.len().saturating_sub(1)]) {
        frames.push(CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "session/projection",
                "sessionId": notification.session_id,
                "key": "subagentTiming",
                "value": timing,
                "seq": seq,
            }),
        });
    }
    if permission_changed {
        frames.push(CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "session/projection",
                "sessionId": notification.session_id,
                "key": "permissions",
                "value": permission_select(&fold_permission_events(&events)),
                "seq": seq,
            }),
        });
    }
    if metrics_changed {
        frames.push(CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "session/projection",
                "sessionId": notification.session_id,
                "key": "sessionStats",
                "value": compat_session_stats(&events),
                "seq": seq,
            }),
        });
        if compat_token_usage_sample(&notification.event).is_some() {
            frames.push(CompatFrame {
                stream: CompatStream::Mux,
                rpc_id: None,
                payload: json!({
                    "type": "session/projection",
                    "sessionId": notification.session_id,
                    "key": "tokenUsage",
                    "value": compat_token_usage(&events),
                    "seq": seq,
                }),
            });
        }
    }
    if notification.event.event_type == "request/context"
        || compat_token_usage_sample(&notification.event).is_some()
    {
        frames.push(CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "session/projection",
                "sessionId": notification.session_id,
                "key": "contextPressure",
                "value": compat_context_pressure(&events),
                "seq": seq,
            }),
        });
    }
    frames
}

fn compat_metrics_changed(event: &SessionEvent) -> bool {
    matches!(
        event.event_type.as_str(),
        "step/start"
            | "assistant/chunk"
            | "assistant/message"
            | "tool/call"
            | "tool/result"
            | "step/end"
            | "turn/end"
    )
}

fn compat_permission_changed(event: &SessionEvent) -> bool {
    matches!(
        event.event_type.as_str(),
        "permission/preset" | "sandbox/mode" | "approval/policy"
    )
}

fn compat_projection_frame(
    state: &ApiState,
    notification: &SessionEventNotification,
) -> Option<CompatFrame> {
    if notification.event.event_type == "session/title" {
        let title = notification.event.data.get("title")?.as_str()?.to_owned();
        let projection = CompatSessionProjections {
            as_of_seq: notification.event.seq,
            values: BTreeMap::from([("title".to_owned(), Value::String(title.clone()))]),
        };
        if let Some(session) = compat_data(&state.compat)
            .sessions
            .get_mut(&notification.session_id)
        {
            session.projections = Some(projection);
        }
        return Some(CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "session/projection",
                "sessionId": notification.session_id,
                "key": "title",
                "value": title,
                "seq": notification.event.seq,
            }),
        });
    }
    if notification.event.event_type == "plan/mode" {
        let active = notification.event.data.get("active")?.as_bool()?;
        let value = json!({"active": active, "pending": false});
        return Some(CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "session/projection",
                "sessionId": notification.session_id,
                "key": "plan",
                "value": value,
                "seq": notification.event.seq,
            }),
        });
    }
    if matches!(
        notification.event.event_type.as_str(),
        "todo/write" | "turn/start"
    ) {
        let value = if notification.event.event_type == "todo/write" {
            notification.event.data.get("todos")?.clone()
        } else {
            Value::Null
        };
        return Some(CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "session/projection",
                "sessionId": notification.session_id,
                "key": "todos",
                "value": value,
                "seq": notification.event.seq,
            }),
        });
    }
    if let Some(goal) = compat_goal_projection_value(&notification.event) {
        return Some(CompatFrame {
            stream: CompatStream::Mux,
            rpc_id: None,
            payload: json!({
                "type": "session/projection",
                "sessionId": notification.session_id,
                "key": "goal",
                "value": goal,
                "seq": notification.event.seq,
            }),
        });
    }
    None
}

fn compat_goal_projection_value(event: &SessionEvent) -> Option<Value> {
    if event.event_type != "goal/change" {
        return None;
    }
    if event.data.get("operation").and_then(Value::as_str) == Some("clear") {
        return Some(Value::Null);
    }
    Some(json!({
        "goal": event.data.get("goal")?.clone(),
        "roundsStarted": event.data.get("roundsStarted")?.clone(),
        "createdAt": event.data.get("createdAt")?.clone(),
        "updatedAt": event.data.get("updatedAt")?.clone(),
    }))
}
fn compat_subagent_timing(events: &[SessionEvent]) -> Value {
    let mut settled_ms = 0_u64;
    let mut active = None::<(u64, u64)>;
    for event in events {
        match event.event_type.as_str() {
            "turn/start" => active = Some((event.time, event.time)),
            "subagent/descriptor" => {
                settled_ms = 0;
                active = active.map(|(since, _)| (since, event.time));
            }
            "turn/end" => {
                if let Some((since, _)) = active.take() {
                    settled_ms = settled_ms.saturating_add(event.time.saturating_sub(since));
                }
            }
            _ => {
                if let Some((_, through)) = active.as_mut() {
                    *through = event.time;
                }
            }
        }
    }
    match active {
        Some((since, through)) => {
            json!({"settledMs": settled_ms, "active": {"since": since, "through": through}})
        }
        None => json!({"settledMs": settled_ms}),
    }
}

fn compat_derived_projection_values(
    events: &[SessionEvent],
    attachment_limits: &AttachmentLimits,
) -> BTreeMap<String, Value> {
    let permission = fold_permission_events(events);
    let mut values = BTreeMap::from([
        (
            "imageLimits".to_owned(),
            json!({
                "maxImageBytes": attachment_limits.max_image_bytes,
                "maxImagesPerMessage": attachment_limits.max_images_per_message,
                "maxMessageImageBytes": attachment_limits.max_message_image_bytes,
                "maxImagePixels": attachment_limits.max_image_pixels,
                "mediaTypes": attachment_limits.media_types.iter().map(|media_type| media_type.as_str()).collect::<Vec<_>>(),
            }),
        ),
        ("permissions".to_owned(), permission_select(&permission)),
        ("sessionStats".to_owned(), compat_session_stats(events)),
        ("tokenUsage".to_owned(), compat_token_usage(events)),
        ("subagentTiming".to_owned(), compat_subagent_timing(events)),
        (
            "contextPressure".to_owned(),
            compat_context_pressure(events),
        ),
    ]);
    if let Some(projection) = compat_title_projection(events) {
        values.extend(projection.values);
    }
    if let Some(goal) = events.iter().rev().find_map(compat_goal_projection_value) {
        values.insert("goal".to_owned(), goal);
    }
    values
}

fn compat_title_projection(events: &[SessionEvent]) -> Option<CompatSessionProjections> {
    let mut values = BTreeMap::new();
    if let Some(title) = events.iter().rev().find_map(|event| {
        (event.event_type == "session/title")
            .then(|| event.data.get("title").and_then(Value::as_str))
            .flatten()
    }) {
        values.insert("title".to_owned(), Value::String(title.to_owned()));
    }
    let active = events
        .iter()
        .rev()
        .find_map(|event| {
            (event.event_type == "plan/mode")
                .then(|| event.data.get("active").and_then(Value::as_bool))
                .flatten()
        })
        .unwrap_or(false);
    values.insert(
        "plan".to_owned(),
        json!({"active": active, "pending": false}),
    );
    let mut todos = Value::Null;
    for event in events {
        match event.event_type.as_str() {
            "todo/write" => todos = event.data.get("todos").cloned().unwrap_or(Value::Null),
            "turn/start" => todos = Value::Null,
            _ => {}
        }
    }
    values.insert("todos".to_owned(), todos);
    if let Some(goal) = events.iter().rev().find_map(compat_goal_projection_value) {
        values.insert("goal".to_owned(), goal);
    }
    (!values.is_empty()).then(|| CompatSessionProjections {
        as_of_seq: events.last().map_or(0, |event| event.seq),
        values,
    })
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

fn question_requested_payload(requested: &crate::question::QuestionRequested) -> Value {
    json!({
        "type": "question/requested",
        "sessionId": requested.session_id,
        "questions": requested.questions,
    })
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
            | ("subagent", "history")
            | ("subagent", "prompt")
            | ("subagent", "interrupt")
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
                client_time_zone: args.client_time_zone,
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
        ("subagent", "history") => {
            let args: SubagentHistoryArgs = decode(args)?;
            require_session(&args.parent_session_id)?;
            require_session(&args.child_session_id)?;
            if let Some(before_seq) = args.before_seq {
                require_safe_integer("beforeSeq", before_seq)?;
            }
            let max_messages = args
                .max_messages
                .map(|max_messages| {
                    require_safe_integer("maxMessages", max_messages)?;
                    if max_messages == 0 {
                        return Err(ApiError::bad_request("maxMessages must be positive"));
                    }
                    usize::try_from(max_messages).map_err(|_| {
                        ApiError::bad_request("maxMessages exceeds the platform size limit")
                    })
                })
                .transpose()?;
            serializable(
                host.subagent_history(SubagentHistoryRequest {
                    parent_session_id: args.parent_session_id,
                    child_session_id: args.child_session_id,
                    mode: args.mode,
                    before_seq: args.before_seq,
                    max_messages,
                })
                .await
                .map_err(host_error)?,
            )
        }
        ("subagent", "prompt") => {
            let args: SubagentPromptArgs = decode(args)?;
            require_session(&args.parent_session_id)?;
            require_session(&args.child_session_id)?;
            let mode = match args.mode {
                ContinuableMode::Continuable => SubagentMode::Continuable,
            };
            for block in &args.content {
                block.validate().map_err(protocol_error)?;
            }
            let client_time_zone = args.client_time_zone.as_deref().map_or(Ok(None), |value| {
                canonical_client_time_zone(value).map(Some).ok_or_else(|| {
                    ApiError::bad_request(
                        "clientTimeZone must be UTC or a valid IANA Area/Location name",
                    )
                })
            })?;
            serializable(
                host.subagent_prompt(SubagentPromptRequest {
                    parent_session_id: args.parent_session_id,
                    child_session_id: args.child_session_id,
                    mode,
                    content: args.content,
                    client_time_zone,
                })
                .await
                .map_err(host_error)?,
            )
        }
        ("subagent", "interrupt") => {
            let args: SubagentInterruptArgs = decode(args)?;
            require_session(&args.parent_session_id)?;
            require_session(&args.child_session_id)?;
            let mode = match args.mode {
                ContinuableMode::Continuable => SubagentMode::Continuable,
            };
            serializable(
                host.subagent_interrupt(SubagentInterruptRequest {
                    parent_session_id: args.parent_session_id,
                    child_session_id: args.child_session_id,
                    mode,
                })
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
    #[serde(default)]
    client_time_zone: Option<String>,
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubagentHistoryArgs {
    parent_session_id: SessionId,
    child_session_id: SessionId,
    mode: SubagentMode,
    before_seq: Option<u64>,
    max_messages: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubagentPromptArgs {
    parent_session_id: SessionId,
    child_session_id: SessionId,
    mode: ContinuableMode,
    content: Vec<crate::protocol::ContentBlock>,
    client_time_zone: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubagentInterruptArgs {
    parent_session_id: SessionId,
    child_session_id: SessionId,
    mode: ContinuableMode,
}

#[derive(Deserialize)]
enum ContinuableMode {
    #[serde(rename = "continuable")]
    Continuable,
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
                    Ok(HostNotification::SessionProjection(notification)) if notification.session_id == session => {
                        match sse_event("session.projection", Some(notification.seq), &notification) {
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
                    Ok(HostNotification::SessionEvent(_))
                    | Ok(HostNotification::SessionProjection(_))
                    | Ok(HostNotification::SessionStatus(_))
                    | Ok(HostNotification::SessionQueue(_))
                    | Ok(HostNotification::SessionJobs(_))
                    | Ok(HostNotification::ApprovalRequested(_))
                    | Ok(HostNotification::ApprovalResolved(_))
                    | Ok(HostNotification::QuestionRequested(_))
                    | Ok(HostNotification::QuestionResolved(_))
                    | Ok(HostNotification::SettingsChanged(_))
                    | Ok(HostNotification::CredentialsChanged(_))
                    | Ok(HostNotification::ModelsChanged)
                    | Ok(HostNotification::AdaptersUpdated)
                    | Ok(HostNotification::RemoteEvent(_))
                    | Ok(HostNotification::SubagentStarted(_))
                    | Ok(HostNotification::SubagentFinished(_)) => {}
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
        client_time_zone: args.client_time_zone,
    };
    require_session(&params.session_id).ok()?;
    params.validate().ok()?;
    Some(params.session_id)
}

fn ws_notification(notification: &HostNotification) -> Result<WsMessage, ApiError> {
    let notification = match notification {
        HostNotification::SessionEvent(value) => json!({"kind": "session-event", "payload": value}),
        HostNotification::SessionProjection(value) => {
            json!({"kind": "session-projection", "payload": value})
        }
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
        HostNotification::QuestionRequested(value) => {
            json!({"kind": "question-requested", "payload": value})
        }
        HostNotification::QuestionResolved(value) => {
            json!({"kind": "question-resolved", "payload": value})
        }
        HostNotification::RemoteEvent(value) => {
            json!({"kind": "remote-event", "payload": value})
        }
        HostNotification::SettingsChanged(value) => {
            json!({"kind": "settings-changed", "payload": value})
        }
        HostNotification::CredentialsChanged(value) => {
            json!({"kind": "credentials-changed", "payload": value})
        }
        HostNotification::SessionQueue(value) => {
            json!({"kind": "session-queue", "payload": value})
        }
        HostNotification::SessionJobs(value) => {
            json!({"kind": "session-jobs", "payload": value})
        }
        HostNotification::ModelsChanged => json!({"kind": "models-changed", "payload": {}}),
        HostNotification::AdaptersUpdated => {
            json!({"kind": "adapters-updated", "payload": {}})
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn attachment_response_limit_handles_two_byte_base64_and_large_limits() {
        assert_eq!(base64_encode(&[0, 1]), "AAE=");
        assert_eq!(compat_attachment_response_limit(2), MAX_FRAME_BYTES + 4);
        let limit = compat_attachment_response_limit(MAX_FRAME_BYTES as u64);
        assert!(limit > MAX_FRAME_BYTES);
        assert_eq!(
            compat_attachment_response_limit(u64::MAX),
            MAX_PROMPT_FRAME_BYTES
        );

        let response = compat_response(
            json!("attachment"),
            json!({"ok": true, "value": {"data": "A".repeat(MAX_FRAME_BYTES)}}),
            limit,
        );
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), limit)
            .await
            .expect("bounded response body");
        assert!(body.len() > MAX_FRAME_BYTES);
    }

    #[test]
    fn history_pagination_counts_messages_and_keeps_turn_boundary() {
        let event = |seq, event_type: &str, source_event_seqs, surface_op| SessionEvent {
            event_type: event_type.into(),
            seq,
            time: 0,
            data: json!({}),
            ignorable: None,
            source_event_seqs,
            surface_op,
        };
        let events = vec![
            event(0, "session/header", None, None),
            event(1, "user/message", None, Some(SurfaceOp::Append)),
            event(2, "assistant/chunk", None, None),
            event(
                3,
                "assistant/message",
                Some(vec![2]),
                Some(SurfaceOp::Append),
            ),
            event(4, "turn/end", None, None),
            event(5, "turn/start", None, None),
            event(6, "user/message", None, Some(SurfaceOp::Append)),
            event(7, "assistant/chunk", None, None),
            event(
                8,
                "assistant/message",
                Some(vec![7]),
                Some(SurfaceOp::Append),
            ),
            event(9, "turn/end", None, None),
        ];

        let (page, has_more) = compat_paginate_history(events, 1);

        assert!(has_more);
        assert_eq!(
            page.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![5, 6, 7, 8, 9]
        );
    }

    #[test]
    fn feedback_put_requires_present_nullable_version() {
        let base = json!({
            "sessionId": "session",
            "messageId": "message",
            "rating": "positive",
            "note": null,
        });
        assert!(serde_json::from_value::<CompatMessageFeedbackPut>(base.clone()).is_err());

        let mut with_null = base;
        with_null["ifVersion"] = Value::Null;
        assert!(serde_json::from_value::<CompatMessageFeedbackPut>(with_null).is_ok());
    }

    #[test]
    fn terminal_view_prefers_signal_metadata_to_exit_code() {
        let call = CompatToolCall {
            name: "bash".into(),
            arguments: json!({"command": "kill -TERM $$"}).to_string(),
        };
        let event = SessionEvent {
            event_type: "tool/result".into(),
            seq: 0,
            time: 0,
            data: json!({
                "meta": {"exitCode": 143, "signal": "SIGTERM"},
                "message": {
                    "content": [{
                        "type": "tool-result",
                        "content": [{"type": "text", "text": "(no output)"}],
                    }],
                },
            }),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
        };

        assert_eq!(
            compat_present_tool_result(&call, &event),
            Some(json!({"card": "terminal", "output": "(no output)", "signal": "SIGTERM"})),
        );
    }
}

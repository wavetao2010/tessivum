use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tessivum::{
    api::{ApiServer, MAX_FRAME_BYTES},
    host::{HostApi, HostNotification},
    protocol::{
        AgentCancelCause, InitializeParams, InitializeResult, MessageId, SdkServerInfo,
        SessionEvent, SessionEventNotification, SessionId, SessionPromptParams,
        SessionPromptResult, SessionStatus,
    },
    TessivumError,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{broadcast, Notify},
    time::timeout,
};

struct FakeHost {
    events: Mutex<BTreeMap<SessionId, Vec<SessionEvent>>>,
    statuses: Mutex<BTreeMap<SessionId, SessionStatus>>,
    notifications: broadcast::Sender<HostNotification>,
    initializations: Mutex<Vec<InitializeParams>>,
    prompt_params: Mutex<Vec<SessionPromptParams>>,
    prompts: AtomicUsize,
    steers: AtomicUsize,
    cancels: AtomicUsize,
    shutdown: AtomicBool,
    delay_prompt: AtomicBool,
    prompt_started: Notify,
    cancel_seen: Notify,
}

impl FakeHost {
    fn new() -> Self {
        let (notifications, _) = broadcast::channel(16);
        Self {
            events: Mutex::new(BTreeMap::new()),
            statuses: Mutex::new(BTreeMap::new()),
            notifications,
            initializations: Mutex::new(Vec::new()),
            prompt_params: Mutex::new(Vec::new()),
            prompts: AtomicUsize::new(0),
            steers: AtomicUsize::new(0),
            cancels: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            delay_prompt: AtomicBool::new(false),
            prompt_started: Notify::new(),
            cancel_seen: Notify::new(),
        }
    }

    fn add_event(&self, session: impl Into<SessionId>, event: SessionEvent) {
        let session = session.into();
        self.events
            .lock()
            .expect("event lock")
            .entry(session.clone())
            .or_default()
            .push(event.clone());
        let _ = self
            .notifications
            .send(HostNotification::SessionEvent(SessionEventNotification {
                session_id: session,
                event,
            }));
    }
}

#[async_trait]
impl HostApi for FakeHost {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult, TessivumError> {
        self.initializations
            .lock()
            .expect("initialization lock")
            .push(params);
        Ok(InitializeResult {
            server_info: SdkServerInfo {
                name: "api-test".into(),
                version: "1".into(),
            },
        })
    }

    async fn prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.prompt_params.lock().expect("prompt lock").push(params);
        self.prompts.fetch_add(1, Ordering::SeqCst);
        self.prompt_started.notify_one();
        if self.delay_prompt.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(SessionPromptResult {
            message_id: MessageId::from("queued-message"),
        })
    }

    async fn steer(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.prompt_params.lock().expect("prompt lock").push(params);
        self.steers.fetch_add(1, Ordering::SeqCst);
        Ok(SessionPromptResult {
            message_id: MessageId::from("steered-message"),
        })
    }

    async fn cancel(
        &self,
        _session: SessionId,
        _cause: AgentCancelCause,
    ) -> Result<bool, TessivumError> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        self.cancel_seen.notify_one();
        Ok(true)
    }

    async fn events(
        &self,
        session: SessionId,
        from_seq: u64,
    ) -> Result<Vec<SessionEvent>, TessivumError> {
        Ok(self
            .events
            .lock()
            .expect("event lock")
            .get(&session)
            .into_iter()
            .flatten()
            .filter(|event| event.seq >= from_seq)
            .cloned()
            .collect())
    }

    async fn status(&self, session: SessionId) -> Result<Option<SessionStatus>, TessivumError> {
        Ok(self
            .statuses
            .lock()
            .expect("status lock")
            .get(&session)
            .copied())
    }

    fn subscribe(&self) -> broadcast::Receiver<HostNotification> {
        self.notifications.subscribe()
    }

    async fn shutdown(&self) -> Result<(), TessivumError> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn event(seq: u64) -> SessionEvent {
    SessionEvent {
        event_type: "turn/start".into(),
        seq,
        time: seq,
        data: json!({"turn": seq}),
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
    }
}

async fn start() -> (ApiServer, Arc<FakeHost>, String) {
    let host = Arc::new(FakeHost::new());
    let server = ApiServer::bind(host.clone()).await.expect("server binds");
    let base = format!("http://{}", server.local_addr());
    (server, host, base)
}

#[tokio::test]
async fn api_rejects_non_loopback_binds_without_authentication() {
    let host: Arc<dyn HostApi> = Arc::new(FakeHost::new());
    let error = match ApiServer::bind_at(host, "0.0.0.0:0".parse().unwrap()).await {
        Ok(mut server) => {
            server.shutdown().await.unwrap();
            panic!("non-loopback bind unexpectedly succeeded");
        }
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn websocket_rejects_a_cross_origin_browser_handshake() {
    let (mut server, _host, _base) = start().await;
    let address = server.local_addr();
    let mut stream = BufReader::new(TcpStream::connect(address).await.unwrap());
    stream
        .get_mut()
        .write_all(
            format!(
                "GET /ws HTTP/1.1\r\nHost: {address}\r\nOrigin: http://evil.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.get_mut().flush().await.unwrap();
    let mut status = String::new();
    stream.read_line(&mut status).await.unwrap();
    assert!(status.contains("403"), "cross-origin handshake: {status}");
    server.shutdown().await.unwrap();
}

async fn browser_call(
    client: &reqwest::Client,
    base: &str,
    rpc_id: &str,
    method: &str,
    payload: Value,
) -> Value {
    let response = client
        .post(format!("{base}/api/{method}"))
        .json(&json!({
            "type": "client-request",
            "rpcId": rpc_id,
            "method": method,
            "payload": payload,
        }))
        .send()
        .await
        .expect("browser RPC response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    response.json().await.expect("browser RPC JSON")
}

#[tokio::test]
async fn browser_rpc_creates_a_session_and_maps_prompt_content() {
    let (mut server, host, base) = start().await;
    let client = reqwest::Client::new();

    let described = browser_call(&client, &base, "describe", "host.describe", json!({})).await;
    assert_eq!(described["type"], "server-response");
    assert_eq!(described["rpcId"], "describe");
    assert_eq!(described["result"]["ok"], true);
    let cwd = described["result"]["value"]["cwd"]
        .as_str()
        .expect("canonical cwd")
        .to_owned();

    let created = browser_call(
        &client,
        &base,
        "create",
        "session.create",
        json!({"sessionId": "browser-session"}),
    )
    .await;
    assert_eq!(created["result"]["value"]["sessionId"], "browser-session");
    {
        let initialized = host.initializations.lock().expect("initialization lock");
        assert_eq!(initialized.len(), 1, "creation initializes lazily once");
        assert_eq!(initialized[0].cwd, cwd);
        assert_eq!(initialized[0].provider, "recorded");
        assert_eq!(initialized[0].model, "recorded");
    }

    let prompted = browser_call(
        &client,
        &base,
        "prompt",
        "session.prompt",
        json!({
            "sessionId": "browser-session",
            "mode": "queue",
            "content": [
                {"type": "text", "text": "hello"},
                {
                    "type": "image",
                    "mediaType": "image/png",
                    "data": "AA==",
                    "name": "pixel.png"
                }
            ]
        }),
    )
    .await;
    assert_eq!(prompted["result"]["value"]["accepted"], true);
    {
        let params = host.prompt_params.lock().expect("prompt lock");
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].session_id.as_str(), "browser-session");
        let content = serde_json::to_value(&params[0].content_blocks).expect("content serializes");
        assert_eq!(content[0], json!({"type": "text", "text": "hello"}));
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["attachment"]["mediaType"], "image/png");
        assert_eq!(content[1]["attachment"]["data"], "AA==");
        assert_eq!(content[1]["attachment"]["name"], "pixel.png");
    }
    let steered = browser_call(
        &client,
        &base,
        "steer",
        "session.prompt",
        json!({
            "sessionId": "browser-session",
            "mode": "steer",
            "content": [{"type": "text", "text": "now"}]
        }),
    )
    .await;
    assert_eq!(steered["result"]["value"]["accepted"], true);
    assert_eq!(host.steers.load(Ordering::SeqCst), 1);

    server.shutdown().await.expect("server shuts down");
}

#[tokio::test]
async fn browser_rpc_rejects_a_body_method_that_differs_from_its_url() {
    let (mut server, _host, base) = start().await;
    let response = reqwest::Client::new()
        .post(format!("{base}/api/host.describe"))
        .json(&json!({
            "type": "client-request",
            "rpcId": "mismatch",
            "method": "settings.describe",
            "payload": {},
        }))
        .send()
        .await
        .expect("mismatch response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response: Value = response.json().await.expect("mismatch JSON");
    assert_eq!(response["type"], "server-response");
    assert_eq!(response["rpcId"], "mismatch");
    assert_eq!(response["result"]["ok"], false);
    assert_eq!(response["result"]["error"]["code"], "bad-request");

    server.shutdown().await.expect("server shuts down");
}

#[tokio::test]
async fn browser_host_websocket_wraps_compatibility_frames_as_server_requests() {
    let (mut server, _host, base) = start().await;
    let client = reqwest::Client::new();
    let mut socket = RawWebSocket::connect_path(server.local_addr(), "/api/events.host").await;

    let _ = browser_call(
        &client,
        &base,
        "ws-create",
        "session.create",
        json!({"sessionId": "ws-session"}),
    )
    .await;
    let frame = timeout(Duration::from_secs(1), async {
        loop {
            let text = socket.read_text().await.expect("compatibility frame");
            let frame: Value = serde_json::from_str(&text).expect("server request frame");
            if frame["payload"]["type"] == "host/session-added" {
                return frame;
            }
        }
    })
    .await
    .expect("session-added WebSocket frame arrives");
    assert_eq!(frame["type"], "server-request");
    assert_eq!(frame["method"], frame["payload"]["type"]);
    assert_eq!(frame["payload"]["sessionId"], "ws-session");

    server.shutdown().await.expect("server shuts down");
}

#[tokio::test]
async fn unary_routes_are_allowlisted_strict_and_stably_enveloped() {
    let (mut server, host, base) = start().await;
    let client = reqwest::Client::new();

    let initialized: Value = client
        .post(format!("{base}/api/session/initialize"))
        .json(&json!({
            "requestId": "initialize-1",
            "args": {"cwd": "/tmp", "provider": "test", "model": "test", "maxTokens": 8}
        }))
        .send()
        .await
        .expect("initialize response")
        .json()
        .await
        .expect("initialize JSON");
    assert_eq!(initialized["requestId"], "initialize-1");
    assert_eq!(initialized["ok"], true);
    assert_eq!(initialized["output"]["serverInfo"]["name"], "api-test");

    let extra = client
        .post(format!("{base}/api/session/initialize"))
        .json(&json!({
            "requestId": "strict-1",
            "args": {"cwd": "/tmp", "provider": "test", "model": "test", "extra": true}
        }))
        .send()
        .await
        .expect("strict response");
    assert_eq!(extra.status(), reqwest::StatusCode::BAD_REQUEST);
    let extra: Value = extra.json().await.expect("strict JSON");
    assert_eq!(extra["requestId"], "strict-1");
    assert_eq!(extra["ok"], false);
    assert_eq!(extra["error"]["code"], "INVALID_REQUEST");

    let unknown = client
        .post(format!("{base}/api/session/unknown"))
        .json(&json!({"requestId": "unknown-1", "args": {}}))
        .send()
        .await
        .expect("unknown response");
    assert_eq!(unknown.status(), reqwest::StatusCode::NOT_FOUND);
    let unknown: Value = unknown.json().await.expect("unknown JSON");
    assert_eq!(unknown["requestId"], "unknown-1");
    assert_eq!(unknown["error"]["code"], "METHOD_NOT_FOUND");

    let malformed = client
        .post(format!("{base}/api/session/status"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{")
        .send()
        .await
        .expect("malformed response");
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        malformed.json::<Value>().await.expect("malformed JSON")["ok"],
        false
    );

    let oversized = client
        .post(format!("{base}/api/session/status"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(vec![b'x'; MAX_FRAME_BYTES + 1])
        .send()
        .await
        .expect("oversized response");
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        oversized.json::<Value>().await.expect("oversized JSON")["error"]["code"],
        "PAYLOAD_TOO_LARGE"
    );

    let method = client
        .get(format!("{base}/api/session/status"))
        .send()
        .await
        .expect("method response");
    assert_eq!(method.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);

    let shutdown: Value = client
        .post(format!("{base}/api/host/shutdown"))
        .json(&json!({"requestId": "shutdown-1", "args": {}}))
        .send()
        .await
        .expect("shutdown response")
        .json()
        .await
        .expect("shutdown JSON");
    assert_eq!(shutdown["ok"], true);
    assert!(host.shutdown.load(Ordering::SeqCst));

    server.shutdown().await.expect("server shuts down");
    assert!(client.get(base).send().await.is_err(), "listener is closed");
}

#[tokio::test]
async fn sse_replays_then_delivers_live_events_and_reconnects_from_last_id() {
    let (mut server, host, base) = start().await;
    host.add_event("sse-session", event(0));
    host.add_event("sse-session", event(1));
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/events/sse-session?from=0"))
        .send()
        .await
        .expect("SSE opens");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let mut stream = response.bytes_stream();
    let mut replay = String::new();
    while !replay.contains("\"seq\":1") {
        let chunk = timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("replay arrives")
            .expect("replay chunk")
            .expect("replay bytes");
        replay.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(
        replay.contains("\"seq\":0"),
        "first durable event is replayed"
    );

    host.add_event("sse-session", event(2));
    let mut live = String::new();
    while !live.contains("\"seq\":2") {
        let chunk = timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("live event arrives")
            .expect("live chunk")
            .expect("live bytes");
        live.push_str(&String::from_utf8_lossy(&chunk));
    }
    drop(stream);

    let response = client
        .get(format!("{base}/events/sse-session"))
        .header("Last-Event-ID", "1")
        .send()
        .await
        .expect("SSE reconnects");
    let mut stream = response.bytes_stream();
    let suffix = timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("durable suffix arrives")
        .expect("suffix chunk")
        .expect("suffix bytes");
    let suffix = String::from_utf8_lossy(&suffix);
    assert!(
        suffix.contains("\"seq\":2"),
        "reconnect resumes after last event ID"
    );
    assert!(
        !suffix.contains("\"seq\":0"),
        "reconnect does not replay older events"
    );

    server.shutdown().await.expect("server shuts down");
}

#[tokio::test]
async fn websocket_streams_notifications_cancels_inflight_prompt_and_exits_on_shutdown() {
    let (mut server, host, _base) = start().await;
    let mut socket = RawWebSocket::connect(server.local_addr()).await;
    let first_started = host.prompt_started.notified();
    socket
        .send_json(json!({
            "requestId": "ws-prompt",
            "namespace": "session",
            "method": "prompt",
            "args": {"sessionId": "ws-session", "contentBlocks": [{"type": "text", "text": "hello"}]}
        }))
        .await;
    let response = socket.read_text().await.expect("WS response");
    assert_eq!(
        serde_json::from_str::<Value>(&response).expect("response JSON")["ok"],
        true
    );
    timeout(Duration::from_secs(1), first_started)
        .await
        .expect("first prompt starts");

    host.add_event("ws-session", event(0));
    let notification = socket.read_text().await.expect("WS notification");
    let notification: Value = serde_json::from_str(&notification).expect("notification JSON");
    assert_eq!(notification["type"], "notification");

    host.delay_prompt.store(true, Ordering::SeqCst);
    let mut cancelling = RawWebSocket::connect(server.local_addr()).await;
    let started = host.prompt_started.notified();
    cancelling
        .send_json(json!({
            "requestId": "ws-cancel",
            "namespace": "session",
            "method": "prompt",
            "args": {"sessionId": "cancel-session", "contentBlocks": [{"type": "text", "text": "cancel"}]}
        }))
        .await;
    timeout(Duration::from_secs(1), started)
        .await
        .expect("prompt starts");
    let cancelled = host.cancel_seen.notified();
    drop(cancelling);
    timeout(Duration::from_secs(1), cancelled)
        .await
        .expect("disconnect cancels in-flight prompt");
    assert_eq!(host.cancels.load(Ordering::SeqCst), 1);

    server
        .shutdown()
        .await
        .expect("server shutdown exits WebSocket");
    let closed = timeout(Duration::from_secs(1), socket.read_text()).await;
    assert!(
        matches!(closed, Ok(None) | Err(_)),
        "socket exits with listener shutdown"
    );
}

struct RawWebSocket {
    stream: BufReader<TcpStream>,
}

impl RawWebSocket {
    async fn connect(address: SocketAddr) -> Self {
        Self::connect_path(address, "/ws").await
    }

    async fn connect_path(address: SocketAddr, path: &str) -> Self {
        let stream = TcpStream::connect(address)
            .await
            .expect("WebSocket TCP connects");
        let mut stream = BufReader::new(stream);
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream
            .get_mut()
            .write_all(request.as_bytes())
            .await
            .expect("WebSocket handshake writes");
        stream
            .get_mut()
            .flush()
            .await
            .expect("WebSocket handshake flushes");
        let mut headers = Vec::new();
        stream
            .read_until(b'\n', &mut headers)
            .await
            .expect("WebSocket status reads");
        assert!(String::from_utf8_lossy(&headers).contains("101"));
        loop {
            headers.clear();
            stream
                .read_until(b'\n', &mut headers)
                .await
                .expect("WebSocket header reads");
            if headers == b"\r\n" {
                break;
            }
        }
        Self { stream }
    }

    async fn send_json(&mut self, value: Value) {
        let payload = serde_json::to_vec(&value).expect("request serializes");
        let mut frame = vec![0x81];
        match payload.len() {
            length @ 0..=125 => frame.push(0x80 | length as u8),
            length @ 126..=65_535 => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(length as u16).to_be_bytes());
            }
            length => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(length as u64).to_be_bytes());
            }
        }
        let mask = [0x11, 0x22, 0x33, 0x44];
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % mask.len()]),
        );
        self.stream
            .get_mut()
            .write_all(&frame)
            .await
            .expect("WebSocket request writes");
        self.stream
            .get_mut()
            .flush()
            .await
            .expect("WebSocket request flushes");
    }

    async fn read_text(&mut self) -> Option<String> {
        let first = match self.stream.read_u8().await {
            Ok(byte) => byte,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return None,
            Err(error) => panic!("WebSocket frame header: {error}"),
        };
        let second = self
            .stream
            .read_u8()
            .await
            .expect("WebSocket length header");
        assert_eq!(second & 0x80, 0, "server frames are not masked");
        let length = match second & 0x7f {
            value @ 0..=125 => value as usize,
            126 => self.stream.read_u16().await.expect("extended length") as usize,
            127 => self.stream.read_u64().await.expect("extended length") as usize,
            _ => unreachable!("a masked length field is at most 127"),
        };
        let mut payload = vec![0; length];
        self.stream
            .read_exact(&mut payload)
            .await
            .expect("WebSocket frame payload");
        match first & 0x0f {
            0x1 => Some(String::from_utf8(payload).expect("text payload")),
            0x8 => None,
            opcode => panic!("unexpected WebSocket opcode {opcode}"),
        }
    }
}

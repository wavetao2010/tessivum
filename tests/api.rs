use std::{
    collections::BTreeMap,
    fs, io,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use parking_lot::Mutex as ParkingMutex;
use serde_json::{json, Value};
#[cfg(unix)]
use tessivum::{
    agent::AgentRegistry,
    api::ApiServerConfig,
    bridge::{BridgeServices, DomainBridge},
    llm::LlmRuntime,
    session::{MemorySessionPersistence, SessionStore},
    system_prompt::SystemPrompt,
    tools::ToolRuntime,
};
use tessivum::{
    agent_mode::AgentModeTrust,
    api::{ApiServer, MAX_FRAME_BYTES},
    approval::{ApprovalId, ApprovalOutcome, ApprovalRequested, ApprovalResolved},
    host::{
        HostApi, HostConfig, HostLlmAdapterFactory, HostModelGroup, HostModelInfo,
        HostModelReasoning, HostModelReasoningEffort, HostNotification, HostPathOpener,
        HostProviderDirectoryEntry, HostProviderEnabled, HostRuntime, HostSessionModels,
        HostSessionQueueItem, HostSessionQueueNotification, HostSessionRenameResult,
        HostSessionSearchHit, HostSessionSearchResult, SessionQueueAction,
        SessionUpdateQueueParams, SessionUpdateQueueResult,
    },
    llm::{LlmAdapter, LlmStream},
    openai_responses::{ResponsesModel, ResponsesRoute},
    protocol::{
        AgentCancelCause, ContentBlock, FinishReason, GenerateRequest, InitializeParams,
        InitializeResult, MessageId, SdkServerInfo, SessionEvent, SessionEventNotification,
        SessionId, SessionModelSelection, SessionPromptParams, SessionPromptResult, SessionStatus,
        StreamChunk,
    },
    settings::{MemorySettingsProvider, Settings, SettingsRegistration},
    subagent::{
        SessionProjectionsBlock, SubagentDeleteRequest, SubagentDeleteResult,
        SubagentHistoryRequest, SubagentHistoryResult, SubagentInterruptRequest,
        SubagentInterruptResult, SubagentMode, SubagentPromptRequest, SubagentPromptResult,
    },
    TessivumError,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{broadcast, Notify},
    time::timeout,
};
use uuid::Uuid;

struct FakeHost {
    events: Mutex<BTreeMap<SessionId, Vec<SessionEvent>>>,
    statuses: Mutex<BTreeMap<SessionId, SessionStatus>>,
    notifications: broadcast::Sender<HostNotification>,
    initializations: Mutex<Vec<InitializeParams>>,
    prompt_params: Mutex<Vec<SessionPromptParams>>,
    subagent_history_params: ParkingMutex<Vec<SubagentHistoryRequest>>,
    subagent_prompt_params: ParkingMutex<Vec<SubagentPromptRequest>>,
    subagent_interrupt_params: ParkingMutex<Vec<SubagentInterruptRequest>>,
    subagent_error: ParkingMutex<Option<TessivumError>>,
    subagent_delete_params: ParkingMutex<Vec<SubagentDeleteRequest>>,
    prompts: AtomicUsize,
    steers: AtomicUsize,
    cancels: AtomicUsize,
    shutdown: AtomicBool,
    delay_prompt: AtomicBool,
    prompt_started: Notify,
    queue_updates: Mutex<Vec<SessionUpdateQueueParams>>,
    queue_error: Mutex<Option<TessivumError>>,
    provider_enable_available: AtomicBool,
    provider_enable_calls: Mutex<Vec<(String, bool)>>,
    desktop_enabled: AtomicBool,
    picked_directory: Mutex<Option<PathBuf>>,
    opened_paths: Mutex<Vec<String>>,
    settings_opens: AtomicUsize,
    settings: Arc<Settings>,
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
            queue_updates: Mutex::new(Vec::new()),
            subagent_history_params: ParkingMutex::new(Vec::new()),
            subagent_prompt_params: ParkingMutex::new(Vec::new()),
            subagent_interrupt_params: ParkingMutex::new(Vec::new()),
            subagent_error: ParkingMutex::new(None),
            subagent_delete_params: ParkingMutex::new(Vec::new()),
            queue_error: Mutex::new(None),
            provider_enable_available: AtomicBool::new(false),
            provider_enable_calls: Mutex::new(Vec::new()),
            desktop_enabled: AtomicBool::new(false),
            picked_directory: Mutex::new(None),
            opened_paths: Mutex::new(Vec::new()),
            settings_opens: AtomicUsize::new(0),
            settings: Arc::new(Settings::new(Arc::new(MemorySettingsProvider::new()))),
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
        Ok(true)
    }

    async fn update_queue(
        &self,
        params: SessionUpdateQueueParams,
    ) -> Result<SessionUpdateQueueResult, TessivumError> {
        if let Some(error) = self.queue_error.lock().expect("queue error lock").clone() {
            return Err(error);
        }
        self.queue_updates
            .lock()
            .expect("queue update lock")
            .push(params);
        Ok(SessionUpdateQueueResult { accepted: true })
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
    async fn subagent_history(
        &self,
        params: SubagentHistoryRequest,
    ) -> Result<SubagentHistoryResult, TessivumError> {
        if let Some(error) = self.subagent_error.lock().clone() {
            return Err(error);
        }
        self.subagent_history_params.lock().push(params);
        Ok(SubagentHistoryResult {
            events: Vec::new(),
            has_more: false,
            projections: Some(SessionProjectionsBlock {
                as_of_seq: -1,
                values: BTreeMap::new(),
            }),
        })
    }

    async fn subagent_prompt(
        &self,
        params: SubagentPromptRequest,
    ) -> Result<SubagentPromptResult, TessivumError> {
        if let Some(error) = self.subagent_error.lock().clone() {
            return Err(error);
        }
        self.subagent_prompt_params.lock().push(params);
        Ok(SubagentPromptResult {
            message_id: MessageId::from("subagent-message"),
        })
    }

    async fn subagent_interrupt(
        &self,
        params: SubagentInterruptRequest,
    ) -> Result<SubagentInterruptResult, TessivumError> {
        if let Some(error) = self.subagent_error.lock().clone() {
            return Err(error);
        }
        self.subagent_interrupt_params.lock().push(params);
        Ok(SubagentInterruptResult { accepted: true })
    }
    async fn subagent_delete(
        &self,
        params: SubagentDeleteRequest,
    ) -> Result<SubagentDeleteResult, TessivumError> {
        if let Some(error) = self.subagent_error.lock().clone() {
            return Err(error);
        }
        self.subagent_delete_params.lock().push(params);
        Ok(SubagentDeleteResult { deleted: true })
    }

    fn provider_directory(&self) -> Vec<HostProviderDirectoryEntry> {
        vec![HostProviderDirectoryEntry {
            route: ResponsesRoute::new(
                "fake-provider",
                "Fake Provider",
                "http://127.0.0.1:1/v1",
                "FAKE_API_KEY",
                vec![ResponsesModel::new("fake-model")],
            ),
            credential_configured: true,
            namespace: "llm-pi-ai".into(),
            settings_path: vec!["providers".into()],
            active: true,
            declared: true,
        }]
    }

    fn model_groups(&self, provider: &str) -> Vec<HostModelGroup> {
        if provider != "fake-provider" {
            return Vec::new();
        }
        vec![HostModelGroup {
            provider: provider.into(),
            display_name: "Fake Provider".into(),
            models: vec![HostModelInfo {
                provider: provider.into(),
                id: "fake-model".into(),
                name: Some("Fake Model".into()),
                description: Some("Fixture model".into()),
                input_modalities: vec!["text".into(), "image".into()],
                context_window: Some(4096),
                max_tokens: Some(512),
                reasoning: Some(HostModelReasoning {
                    efforts: vec![HostModelReasoningEffort {
                        id: "high".into(),
                        name: "High".into(),
                        description: None,
                    }],
                    default_effort: Some("high".into()),
                }),
                routable: true,
            }],
            credential_configured: true,
            routable: true,
            failure: None,
        }]
    }

    async fn session_models(
        &self,
        _session: SessionId,
    ) -> Result<HostSessionModels, TessivumError> {
        Ok(HostSessionModels {
            current: Some(SessionModelSelection {
                provider: "fake-provider".into(),
                model: "fake-model".into(),
                reasoning_effort: None,
            }),
            routable: true,
            groups: self.model_groups("fake-provider"),
            failures: Vec::new(),
        })
    }

    async fn select_model(
        &self,
        _session: SessionId,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Result<SessionModelSelection, TessivumError> {
        Ok(SessionModelSelection {
            provider,
            model,
            reasoning_effort,
        })
    }
    async fn set_provider_enabled(
        &self,
        provider: String,
        enabled: bool,
    ) -> Result<HostProviderEnabled, TessivumError> {
        if !self.provider_enable_available.load(Ordering::SeqCst) {
            return Err(TessivumError::new(
                "SETTINGS_UNAVAILABLE",
                "settings service is unavailable",
                "settings",
                json!({"namespace": "llm-pi-ai"}),
            ));
        }
        self.provider_enable_calls
            .lock()
            .expect("provider enable lock")
            .push((provider.clone(), enabled));
        Ok(HostProviderEnabled { provider, enabled })
    }
    async fn search_sessions(
        &self,
        query: String,
    ) -> Result<HostSessionSearchResult, TessivumError> {
        Ok(HostSessionSearchResult {
            items: vec![HostSessionSearchHit {
                session_id: SessionId::from("visible-session"),
                snippet: format!("matched {query}"),
            }],
            has_more: true,
        })
    }

    async fn rename_session(
        &self,
        _session_id: SessionId,
        title: String,
    ) -> Result<HostSessionRenameResult, TessivumError> {
        if title.trim().is_empty() {
            return Err(TessivumError::new(
                "TITLE_INVALID",
                "session title must contain visible text",
                "host",
                Value::Null,
            ));
        }
        Ok(HostSessionRenameResult { title, seq: 0 })
    }

    async fn fork_session(
        &self,
        session_id: SessionId,
        at_seq: Option<u64>,
    ) -> Result<SessionId, TessivumError> {
        if at_seq == Some(99) {
            return Err(TessivumError::new(
                "FORK_UNAVAILABLE",
                "session has no completed turn at the requested sequence",
                "host",
                Value::Null,
            ));
        }
        let _ = session_id;
        Ok(SessionId::from("forked-session"))
    }

    fn can_open_path(&self) -> bool {
        self.desktop_enabled.load(Ordering::SeqCst)
    }
    fn has_settings_document(&self) -> bool {
        self.desktop_enabled.load(Ordering::SeqCst)
    }
    fn settings(&self) -> Option<Arc<Settings>> {
        self.desktop_enabled
            .load(Ordering::SeqCst)
            .then(|| Arc::clone(&self.settings))
    }
    async fn pick_directory(&self) -> Result<Option<String>, TessivumError> {
        if !self.desktop_enabled.load(Ordering::SeqCst) {
            return Err(TessivumError::new(
                "DIRECTORY_PICKER_UNAVAILABLE",
                "native directory picker is unavailable",
                "host",
                json!({"capability": "absent"}),
            ));
        }
        Ok(self
            .picked_directory
            .lock()
            .expect("picked directory lock")
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()))
    }
    async fn open_path(&self, path: String) -> Result<(), TessivumError> {
        if !self.desktop_enabled.load(Ordering::SeqCst) {
            return Err(TessivumError::new(
                "PATH_OPENER_UNAVAILABLE",
                "native path opener is unavailable",
                "host",
                Value::Null,
            ));
        }
        self.opened_paths
            .lock()
            .expect("opened paths lock")
            .push(path);
        Ok(())
    }
    async fn open_settings_document(&self) -> Result<(), TessivumError> {
        if !self.desktop_enabled.load(Ordering::SeqCst) {
            return Err(TessivumError::new(
                "SETTINGS_DOCUMENT_UNAVAILABLE",
                "settings document opener is unavailable",
                "settings",
                Value::Null,
            ));
        }
        self.settings_opens.fetch_add(1, Ordering::SeqCst);
        Ok(())
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

struct RecordingNativeOpener(Mutex<Vec<PathBuf>>);

#[async_trait]
impl HostPathOpener for RecordingNativeOpener {
    fn can_open_path(&self) -> bool {
        true
    }
    async fn open_path(&self, path: PathBuf) -> Result<(), TessivumError> {
        self.0.lock().expect("native opener lock").push(path);
        Ok(())
    }
    async fn open_text_file(&self, path: PathBuf) -> Result<(), TessivumError> {
        self.0.lock().expect("native opener lock").push(path);
        Ok(())
    }
}
async fn raw_http_status(
    address: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    origins: &[&str],
    body: &str,
) -> String {
    let stream = TcpStream::connect(address)
        .await
        .expect("HTTP TCP connects");
    let mut stream = BufReader::new(stream);
    let origins = origins
        .iter()
        .map(|origin| format!("Origin: {origin}\r\n"))
        .collect::<String>();
    let content_type = if body.is_empty() {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\n{origins}{content_type}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .get_mut()
        .write_all(request.as_bytes())
        .await
        .expect("HTTP request writes");
    stream
        .get_mut()
        .flush()
        .await
        .expect("HTTP request flushes");
    let mut status = String::new();
    stream
        .read_line(&mut status)
        .await
        .expect("HTTP status reads");
    status
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn registered_market_route_forwards_through_the_bound_listener() {
    use std::{os::unix::net::UnixStream, sync::mpsc, thread};

    use tessivum_node_bridge::{
        BridgeClient, BridgeHandler, ClientConfig, Frame, FrameCodec, FrameKind,
    };

    let (rust, mut node) = UnixStream::pair().expect("duplex pair constructs");
    let client = BridgeClient::from_io(
        rust.try_clone().expect("reader clones"),
        rust,
        1,
        ClientConfig::default(),
    )
    .expect("client constructs");
    let (request_tx, request_rx) = mpsc::sync_channel(1);
    let peer = thread::spawn(move || {
        let codec = FrameCodec::new(1024 * 1024).expect("codec constructs");
        assert_eq!(
            codec.read_frame(&mut node).expect("hello reads").kind,
            FrameKind::Hello
        );
        codec
            .write_frame(
                &mut node,
                &Frame::new(
                    1,
                    FrameKind::Ready,
                    json!({"capabilities": ["web.route/v1"]}),
                ),
            )
            .expect("ready writes");
        let request = codec.read_frame(&mut node).expect("route request reads");
        assert_eq!(request.kind, FrameKind::WebRouteInvoke);
        request_tx.send(request.payload).expect("request reports");
        codec
            .write_frame(
                &mut node,
                &Frame::response(
                    1,
                    request.request_id.expect("route request is correlated"),
                    json!({"status": 201, "headers": [["content-type", "text/plain"]], "bodyBase64": "b2s="}),
                ),
            )
            .expect("route response writes");
    });
    client
        .handshake(Duration::from_secs(1))
        .expect("client becomes ready");
    let tools = ToolRuntime::new();
    let sessions = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let bridge = DomainBridge::new(BridgeServices::new(
        tools,
        SystemPrompt::new(),
        LlmRuntime::new(),
        sessions.clone(),
        AgentRegistry::new(sessions),
    ))
    .expect("bridge constructs");
    bridge.attach_client(client, 1).expect("bridge attaches");
    BridgeHandler::handle(
        &bridge,
        Frame::request(
            1,
            1,
            FrameKind::WebRouteRegister,
            json!({"routeId": "market", "kind": "exact", "path": "/dsh-market/market"}),
        ),
    )
    .expect("route registers");
    let host = Arc::new(FakeHost::new());
    let mut server =
        ApiServer::bind_with_web_routes(host, ApiServerConfig::default(), Vec::new(), Some(bridge))
            .await
            .expect("listener binds");
    let authority = server.local_addr().to_string();
    let origin = format!("http://{authority}");
    assert!(
        raw_http_status(
            server.local_addr(),
            "POST",
            "/dsh-market/market?page=1",
            &authority,
            &[&origin],
            "body",
        )
        .await
        .contains(" 201 "),
        "registered route must reach its Node owner"
    );
    let request = request_rx.recv().expect("Node receives forwarded request");
    assert_eq!(request["routeId"], "market");
    assert_eq!(request["path"], "/dsh-market/market");
    assert_eq!(request["query"], "page=1");
    server.shutdown().await.expect("listener stops");
    peer.join().expect("peer joins");
}

async fn assert_http_forbidden(
    address: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    origins: &[&str],
) {
    let status = raw_http_status(address, method, path, host, origins, "").await;
    assert!(
        status.contains(" 403 "),
        "{path} allowed rebinding: {status}"
    );
}

struct BrowserStopFixture(PathBuf);

impl BrowserStopFixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tessivum-browser-stop-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for BrowserStopFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct DelayedAdapter {
    calls: AtomicUsize,
    model_calls: AtomicUsize,
    model_configs: ParkingMutex<Vec<Value>>,
    started: Notify,
}

impl DelayedAdapter {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            model_calls: AtomicUsize::new(0),
            model_configs: ParkingMutex::new(Vec::new()),
            started: Notify::new(),
        }
    }
}

#[async_trait]
impl LlmAdapter for DelayedAdapter {
    async fn generate(
        &self,
        _: GenerateRequest,
        cancellation: tessivum_core::CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_one();
            cancellation.cancelled().await;
            return Err(TessivumError::new(
                "LLM_CANCELLED",
                "delayed Browser generation was cancelled",
                "llm",
                Value::Null,
            ));
        }
        let marker = "fresh-generation-complete";
        Ok(Box::pin(stream::iter(
            vec![
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: "text".into(),
                },
                StreamChunk::TextDelta {
                    index: 0,
                    text: marker.into(),
                },
                StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text {
                        text: marker.into(),
                    },
                },
                StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                },
            ]
            .into_iter()
            .map(Ok),
        )))
    }

    async fn models(&self, config: Value) -> Result<Value, TessivumError> {
        self.model_calls.fetch_add(1, Ordering::SeqCst);
        self.model_configs.lock().push(config);
        Ok(json!([{
            "id": "fake-model",
            "name": "Fake Model",
            "contextWindow": 4096,
            "maxOutput": 512,
            "reasoning": true,
            "input": ["text", "image"]
        }]))
    }
}

struct DelayedFactory(Arc<DelayedAdapter>);

impl HostLlmAdapterFactory for DelayedFactory {
    fn create(&self, _: &str, _: &str) -> Result<Arc<dyn LlmAdapter>, TessivumError> {
        Ok(self.0.clone())
    }
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
async fn bound_listener_rejects_dns_rebinding_across_http_routes() {
    let (mut server, _host, _base) = start().await;
    let address = server.local_addr();
    let authority = address.to_string();
    let origin = format!("http://{authority}");
    let exact_origin = [origin.as_str()];
    let session_body =
        r#"{"requestId":"authority-session","args":{"session":"authority-session"}}"#;
    let respond_body = r#"{"type":"client-response","rpcId":"authority-approval","result":{"ok":true,"value":{"sessionId":"session","approvalId":"approval","outcome":"allowed-once"}}}"#;

    assert!(
        raw_http_status(address, "GET", "/", &authority, &exact_origin, "")
            .await
            .contains(" 404 "),
        "exact authority reaches static fallback"
    );
    assert!(
        raw_http_status(
            address,
            "POST",
            "/api/session/status",
            &authority,
            &exact_origin,
            session_body,
        )
        .await
        .contains(" 200 "),
        "exact authority reaches API"
    );
    assert!(
        raw_http_status(
            address,
            "GET",
            "/events/authority-session",
            &authority,
            &exact_origin,
            "",
        )
        .await
        .contains(" 200 "),
        "exact authority reaches SSE"
    );
    assert!(
        raw_http_status(
            address,
            "POST",
            "/api/respond",
            &authority,
            &exact_origin,
            respond_body,
        )
        .await
        .contains(" 200 "),
        "exact authority reaches approval response"
    );

    assert_http_forbidden(address, "GET", "/", "attacker.example", &exact_origin).await;
    assert_http_forbidden(
        address,
        "POST",
        "/api/session/status",
        &authority,
        &["http://attacker.example"],
    )
    .await;
    assert_http_forbidden(
        address,
        "GET",
        "/events/authority-session",
        "attacker.example",
        &[],
    )
    .await;
    assert_http_forbidden(address, "POST", "/api/respond", &authority, &["null"]).await;
    assert_http_forbidden(
        address,
        "POST",
        "/api/session/status",
        &authority,
        &["https://attacker.example"],
    )
    .await;
    assert_http_forbidden(
        address,
        "POST",
        "/api/session/status",
        &authority,
        &["not-an-origin"],
    )
    .await;
    assert_http_forbidden(
        address,
        "POST",
        "/api/session/status",
        &authority,
        &[origin.as_str(), "http://attacker.example"],
    )
    .await;

    server.shutdown().await.expect("server shuts down");
}

#[tokio::test]
async fn bound_listener_uses_bracketed_ipv6_authority_when_available() {
    let host: Arc<dyn HostApi> = Arc::new(FakeHost::new());
    let mut server = match ApiServer::bind_at(host, "[::1]:0".parse().unwrap()).await {
        Ok(server) => server,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
            ) =>
        {
            return
        }
        Err(error) => panic!("IPv6 loopback bind failed: {error}"),
    };
    let authority = server.local_addr().to_string();
    assert!(authority.starts_with('['), "IPv6 authority is bracketed");
    let origin = format!("http://{authority}");
    let origins = [origin.as_str()];
    let status = raw_http_status(
        server.local_addr(),
        "POST",
        "/api/session/status",
        &authority,
        &origins,
        r#"{"requestId":"ipv6-authority","args":{"session":"ipv6-authority"}}"#,
    )
    .await;
    assert!(
        status.contains(" 200 "),
        "bracketed IPv6 authority accepted: {status}"
    );
    server.shutdown().await.expect("IPv6 server shuts down");
}

#[tokio::test]
async fn websocket_rejects_dns_rebinding_handshakes() {
    let (mut server, _host, _base) = start().await;
    let address = server.local_addr();
    let authority = address.to_string();
    let origin = format!("http://{authority}");
    let exact_origin = [origin.as_str()];

    for path in ["/ws", "/api/events.mux", "/api/events.host"] {
        let socket =
            match RawWebSocket::connect_path_with_headers(address, path, &authority, &exact_origin)
                .await
            {
                Ok(socket) => socket,
                Err(status) => panic!("exact authority WebSocket rejected: {status}"),
            };
        drop(socket);
    }
    let attacker_origin = ["http://attacker.example"];
    let null_origin = ["null"];
    let https_origin = ["https://attacker.example"];
    for (path, host, origins) in [
        ("/ws", authority.as_str(), &attacker_origin[..]),
        ("/api/events.mux", authority.as_str(), &null_origin[..]),
        ("/api/events.host", authority.as_str(), &https_origin[..]),
        ("/ws", "attacker.example", &exact_origin[..]),
    ] {
        let status =
            match RawWebSocket::connect_path_with_headers(address, path, host, origins).await {
                Ok(_) => panic!("{path} accepted rebinding handshake"),
                Err(status) => status,
            };
        assert!(status.contains(" 403 "), "{path} status: {status}");
    }
    server.shutdown().await.expect("server shuts down");
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
async fn browser_permission_remote_switches_and_seeds_projection_history() {
    let fixture = BrowserStopFixture::new();
    let runtime = Arc::new(
        HostRuntime::boot(HostConfig::new(&fixture.0, fixture.0.join("data")))
            .await
            .unwrap(),
    );
    let session = SessionId::from("browser-permission");
    runtime.create_session(session.clone()).await.unwrap();
    let mut server = ApiServer::bind(runtime).await.unwrap();
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();

    let listed = browser_call(
        &client,
        &base,
        "permission-list",
        "commands/list",
        json!({"args": {"agentId": session}}),
    )
    .await;
    assert!(
        listed["result"]["value"]
            .as_array()
            .is_some_and(|commands| commands
                .iter()
                .any(|command| command["name"] == "permission")),
        "{listed}"
    );

    let executed = browser_call(
        &client,
        &base,
        "permission-execute",
        "commands/execute",
        json!({"args": {"agentId": session, "line": "/permission danger-full-access"}}),
    )
    .await;
    assert_eq!(
        executed["result"]["value"]["result"],
        json!({"kind": "success", "text": "preset danger-full-access"})
    );

    let history = browser_call(
        &client,
        &base,
        "permission-history",
        "session.history",
        json!({"sessionId": session}),
    )
    .await;
    assert_eq!(
        history["result"]["value"]["projections"]["values"]["permissions"]["currentValue"],
        "danger-full-access"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn browser_subagent_routes_validate_and_preserve_payloads() {
    let (mut server, host, base) = start().await;
    let client = reqwest::Client::new();

    let history = browser_call(
        &client,
        &base,
        "subagent-history",
        "subagent.history",
        json!({
            "parentSessionId": "parent",
            "childSessionId": "child",
            "mode": "one-shot",
            "beforeSeq": 2,
            "maxMessages": 1,
        }),
    )
    .await;
    assert_eq!(history["result"]["value"]["hasMore"], false);
    assert_eq!(
        host.subagent_history_params.lock().as_slice(),
        &[SubagentHistoryRequest {
            parent_session_id: SessionId::from("parent"),
            child_session_id: SessionId::from("child"),
            mode: SubagentMode::OneShot,
            before_seq: Some(2),
            max_messages: Some(1),
        }]
    );

    let prompt = browser_call(
        &client,
        &base,
        "subagent-prompt",
        "subagent.prompt",
        json!({
            "parentSessionId": "parent",
            "childSessionId": "child",
            "mode": "continuable",
            "content": [{"type": "text", "text": "continue"}],
            "clientTimeZone": "UTC",
        }),
    )
    .await;
    assert_eq!(prompt["result"]["value"]["messageId"], "subagent-message");
    assert_eq!(
        host.subagent_prompt_params.lock()[0].mode,
        SubagentMode::Continuable
    );
    assert_eq!(
        host.subagent_prompt_params.lock()[0]
            .client_time_zone
            .as_deref(),
        Some("UTC")
    );

    let interrupt = browser_call(
        &client,
        &base,
        "subagent-interrupt",
        "subagent.interrupt",
        json!({
            "parentSessionId": "parent",
            "childSessionId": "child",
            "mode": "continuable",
        }),
    )
    .await;
    assert_eq!(interrupt["result"]["value"]["accepted"], true);
    assert_eq!(
        host.subagent_interrupt_params.lock()[0].mode,
        SubagentMode::Continuable
    );
    let deleted = browser_call(
        &client,
        &base,
        "subagent-delete",
        "subagent.delete",
        json!({
            "parentSessionId": "parent",
            "childSessionId": "child",
        }),
    )
    .await;
    assert_eq!(deleted["result"]["value"]["deleted"], true);
    assert_eq!(
        host.subagent_delete_params.lock().as_slice(),
        &[SubagentDeleteRequest {
            parent_session_id: SessionId::from("parent"),
            child_session_id: SessionId::from("child"),
        }]
    );

    let invalid = browser_call(
        &client,
        &base,
        "subagent-invalid",
        "subagent.history",
        json!({
            "parentSessionId": "parent",
            "childSessionId": "child",
            "mode": "one-shot",
            "unexpected": true,
        }),
    )
    .await;
    assert_eq!(invalid["result"]["error"]["code"], "bad-request");

    *host.subagent_error.lock() = Some(TessivumError::new(
        "SUBAGENT_PARENT_MISMATCH",
        "forged parent",
        "subagent",
        Value::Null,
    ));
    let denied = browser_call(
        &client,
        &base,
        "subagent-denied",
        "subagent.interrupt",
        json!({
            "parentSessionId": "parent",
            "childSessionId": "child",
            "mode": "continuable",
        }),
    )
    .await;
    assert_eq!(denied["result"]["error"]["code"], "subagent-unauthorized");
    assert_eq!(
        denied["result"]["error"]["details"]["childSessionId"],
        "child"
    );
    *host.subagent_error.lock() = Some(TessivumError::new(
        "SUBAGENT_DELETE_HAS_CHILDREN",
        "delete leaves first",
        "subagent",
        Value::Null,
    ));
    let blocked_delete = browser_call(
        &client,
        &base,
        "subagent-delete-blocked",
        "subagent.delete",
        json!({"parentSessionId": "parent", "childSessionId": "child"}),
    )
    .await;
    assert_eq!(
        blocked_delete["result"]["error"]["code"],
        "subagent-has-children"
    );
    server.shutdown().await.expect("server shuts down");
}
#[tokio::test]
async fn browser_directory_rpc_lists_and_creates_directories() {
    let root = BrowserStopFixture::new();
    fs::create_dir(root.0.join("visible")).unwrap();
    fs::create_dir(root.0.join(".hidden")).unwrap();

    fs::write(root.0.join("file.txt"), b"not a directory").unwrap();
    let canonical = fs::canonicalize(&root.0).unwrap();
    let (mut server, _host, base) = start().await;
    let client = reqwest::Client::new();

    let listed = browser_call(
        &client,
        &base,
        "directory-list",
        "host.listDirectory",
        json!({"path": root.0.to_string_lossy()}),
    )
    .await;
    assert_eq!(
        listed["result"]["value"]["path"],
        canonical.to_string_lossy().as_ref()
    );
    assert_eq!(listed["result"]["value"]["truncated"], false);
    assert_eq!(
        listed["result"]["value"]["entries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(listed["result"]["value"]["entries"][0]["name"], ".hidden");
    assert_eq!(listed["result"]["value"]["entries"][0]["hidden"], true);
    assert_eq!(listed["result"]["value"]["entries"][1]["name"], "visible");

    let created = browser_call(
        &client,
        &base,
        "directory-create",
        "host.createDirectory",
        json!({"path": root.0.to_string_lossy(), "name": "created"}),
    )
    .await;
    assert_eq!(
        created["result"]["value"]["path"],
        canonical.join("created").to_string_lossy().as_ref()
    );
    assert!(root.0.join("created").is_dir());
    server.shutdown().await.expect("server shuts down");
}
#[tokio::test]
async fn browser_session_search_rename_and_fork_use_strict_contracts() {
    let (mut server, _host, base) = start().await;
    let client = reqwest::Client::new();

    let search = browser_call(
        &client,
        &base,
        "search",
        "session.search",
        json!({"query": "  needle  "}),
    )
    .await;
    assert_eq!(
        search["result"]["value"]["items"][0]["sessionId"],
        "visible-session"
    );
    assert_eq!(
        search["result"]["value"]["items"][0]["snippet"],
        "matched needle"
    );
    assert_eq!(search["result"]["value"]["hasMore"], true);

    let invalid_search = browser_call(
        &client,
        &base,
        "invalid-search",
        "session.search",
        json!({"query": "\u{0000}"}),
    )
    .await;
    assert_eq!(invalid_search["result"]["error"]["code"], "bad-request");
    let invalid_fork = browser_call(
        &client,
        &base,
        "invalid-fork-seq",
        "session.fork",
        json!({"sessionId": "source", "atSeq": -1}),
    )
    .await;
    assert_eq!(invalid_fork["result"]["error"]["code"], "bad-request");

    let rename_extra = browser_call(
        &client,
        &base,
        "rename-extra",
        "session.rename",
        json!({"sessionId": "source", "title": "Renamed", "unexpected": true}),
    )
    .await;
    assert_eq!(rename_extra["result"]["error"]["code"], "bad-request");

    let title_invalid = browser_call(
        &client,
        &base,
        "title-invalid",
        "session.rename",
        json!({"sessionId": "source", "title": "   "}),
    )
    .await;
    assert_eq!(title_invalid["result"]["error"]["code"], "title-invalid");

    let renamed = browser_call(
        &client,
        &base,
        "rename",
        "session.rename",
        json!({"sessionId": "source", "title": "Renamed"}),
    )
    .await;
    assert_eq!(renamed["result"]["value"]["title"], "Renamed");
    assert_eq!(
        renamed["result"]["value"],
        json!({"title": "Renamed", "seq": 0})
    );

    let unavailable = browser_call(
        &client,
        &base,
        "fork-unavailable",
        "session.fork",
        json!({"sessionId": "source", "atSeq": 99}),
    )
    .await;
    assert_eq!(unavailable["result"]["error"]["code"], "fork-unavailable");

    let forked = browser_call(
        &client,
        &base,
        "fork",
        "session.fork",
        json!({"sessionId": "source"}),
    )
    .await;
    assert_eq!(forked["result"]["value"]["sessionId"], "forked-session");

    server.shutdown().await.expect("server shuts down");
}

#[tokio::test]
async fn browser_desktop_compat_rpcs_report_absence_without_success() {
    let (mut server, _host, base) = start().await;
    let client = reqwest::Client::new();

    let described = browser_call(
        &client,
        &base,
        "desktop-describe",
        "host.describe",
        json!({}),
    )
    .await;
    assert_eq!(described["result"]["value"]["canOpenPath"], false);
    let picked = browser_call(
        &client,
        &base,
        "desktop-pick",
        "host.pickDirectory",
        json!({}),
    )
    .await;
    assert_eq!(picked["result"]["ok"], false);
    assert_eq!(
        picked["result"]["error"]["code"],
        "directory-picker-unavailable"
    );
    assert_eq!(
        picked["result"]["error"]["details"],
        json!({"capability": "absent"})
    );
    let pick_extra = browser_call(
        &client,
        &base,
        "desktop-pick-extra",
        "host.pickDirectory",
        json!({"unexpected": true}),
    )
    .await;
    assert_eq!(pick_extra["result"]["error"]["code"], "bad-request");

    let opened = browser_call(
        &client,
        &base,
        "desktop-open",
        "host.openPath",
        json!({"path": "/tmp"}),
    )
    .await;
    assert_eq!(opened["result"]["ok"], false);
    assert_eq!(opened["result"]["error"]["code"], "internal");
    assert_eq!(opened["result"]["error"]["details"], json!({}));
    let empty_open = browser_call(
        &client,
        &base,
        "desktop-open-empty",
        "host.openPath",
        json!({"path": ""}),
    )
    .await;
    assert_eq!(empty_open["result"]["error"]["code"], "bad-request");
    let open_extra = browser_call(
        &client,
        &base,
        "desktop-open-extra",
        "host.openPath",
        json!({"path": "/tmp", "unexpected": true}),
    )
    .await;
    assert_eq!(open_extra["result"]["error"]["code"], "bad-request");

    let settings = browser_call(
        &client,
        &base,
        "desktop-settings",
        "settings.openDocument",
        json!({}),
    )
    .await;
    assert_eq!(settings["result"]["ok"], false);
    assert_eq!(settings["result"]["error"]["code"], "internal");
    assert!(settings["result"]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("settings service is absent"));
    let settings_extra = browser_call(
        &client,
        &base,
        "desktop-settings-extra",
        "settings.openDocument",
        json!({"unexpected": true}),
    )
    .await;
    assert_eq!(settings_extra["result"]["error"]["code"], "bad-request");

    server.shutdown().await.expect("server shuts down");
}
#[tokio::test]
async fn browser_desktop_rpcs_preserve_success_cancel_and_origin_envelopes() {
    let (mut server, host, base) = start().await;
    let client = reqwest::Client::new();
    host.desktop_enabled.store(true, Ordering::SeqCst);
    *host.picked_directory.lock().expect("picked directory lock") = Some(PathBuf::from("/chosen"));

    let described = browser_call(&client, &base, "desktop-ready", "host.describe", json!({})).await;
    assert_eq!(described["result"]["value"]["canOpenPath"], true);

    let picked = browser_call(
        &client,
        &base,
        "desktop-picked",
        "host.pickDirectory",
        json!({}),
    )
    .await;
    assert_eq!(
        picked["result"],
        json!({"ok": true, "value": {"path": "/chosen"}})
    );
    *host.picked_directory.lock().expect("picked directory lock") = None;
    let cancelled = browser_call(
        &client,
        &base,
        "desktop-cancelled",
        "host.pickDirectory",
        json!({}),
    )
    .await;
    assert_eq!(
        cancelled["result"],
        json!({"ok": true, "value": {"path": null}})
    );

    let opened = browser_call(
        &client,
        &base,
        "desktop-opened",
        "host.openPath",
        json!({"path": "/chosen/file.txt"}),
    )
    .await;
    assert_eq!(
        opened["result"],
        json!({"ok": true, "value": {"opened": true}})
    );
    assert_eq!(
        host.opened_paths
            .lock()
            .expect("opened paths lock")
            .as_slice(),
        ["/chosen/file.txt"]
    );

    let settings = browser_call(
        &client,
        &base,
        "desktop-settings",
        "settings.openDocument",
        json!({}),
    )
    .await;
    assert_eq!(
        settings["result"],
        json!({"ok": true, "value": {"opened": true}})
    );
    assert_eq!(host.settings_opens.load(Ordering::SeqCst), 1);

    let body = r#"{"requestId":"desktop-remote","args":{"path":"/chosen/file.txt"}}"#;
    let status = raw_http_status(
        server.local_addr(),
        "POST",
        "/api/host.openPath",
        "attacker.example",
        &["http://attacker.example"],
        body,
    )
    .await;
    assert!(
        status.contains(" 403 "),
        "openPath accepted an untrusted origin: {status}"
    );
    server.shutdown().await.expect("server shuts down");
}

async fn wait_host_running(socket: &mut RawWebSocket, session: &str, running: bool) -> Value {
    timeout(Duration::from_secs(2), async {
        loop {
            let text = socket.read_text().await.expect("host downlink frame");
            let frame: Value = serde_json::from_str(&text).expect("host frame JSON");
            if frame["payload"]["type"] == "host/session-status"
                && frame["payload"]["sessionId"] == session
                && frame["payload"]["running"] == running
            {
                return frame;
            }
        }
    })
    .await
    .expect("host session status frame arrives")
}

#[tokio::test]
async fn browser_rpc_creates_a_session_and_maps_prompt_content() {
    let (mut server, host, base) = start().await;
    let client = reqwest::Client::new();

    let described = browser_call(&client, &base, "describe", "host.describe", json!({})).await;
    assert_eq!(described["type"], "server-response");
    assert_eq!(described["rpcId"], "describe");
    assert_eq!(described["result"]["ok"], true);
    assert_eq!(described["result"]["value"]["canOpenPath"], false);
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
            "clientTimeZone": "Asia/Shanghai",
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
async fn browser_model_rpcs_use_host_dtos_and_published_selection_wire() {
    let (mut server, _host, base) = start().await;
    let client = reqwest::Client::new();

    let providers = browser_call(&client, &base, "providers", "llm.providers", json!({})).await;
    assert_eq!(
        providers["result"]["value"]["providers"][0]["provider"],
        "fake-provider"
    );
    assert_eq!(
        providers["result"]["value"]["providers"][0]["settingsNs"],
        "llm-pi-ai"
    );

    let models = browser_call(&client, &base, "models", "llm.models", json!({})).await;
    assert_eq!(
        models["result"]["value"]["groups"][0]["id"],
        "fake-provider"
    );
    assert_eq!(
        models["result"]["value"]["groups"][0]["models"][0]["description"],
        "Fixture model"
    );
    assert_eq!(
        models["result"]["value"]["groups"][0]["models"][0]["reasoning"]["efforts"][0],
        json!({"id": "high", "name": "High"})
    );
    assert_eq!(
        models["result"]["value"]["groups"][0]["models"][0]["reasoning"]["defaultEffort"],
        "high"
    );

    let session = browser_call(
        &client,
        &base,
        "session-models",
        "session.models",
        json!({"sessionId": "browser-session"}),
    )
    .await;
    assert_eq!(session["result"]["value"]["current"]["model"], "fake-model");

    let selected = browser_call(
        &client,
        &base,
        "select-model",
        "session.selectModel",
        json!({
            "sessionId": "browser-session",
            "provider": "fake-provider",
            "model": "fake-model",
            "reasoningEffort": "high"
        }),
    )
    .await;
    assert_eq!(
        selected["result"]["value"]["selected"],
        json!({
            "provider": "fake-provider",
            "model": "fake-model",
            "reasoningEffort": "high"
        })
    );

    server.shutdown().await.expect("server shuts down");
}

#[tokio::test]
async fn browser_provider_enable_rpc_is_strict_and_structured() {
    let (mut server, host, base) = start().await;
    let client = reqwest::Client::new();

    let absent = browser_call(
        &client,
        &base,
        "provider-enable-absent-settings",
        "llm.setProviderEnabled",
        json!({"provider": "fake-provider", "enabled": true}),
    )
    .await;
    assert_eq!(absent["result"]["ok"], false);
    assert_eq!(
        absent["result"]["error"],
        json!({
            "code": "SETTINGS_UNAVAILABLE",
            "message": "settings service is unavailable",
            "details": {"namespace": "llm-pi-ai"},
        })
    );

    host.provider_enable_available.store(true, Ordering::SeqCst);
    let enabled = browser_call(
        &client,
        &base,
        "provider-enable",
        "llm.setProviderEnabled",
        json!({"provider": "fake-provider", "enabled": true}),
    )
    .await;
    assert_eq!(
        enabled["result"]["value"],
        json!({
            "provider": "fake-provider",
            "enabled": true,
        })
    );
    assert_eq!(
        host.provider_enable_calls
            .lock()
            .expect("provider enable lock")
            .as_slice(),
        &[("fake-provider".into(), true)]
    );

    let extra = browser_call(
        &client,
        &base,
        "provider-enable-extra",
        "llm.setProviderEnabled",
        json!({"provider": "fake-provider", "enabled": false, "unexpected": true}),
    )
    .await;
    assert_eq!(extra["result"]["error"]["code"], "bad-request");
    assert_eq!(
        host.provider_enable_calls
            .lock()
            .expect("provider enable lock")
            .len(),
        1
    );

    server.shutdown().await.expect("server shuts down");
}

#[tokio::test]
async fn browser_adapter_updates_keep_the_owner_event_name() {
    let (mut server, host, _base) = start().await;
    let mut socket = RawWebSocket::connect_path(server.local_addr(), "/api/events.host").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    host.notifications
        .send(HostNotification::AdaptersUpdated)
        .expect("owner event sends");
    let frame: Value = serde_json::from_str(
        &timeout(Duration::from_secs(1), socket.read_text())
            .await
            .expect("owner event arrives")
            .expect("owner socket remains open"),
    )
    .expect("owner event is JSON");
    assert_eq!(frame["method"], "host/remote-event");
    assert_eq!(
        frame["payload"],
        json!({"type": "host/remote-event", "event": "llm/adapters-updated", "args": []})
    );

    server.shutdown().await.expect("server shuts down");
}

#[tokio::test]
async fn browser_provider_models_uses_strict_payloads_and_exact_result_envelopes() {
    let fixture = BrowserStopFixture::new();
    let adapter = Arc::new(DelayedAdapter::new());
    let mut config = HostConfig::new(&fixture.0, fixture.0.join("data"))
        .with_adapter_factory(Arc::new(DelayedFactory(Arc::clone(&adapter))));
    config.provider = "fake-provider".into();
    config.model = "fake-model".into();
    let runtime = Arc::new(HostRuntime::boot(config).await.expect("real Host boots"));
    let mut server = ApiServer::bind(runtime.clone())
        .await
        .expect("server binds");
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();

    let default_config = browser_call(
        &client,
        &base,
        "provider-models-default",
        "llm.providerModels",
        json!({"provider": "fake-provider"}),
    )
    .await;
    let updated_at = default_config["result"]["value"]["updatedAt"]
        .as_u64()
        .expect("epoch-millisecond timestamp");
    assert_eq!(
        default_config,
        json!({
            "type": "server-response",
            "rpcId": "provider-models-default",
            "result": {
                "ok": true,
                "value": {
                    "provider": "fake-provider",
                    "models": [{
                        "id": "fake-model",
                        "name": "Fake Model",
                        "contextWindow": 4096,
                        "maxOutput": 512,
                        "reasoning": true,
                        "input": ["text", "image"],
                    }],
                    "updatedAt": updated_at,
                },
            },
        })
    );

    let custom = json!({"apiKey": "draft-key", "nested": {"region": "test"}});
    let custom_config = browser_call(
        &client,
        &base,
        "provider-models-custom",
        "llm.providerModels",
        json!({"provider": "fake-provider", "config": custom.clone()}),
    )
    .await;
    assert_eq!(custom_config["result"]["ok"], true);

    let unknown = browser_call(
        &client,
        &base,
        "provider-models-unknown",
        "llm.providerModels",
        json!({"provider": "fake-provider-alias"}),
    )
    .await;
    assert_eq!(unknown["type"], "server-response");
    assert_eq!(unknown["rpcId"], "provider-models-unknown");
    assert_eq!(unknown["result"]["ok"], false);
    assert_eq!(unknown["result"]["error"]["code"], "LLM_PROVIDER_NOT_FOUND");
    assert_eq!(
        unknown["result"]["error"]["details"],
        json!({
            "provider": "fake-provider-alias",
            "attempts": 0,
            "retries": 0,
            "retryable": false,
        })
    );

    for (rpc_id, payload) in [
        (
            "provider-models-extra",
            json!({"provider": "fake-provider", "unexpected": true}),
        ),
        (
            "provider-models-config-array",
            json!({"provider": "fake-provider", "config": []}),
        ),
    ] {
        let response = browser_call(&client, &base, rpc_id, "llm.providerModels", payload).await;
        assert_eq!(response["type"], "server-response");
        assert_eq!(response["rpcId"], rpc_id);
        assert_eq!(response["result"]["ok"], false);
        assert_eq!(response["result"]["error"]["code"], "bad-request");
        assert_eq!(
            response["result"]["error"]["details"],
            json!({"issues": []})
        );
    }
    assert_eq!(adapter.model_calls.load(Ordering::SeqCst), 2);
    assert_eq!(*adapter.model_configs.lock(), vec![json!({}), custom]);

    server.shutdown().await.expect("server shuts down");
    runtime.shutdown().await.expect("host shuts down");
}

#[tokio::test]
async fn browser_queue_rpc_is_strict_and_preserves_host_queue_errors() {
    let (mut server, host, base) = start().await;
    let client = reqwest::Client::new();

    let accepted = browser_call(
        &client,
        &base,
        "queue-edit",
        "session.updateQueue",
        json!({
            "sessionId": "queue-session",
            "itemId": "second",
            "action": {"kind": "edit", "content": [{"type": "text", "text": "edited"}]},
        }),
    )
    .await;
    assert_eq!(accepted["result"]["value"], json!({"accepted": true}));
    {
        let updates = host.queue_updates.lock().expect("queue update lock");
        assert!(matches!(
            updates.as_slice(),
            [SessionUpdateQueueParams {
                session_id,
                item_id,
                action: SessionQueueAction::Edit { .. },
            }] if session_id == &SessionId::from("queue-session") && item_id.as_str() == "second"
        ));
    }

    let malformed = browser_call(
        &client,
        &base,
        "queue-malformed",
        "session.updateQueue",
        json!({
            "sessionId": "queue-session",
            "itemId": "second",
            "action": {"kind": "remove", "extra": true}
        }),
    )
    .await;
    assert_eq!(malformed["type"], "server-response");
    assert_eq!(malformed["rpcId"], "queue-malformed");
    assert_eq!(malformed["result"]["ok"], false);
    assert_eq!(malformed["result"]["error"]["code"], "bad-request");
    assert_eq!(
        malformed["result"]["error"]["details"],
        json!({"issues": []})
    );
    assert!(malformed["result"].get("value").is_none());
    assert_eq!(
        host.queue_updates.lock().expect("queue update lock").len(),
        1
    );

    *host.queue_error.lock().expect("queue error lock") = Some(TessivumError::new(
        "steer-unavailable",
        "current turn no longer accepts steering",
        "queue",
        json!({"itemId": "second"}),
    ));
    let unavailable = browser_call(
        &client,
        &base,
        "queue-steer",
        "session.updateQueue",
        json!({
            "sessionId": "queue-session",
            "itemId": "second",
            "action": {"kind": "steer"},
        }),
    )
    .await;
    assert_eq!(unavailable["result"]["error"]["code"], "steer-unavailable");
    assert_eq!(
        unavailable["result"]["error"]["details"],
        json!({"itemId": "second"})
    );

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
async fn browser_approval_responses_are_raw_and_fail_closed() {
    let (mut server, _host, base) = start().await;
    let client = reqwest::Client::new();
    let response: Value = client
        .post(format!("{base}/api/respond"))
        .json(&json!({
            "type": "client-response",
            "rpcId": "unknown-approval",
            "result": {"ok": true, "value": {
                "sessionId": "session",
                "approvalId": "approval",
                "outcome": "allowed-once"
            }}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        response,
        json!({"accepted": false, "reason": "not-pending"})
    );

    let malformed: Value = client
        .post(format!("{base}/api/respond"))
        .json(&json!({"type": "client-request", "rpcId": "bad", "result": {}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        malformed,
        json!({"accepted": false, "reason": "bad-response"})
    );
    server.shutdown().await.unwrap();
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
async fn browser_queue_notifications_publish_full_snapshots() {
    let (mut server, host, _base) = start().await;
    let mut socket = RawWebSocket::connect_path(server.local_addr(), "/api/events.mux").await;
    let message = serde_json::from_value(json!({
        "id": "queue-item",
        "role": "user",
        "content": [{"type": "text", "text": "pending"}],
        "source": {"kind": "user"},
    }))
    .expect("queue message");
    host.notifications
        .send(HostNotification::SessionQueue(
            HostSessionQueueNotification {
                session_id: SessionId::from("queue-session"),
                items: vec![HostSessionQueueItem {
                    id: MessageId::from("queue-item"),
                    placement: "queued".into(),
                    message,
                }],
            },
        ))
        .expect("queue notification publishes");
    let frame: Value = serde_json::from_str(
        &timeout(Duration::from_secs(1), socket.read_text())
            .await
            .expect("queue frame arrives")
            .expect("queue socket remains open"),
    )
    .expect("queue frame JSON");
    assert_eq!(frame["payload"]["type"], "session/queue");
    assert_eq!(frame["payload"]["items"][0]["id"], "queue-item");
    assert_eq!(frame["payload"]["items"][0]["placement"], "queued");

    server.shutdown().await.expect("server shuts down");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_workspace_archives_publish_monotonic_snapshots() {
    let fixture = BrowserStopFixture::new();
    let runtime = HostRuntime::boot(HostConfig::new(&fixture.0, fixture.0.join("data")))
        .await
        .expect("real Host boots");
    let handle = runtime.handle();
    let host: Arc<dyn HostApi> = Arc::new(handle.clone());
    let mut server = ApiServer::bind(host).await.expect("real API binds");
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let workspaces = browser_call(
        &client,
        &base,
        "workspace-list",
        "workspace.list",
        json!({}),
    )
    .await;
    let workspace_id = workspaces["result"]["value"]["items"][0]["workspaceId"]
        .as_str()
        .expect("default workspace id")
        .to_owned();
    for session_id in ["archive-first", "archive-second"] {
        let created = browser_call(
            &client,
            &base,
            session_id,
            "session.create",
            json!({"workspaceId": workspace_id, "sessionId": session_id}),
        )
        .await;
        assert_eq!(created["result"]["ok"], true);
    }
    let mut downlink = RawWebSocket::connect_path(server.local_addr(), "/api/events.host").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let (first, second) = tokio::join!(
        browser_call(
            &client,
            &base,
            "archive-first",
            "workspace.archiveSession",
            json!({"sessionId": "archive-first"}),
        ),
        browser_call(
            &client,
            &base,
            "archive-second",
            "workspace.archiveSession",
            json!({"sessionId": "archive-second"}),
        )
    );
    assert_eq!(first["result"]["ok"], true);
    assert_eq!(second["result"]["ok"], true);
    let mut snapshots = Vec::new();
    while snapshots.len() < 2 {
        let frame: Value = serde_json::from_str(
            &timeout(Duration::from_secs(1), downlink.read_text())
                .await
                .expect("archive frame arrives")
                .expect("archive frame is text"),
        )
        .expect("archive frame is JSON");
        if frame["payload"]["type"] == "host/archived-sessions-changed" {
            snapshots.push(frame["payload"]["archivedSessionIds"].clone());
        }
    }
    assert_eq!(snapshots[0].as_array().unwrap().len(), 1);
    assert_eq!(snapshots[1].as_array().unwrap().len(), 2);
    assert!(snapshots[1]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id == "archive-first"));
    assert!(snapshots[1]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id == "archive-second"));

    server.shutdown().await.expect("API shuts down");
    runtime.shutdown().await.expect("Host shuts down");
}

#[tokio::test]
async fn browser_workspace_insert_before_is_durable_and_broadcasts_complete_order() {
    let fixture = BrowserStopFixture::new();
    let first_path = fixture.0.join("first");
    let second_path = fixture.0.join("second");
    fs::create_dir(&first_path).unwrap();
    fs::create_dir(&second_path).unwrap();
    let runtime = HostRuntime::boot(HostConfig::new(&fixture.0, fixture.0.join("data")))
        .await
        .expect("real Host boots");
    let handle = runtime.handle();
    let host: Arc<dyn HostApi> = Arc::new(handle.clone());
    let mut server = ApiServer::bind(host).await.expect("real API binds");
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let first = browser_call(
        &client,
        &base,
        "workspace-first",
        "workspace.create",
        json!({"path": first_path.to_string_lossy()}),
    )
    .await;
    let first_id = first["result"]["value"]["workspace"]["workspaceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = browser_call(
        &client,
        &base,
        "workspace-second",
        "workspace.create",
        json!({"path": second_path.to_string_lossy()}),
    )
    .await;
    let second_id = second["result"]["value"]["workspace"]["workspaceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let initial = browser_call(
        &client,
        &base,
        "workspace-order-initial",
        "workspace.list",
        json!({}),
    )
    .await;
    let default_id = initial["result"]["value"]["items"][2]["workspaceId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        initial["result"]["value"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|workspace| workspace["workspaceId"].clone())
            .collect::<Vec<_>>(),
        vec![
            Value::String(second_id.clone()),
            Value::String(first_id.clone()),
            Value::String(default_id.clone()),
        ]
    );

    let mut downlink = RawWebSocket::connect_path(server.local_addr(), "/api/events.host").await;
    let reordered = browser_call(
        &client,
        &base,
        "workspace-order-before",
        "workspace.insertBefore",
        json!({"workspaceId": default_id, "beforeWorkspaceId": second_id}),
    )
    .await;
    let first_order = json!([default_id, second_id, first_id]);
    assert_eq!(
        reordered,
        json!({
            "type": "server-response",
            "rpcId": "workspace-order-before",
            "result": {"ok": true, "value": {"workspaceIds": first_order}},
        })
    );
    let first_frame: Value = serde_json::from_str(
        &timeout(Duration::from_secs(1), downlink.read_text())
            .await
            .expect("workspace order frame arrives")
            .expect("workspace order frame is text"),
    )
    .expect("workspace order frame is JSON");
    assert_eq!(first_frame["type"], "server-request");
    assert_eq!(first_frame["method"], "host/workspace-order-changed");
    assert_eq!(first_frame["payload"]["workspaceIds"], first_order);

    let appended = browser_call(
        &client,
        &base,
        "workspace-order-append",
        "workspace.insertBefore",
        json!({"workspaceId": default_id}),
    )
    .await;
    let appended_order = json!([second_id, first_id, default_id]);
    assert_eq!(
        appended,
        json!({
            "type": "server-response",
            "rpcId": "workspace-order-append",
            "result": {"ok": true, "value": {"workspaceIds": appended_order}},
        })
    );
    let appended_frame: Value = serde_json::from_str(
        &timeout(Duration::from_secs(1), downlink.read_text())
            .await
            .expect("workspace append frame arrives")
            .expect("workspace append frame is text"),
    )
    .expect("workspace append frame is JSON");
    assert_eq!(appended_frame["type"], "server-request");
    assert_eq!(appended_frame["method"], "host/workspace-order-changed");
    assert_eq!(appended_frame["payload"]["workspaceIds"], appended_order);

    let no_op = browser_call(
        &client,
        &base,
        "workspace-order-no-op",
        "workspace.insertBefore",
        json!({"workspaceId": default_id, "beforeWorkspaceId": default_id}),
    )
    .await;
    assert_eq!(
        no_op,
        json!({
            "type": "server-response",
            "rpcId": "workspace-order-no-op",
            "result": {"ok": true, "value": {"workspaceIds": appended_order}},
        })
    );
    assert!(
        timeout(Duration::from_millis(100), downlink.read_text())
            .await
            .is_err(),
        "no-op workspace reorders emit no order frame"
    );

    let unknown = Uuid::new_v4().to_string();
    let unknown_source = browser_call(
        &client,
        &base,
        "workspace-order-unknown-source",
        "workspace.insertBefore",
        json!({"workspaceId": unknown.clone()}),
    )
    .await;
    assert_eq!(
        unknown_source,
        json!({
            "type": "server-response",
            "rpcId": "workspace-order-unknown-source",
            "result": {"ok": false, "error": {
                "code": "workspace-not-found",
                "message": "workspace was not found",
                "details": {"workspaceId": unknown},
            }},
        })
    );
    let unknown_anchor = browser_call(
        &client,
        &base,
        "workspace-order-unknown-anchor",
        "workspace.insertBefore",
        json!({"workspaceId": default_id.clone(), "beforeWorkspaceId": unknown.clone()}),
    )
    .await;
    assert_eq!(
        unknown_anchor,
        json!({
            "type": "server-response",
            "rpcId": "workspace-order-unknown-anchor",
            "result": {"ok": false, "error": {
                "code": "workspace-not-found",
                "message": "workspace was not found",
                "details": {"workspaceId": unknown},
            }},
        })
    );
    for (rpc_id, payload) in [
        (
            "workspace-order-extra",
            json!({"workspaceId": default_id.clone(), "extra": true}),
        ),
        (
            "workspace-order-null-anchor",
            json!({"workspaceId": default_id.clone(), "beforeWorkspaceId": null}),
        ),
    ] {
        let rejected =
            browser_call(&client, &base, rpc_id, "workspace.insertBefore", payload).await;
        assert_eq!(rejected["type"], "server-response");
        assert_eq!(rejected["rpcId"], rpc_id);
        assert_eq!(rejected["result"]["ok"], false);
        assert_eq!(rejected["result"]["error"]["code"], "bad-request");
        assert_eq!(
            rejected["result"]["error"]["details"],
            json!({"issues": []})
        );
    }

    let data_dir = fixture.0.join("data");
    let displaced_data_dir = fixture.0.join("data-displaced");
    #[cfg(windows)]
    assert_eq!(
        fs::rename(&data_dir, &displaced_data_dir)
            .expect_err("the held directory capability prevents replacement")
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    #[cfg(not(windows))]
    {
        fs::rename(&data_dir, &displaced_data_dir).unwrap();
        fs::create_dir(&data_dir).unwrap();
        let failed_write = browser_call(
            &client,
            &base,
            "workspace-order-failed-write",
            "workspace.insertBefore",
            json!({"workspaceId": first_id, "beforeWorkspaceId": second_id}),
        )
        .await;
        fs::remove_dir(&data_dir).unwrap();
        fs::rename(&displaced_data_dir, &data_dir).unwrap();
        assert_eq!(
            failed_write,
            json!({
                "type": "server-response",
                "rpcId": "workspace-order-failed-write",
                "result": {"ok": false, "error": {
                    "code": "internal",
                    "message": "workspace operation failed: WORKSPACE_PERSISTENCE_FAILED",
                    "details": {},
                }},
            })
        );
    }
    let unchanged = browser_call(
        &client,
        &base,
        "workspace-order-unchanged",
        "workspace.list",
        json!({}),
    )
    .await;
    assert_eq!(
        Value::Array(
            unchanged["result"]["value"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|workspace| workspace["workspaceId"].clone())
                .collect(),
        ),
        appended_order
    );
    assert!(
        timeout(Duration::from_millis(100), downlink.read_text())
            .await
            .is_err(),
        "rejected workspace reorders emit no order frame"
    );

    server.shutdown().await.expect("API shuts down");
    runtime.shutdown().await.expect("Host shuts down");
}

#[tokio::test]
async fn approval_mux_frames_keep_the_stable_rpc_id_and_redact_arguments() {
    let (mut server, host, _base) = start().await;
    let mut mux = RawWebSocket::connect_path(server.local_addr(), "/api/events.mux").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let requested = ApprovalRequested {
        rpc_id: "stable-approval-rpc".into(),
        session_id: SessionId::from("approval-session"),
        approval_id: ApprovalId::new("approval-id"),
        tool_name: "danger".into(),
        call_id: Some("tool-call".into()),
        reason: Some("policy".into()),
    };
    host.notifications
        .send(HostNotification::ApprovalRequested(requested.clone()))
        .unwrap();
    let requested_frame: Value = serde_json::from_str(
        &timeout(Duration::from_secs(1), mux.read_text())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(requested_frame["type"], "server-request");
    assert_eq!(requested_frame["rpcId"], requested.rpc_id);
    assert_eq!(requested_frame["method"], "approval/requested");
    assert_eq!(
        requested_frame["payload"],
        json!({
            "type": "approval/requested",
            "sessionId": "approval-session",
            "approvalId": "approval-id",
            "toolName": "danger",
            "callId": "tool-call",
            "reason": "policy",
        })
    );
    host.notifications
        .send(HostNotification::ApprovalResolved(ApprovalResolved {
            rpc_id: requested.rpc_id,
            session_id: SessionId::from("approval-session"),
            approval_id: ApprovalId::new("approval-id"),
            outcome: ApprovalOutcome::Rejected,
        }))
        .unwrap();
    let resolved_frame: Value = serde_json::from_str(
        &timeout(Duration::from_secs(1), mux.read_text())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(resolved_frame["method"], "approval/resolved");
    assert_eq!(resolved_frame["payload"]["outcome"], "rejected");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn browser_settings_and_credentials_use_redacted_published_wire() {
    let fixture = BrowserStopFixture::new();
    let opener = Arc::new(RecordingNativeOpener(Mutex::new(Vec::new())));
    let runtime = HostRuntime::boot(
        HostConfig::new(&fixture.0, fixture.0.join("data")).with_path_opener(opener.clone()),
    )
    .await
    .expect("real Host boots");
    let handle = runtime.handle();
    let settings = handle.settings().expect("settings service");
    let namespace = "llm-settings-wire".to_owned();
    settings
        .register(
            SettingsRegistration::new(
                namespace.clone(),
                json!({"type": "object"}),
                json!({"visible": "default", "secret": "default-secret"}),
                json!({"base": true}),
            )
            .with_secret_paths(vec![vec!["secret".into()]]),
        )
        .await
        .expect("namespace registers");
    let restart_namespace = "agent-default-model".to_owned();
    let internal_namespace = "internal-settings".to_owned();
    settings
        .register(SettingsRegistration::new(
            internal_namespace.clone(),
            json!({"type": "object"}),
            json!({}),
            json!({}),
        ))
        .await
        .expect("internal namespace registers");
    let host: Arc<dyn HostApi> = Arc::new(handle.clone());
    let mut server = ApiServer::bind(host).await.expect("real API binds");
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let mut downlink = RawWebSocket::connect_path(server.local_addr(), "/api/events.host").await;

    let described = browser_call(
        &client,
        &base,
        "settings-describe",
        "settings.describe",
        json!({}),
    )
    .await;
    let value = &described["result"]["value"];
    assert_eq!(value["writable"], true);
    assert_eq!(value["hasDocument"], true);

    let opened_document = browser_call(
        &client,
        &base,
        "settings-open-document",
        "settings.openDocument",
        json!({}),
    )
    .await;
    assert_eq!(
        opened_document["result"],
        json!({"ok": true, "value": {"opened": true}})
    );
    let settings_path = fixture.0.join("data/settings.yaml");
    assert!(settings_path.is_file());
    assert_eq!(
        opener.0.lock().expect("native opener lock").as_slice(),
        [fs::canonicalize(settings_path).unwrap()]
    );
    let document_extra = browser_call(
        &client,
        &base,
        "settings-open-document-extra",
        "settings.openDocument",
        json!({"unexpected": true}),
    )
    .await;
    assert_eq!(document_extra["result"]["error"]["code"], "bad-request");
    let namespaces = value["namespaces"].as_array().unwrap();
    let namespace_view = namespaces
        .iter()
        .find(|view| view["ns"].as_str() == Some(namespace.as_str()))
        .expect("published namespace is described");
    assert!(namespaces
        .iter()
        .all(|view| view["ns"].as_str() != Some(internal_namespace.as_str())));
    assert_eq!(namespace_view["applies"], "live");
    let restart_view = namespaces
        .iter()
        .find(|view| view["ns"].as_str() == Some(restart_namespace.as_str()))
        .expect("restart namespace is described");
    assert_eq!(restart_view["applies"], "restart");
    assert_eq!(
        namespace_view["secrets"],
        json!([{"path": ["secret"], "set": true}])
    );
    assert_eq!(namespace_view["base"], json!({"base": true}));
    assert!(namespace_view.get("user").is_none());
    assert!(!described.to_string().contains("default-secret"));
    let hidden = browser_call(
        &client,
        &base,
        "settings-hidden",
        "settings.update",
        json!({"ns": internal_namespace, "patch": {"hidden": true}}),
    )
    .await;
    assert_eq!(hidden["result"]["error"]["code"], "settings-not-exposed");
    assert_eq!(
        hidden["result"]["error"]["details"],
        json!({"ns": internal_namespace})
    );
    assert_eq!(settings.get(&internal_namespace).unwrap().value, json!({}));

    let secret = "settings-wire-secret";
    let updated = browser_call(
        &client,
        &base,
        "settings-update",
        "settings.update",
        json!({"ns": namespace, "patch": {"visible": "saved", "secret": secret}, "expectedRevision": 0}),
    )
    .await;
    assert_eq!(updated["result"]["value"]["revision"], 1);
    assert!(!updated.to_string().contains(secret));
    let mutated = browser_call(
        &client,
        &base,
        "settings-mutate",
        "settings.mutate",
        json!({"ns": namespace, "ops": [{"op": "set", "path": ["added"], "value": true}, {"op": "unset", "path": ["visible"]}], "expectedRevision": 1}),
    )
    .await;
    assert_eq!(mutated["result"]["value"]["revision"], 2);
    let conflict = browser_call(
        &client,
        &base,
        "settings-conflict",
        "settings.replace",
        json!({"ns": namespace, "section": {}, "expectedRevision": 1}),
    )
    .await;
    assert_eq!(conflict["result"]["error"]["code"], "settings-conflict");
    assert_eq!(
        conflict["result"]["error"]["details"],
        json!({"ns": namespace, "expected": 1, "actual": 2})
    );

    let settings_frame = timeout(Duration::from_secs(1), async {
        loop {
            let frame: Value =
                serde_json::from_str(&downlink.read_text().await.expect("host frame"))
                    .expect("host frame JSON");
            if frame["payload"]["type"] == "host/remote-event"
                && frame["payload"]["event"] == "settings/document-updated"
            {
                return frame;
            }
        }
    })
    .await
    .expect("settings invalidation arrives");
    assert_eq!(settings_frame["payload"]["args"], json!([namespace, 1]));
    assert!(!settings_frame.to_string().contains(secret));

    let reference = format!("TESSIVUM_BROWSER_{}", Uuid::new_v4().simple());
    let credential_secret = "credential-wire-secret";
    let set = browser_call(
        &client,
        &base,
        "credentials-set",
        "credentials.set",
        json!({"ref": reference, "value": credential_secret}),
    )
    .await;
    assert_eq!(set["result"]["value"], json!({}));
    assert!(!set.to_string().contains(credential_secret));
    let credentials = browser_call(
        &client,
        &base,
        "credentials-describe",
        "credentials.describe",
        json!({"refs": [reference]}),
    )
    .await;
    assert_eq!(
        credentials["result"]["value"]["credentials"][reference.as_str()]["configured"],
        true
    );
    assert_eq!(
        credentials["result"]["value"]["credentials"][reference.as_str()]["source"],
        "file"
    );
    assert!(!credentials.to_string().contains(credential_secret));
    let credential_frame = timeout(Duration::from_secs(1), async {
        loop {
            let frame: Value =
                serde_json::from_str(&downlink.read_text().await.expect("host frame"))
                    .expect("host frame JSON");
            if frame["payload"]["type"] == "host/remote-event"
                && frame["payload"]["event"] == "credentials/updated"
            {
                return frame;
            }
        }
    })
    .await
    .expect("credential invalidation arrives");
    assert_eq!(credential_frame["payload"]["args"], json!([reference]));
    assert!(!credential_frame.to_string().contains(credential_secret));

    let shadow = format!("TESSIVUM_SHADOW_{}", Uuid::new_v4().simple());
    std::env::set_var(&shadow, "environment-secret");
    let shadowed = browser_call(
        &client,
        &base,
        "credentials-shadowed",
        "credentials.set",
        json!({"ref": shadow, "value": "shadow-file-secret"}),
    )
    .await;
    std::env::remove_var(&shadow);
    assert_eq!(shadowed["result"]["error"]["code"], "credential-rejected");
    assert!(!shadowed.to_string().contains("environment-secret"));
    assert!(!shadowed.to_string().contains("shadow-file-secret"));
    let unset = browser_call(
        &client,
        &base,
        "credentials-unset",
        "credentials.unset",
        json!({"ref": reference}),
    )
    .await;
    assert_eq!(unset["result"]["value"], json!({}));
    let invalid_mutation = browser_call(
        &client,
        &base,
        "settings-invalid-mutate",
        "settings.mutate",
        json!({"ns": namespace, "ops": [{"op": "remove", "path": ["added"]}]}),
    )
    .await;
    assert_eq!(invalid_mutation["result"]["error"]["code"], "bad-request");
    let too_many_refs = browser_call(
        &client,
        &base,
        "credentials-too-many",
        "credentials.describe",
        json!({"refs": (0..65).map(|index| format!("REF_{index}")).collect::<Vec<_>>() }),
    )
    .await;
    assert_eq!(too_many_refs["result"]["error"]["code"], "bad-request");

    server.shutdown().await.expect("server shuts down");
    runtime.shutdown().await.expect("host shuts down");
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_stop_quiesces_output_and_next_prompt_uses_fresh_generation() {
    let fixture = BrowserStopFixture::new();
    let adapter = Arc::new(DelayedAdapter::new());
    let mut config = HostConfig::new(&fixture.0, fixture.0.join("data"))
        .with_adapter_factory(Arc::new(DelayedFactory(Arc::clone(&adapter))));
    config.provider = "delayed".into();
    config.model = "delayed".into();
    let runtime = HostRuntime::boot(config).await.expect("real Host boots");
    let handle = runtime.handle();
    let host: Arc<dyn HostApi> = Arc::new(handle.clone());
    let mut server = ApiServer::bind(host).await.expect("real API binds");
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let mut downlink = RawWebSocket::connect_path(server.local_addr(), "/api/events.host").await;
    let session = "browser-stop";

    let created = browser_call(
        &client,
        &base,
        "stop-create",
        "session.create",
        json!({"sessionId": session}),
    )
    .await;
    assert_eq!(created["result"]["ok"], true);
    let prompted = browser_call(
        &client,
        &base,
        "stop-prompt",
        "session.prompt",
        json!({
            "sessionId": session,
            "mode": "queue",
            "content": [{"type": "text", "text": "block until stopped"}],
        }),
    )
    .await;
    assert_eq!(prompted["result"]["value"]["accepted"], true);
    timeout(Duration::from_secs(2), adapter.started.notified())
        .await
        .expect("delayed model starts");
    let running = wait_host_running(&mut downlink, session, true).await;
    assert_eq!(running["type"], "server-request");

    let stopped = browser_call(
        &client,
        &base,
        "stop-cancel",
        "session.cancel",
        json!({"sessionId": session}),
    )
    .await;
    assert_eq!(stopped["rpcId"], "stop-cancel");
    assert_eq!(stopped["result"]["value"]["accepted"], true);
    wait_host_running(&mut downlink, session, false).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let cancelled_events = handle
        .events(SessionId::from(session), 0)
        .await
        .expect("cancelled events remain durable");
    assert!(cancelled_events
        .iter()
        .all(|event| event.event_type != "assistant/message"));

    let duplicate = browser_call(
        &client,
        &base,
        "stop-duplicate",
        "session.cancel",
        json!({"sessionId": session}),
    )
    .await;
    assert_eq!(duplicate["result"]["value"]["accepted"], true);
    assert!(
        timeout(Duration::from_millis(100), downlink.read_text())
            .await
            .is_err(),
        "duplicate cancel does not publish another terminal status"
    );

    let resumed = browser_call(
        &client,
        &base,
        "stop-resume",
        "session.prompt",
        json!({
            "sessionId": session,
            "mode": "queue",
            "content": [{"type": "text", "text": "resume"}],
        }),
    )
    .await;
    assert_eq!(resumed["result"]["value"]["accepted"], true);
    wait_host_running(&mut downlink, session, true).await;
    wait_host_running(&mut downlink, session, false).await;
    let events = timeout(Duration::from_secs(2), async {
        loop {
            let events = handle.events(SessionId::from(session), 0).await.unwrap();
            if events.iter().any(|event| {
                event.event_type == "assistant/message"
                    && event.data.to_string().contains("fresh-generation-complete")
            }) {
                return events;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fresh generation completes");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/message")
            .count(),
        1
    );

    server.shutdown().await.expect("API shuts down");
    runtime.shutdown().await.expect("Host shuts down");
}

#[tokio::test]
async fn browser_goal_rpc_reports_missing_service() {
    let (mut server, _host, base) = start().await;
    let response = browser_call(
        &reqwest::Client::new(),
        &base,
        "goal-service-missing",
        "goal.create",
        json!({"sessionId": "session", "objective": "ship"}),
    )
    .await;
    assert_eq!(response["result"]["error"]["code"], "internal");
    server.shutdown().await.expect("server shuts down");
}

#[tokio::test]
async fn browser_goal_rpcs_are_cas_durable_and_structured() {
    let fixture = BrowserStopFixture::new();
    let adapter = Arc::new(DelayedAdapter::new());
    let mut config = HostConfig::new(&fixture.0, fixture.0.join("data"))
        .with_adapter_factory(Arc::new(DelayedFactory(Arc::clone(&adapter))));
    config.provider = "delayed".into();
    config.model = "delayed".into();
    let runtime = HostRuntime::boot(config).await.expect("real Host boots");
    let handle = runtime.handle();
    let host: Arc<dyn HostApi> = Arc::new(handle.clone());
    let mut server = ApiServer::bind(host).await.expect("real API binds");
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let session = "goal-api";

    let missing = browser_call(
        &client,
        &base,
        "goal-missing",
        "goal.create",
        json!({"sessionId": "missing-goal-session", "objective": "missing"}),
    )
    .await;
    assert_eq!(missing["result"]["error"]["code"], "session-not-found");

    let created_session = browser_call(
        &client,
        &base,
        "goal-session",
        "session.create",
        json!({"sessionId": session}),
    )
    .await;
    assert_eq!(created_session["result"]["ok"], true);
    let malformed = browser_call(
        &client,
        &base,
        "goal-malformed",
        "goal.create",
        json!({"sessionId": session, "objective": "ship", "unexpected": true}),
    )
    .await;
    assert_eq!(malformed["type"], "server-response");
    assert_eq!(malformed["rpcId"], "goal-malformed");
    assert_eq!(malformed["result"]["error"]["code"], "bad-request");
    assert_eq!(
        malformed["result"]["error"]["details"],
        json!({"issues": []})
    );

    let unsafe_rounds = browser_call(
        &client,
        &base,
        "goal-unsafe-rounds",
        "goal.create",
        json!({"sessionId": session, "objective": "ship", "maxGoalRounds": 9_007_199_254_740_992u64}),
    )
    .await;
    assert_eq!(unsafe_rounds["result"]["error"]["code"], "bad-request");

    let created = browser_call(
        &client,
        &base,
        "goal-create",
        "goal.create",
        json!({"sessionId": session, "objective": "ship", "maxGoalRounds": 3}),
    )
    .await;
    assert_eq!(created["result"]["ok"], true);
    let first = created["result"]["value"]["ref"].clone();
    assert_eq!(
        created["result"],
        json!({"ok": true, "value": {"ref": first.clone()}})
    );

    let other = "goal-api-other";
    browser_call(
        &client,
        &base,
        "goal-other-session",
        "session.create",
        json!({"sessionId": other}),
    )
    .await;
    let other_created = browser_call(
        &client,
        &base,
        "goal-other-create",
        "goal.create",
        json!({"sessionId": other, "objective": "other"}),
    )
    .await;
    assert_eq!(other_created["result"]["ok"], true);
    let foreign = browser_call(
        &client,
        &base,
        "goal-cross-session",
        "goal.pause",
        json!({"sessionId": other, "ref": first.clone()}),
    )
    .await;
    assert_eq!(foreign["result"]["error"]["code"], "internal");
    assert_eq!(
        foreign["result"]["error"]["details"]["goalCode"],
        "GOAL_NOT_FOUND"
    );
    assert_eq!(
        handle
            .events(SessionId::from(other), 0)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.event_type == "goal/change")
            .count(),
        1
    );

    let edited = browser_call(
        &client,
        &base,
        "goal-edit",
        "goal.edit",
        json!({"sessionId": session, "ref": first, "objective": "ship safely", "maxGoalRounds": 4}),
    )
    .await;
    let second = edited["result"]["value"]["ref"].clone();
    assert_eq!(second["revision"], 2);

    let stale = browser_call(
        &client,
        &base,
        "goal-stale",
        "goal.pause",
        json!({"sessionId": session, "ref": created["result"]["value"]["ref"]}),
    )
    .await;
    assert_eq!(stale["result"]["error"]["code"], "internal");
    assert_eq!(
        stale["result"]["error"]["details"]["goalCode"],
        "GOAL_STALE_REVISION"
    );

    let paused = browser_call(
        &client,
        &base,
        "goal-pause",
        "goal.pause",
        json!({"sessionId": session, "ref": second}),
    )
    .await;
    let repeated_pause = browser_call(
        &client,
        &base,
        "goal-repeated-pause",
        "goal.pause",
        json!({"sessionId": session, "ref": paused["result"]["value"]["ref"]}),
    )
    .await;
    assert_eq!(repeated_pause["result"]["error"]["code"], "internal");
    assert_eq!(
        repeated_pause["result"]["error"]["details"]["goalCode"],
        "GOAL_INVALID_TRANSITION"
    );
    let resumed = browser_call(
        &client,
        &base,
        "goal-resume",
        "goal.resume",
        json!({"sessionId": session, "ref": paused["result"]["value"]["ref"]}),
    )
    .await;
    let complete = browser_call(
        &client,
        &base,
        "goal-complete",
        "goal.complete",
        json!({"sessionId": session, "ref": resumed["result"]["value"]["ref"]}),
    )
    .await;
    let cleared = browser_call(
        &client,
        &base,
        "goal-clear",
        "goal.clear",
        json!({"sessionId": session, "ref": complete["result"]["value"]["ref"]}),
    )
    .await;
    assert_eq!(cleared["result"]["value"], json!({"cleared": true}));

    let goal_events = handle.events(SessionId::from(session), 0).await.unwrap();
    assert_eq!(
        goal_events
            .iter()
            .filter(|event| event.event_type == "goal/change")
            .count(),
        6
    );
    assert_eq!(goal_events.last().unwrap().data["operation"], "clear");
    assert_eq!(
        goal_events.last().unwrap().data["cleared"]["id"],
        complete["result"]["value"]["ref"]["id"]
    );
    assert_eq!(
        goal_events.last().unwrap().data["cleared"]["revision"]
            .as_u64()
            .unwrap(),
        complete["result"]["value"]["ref"]["revision"]
            .as_u64()
            .unwrap()
            + 1
    );
    assert!(goal_events.last().unwrap().data.get("goal").is_none());

    server.shutdown().await.expect("API shuts down");
    runtime.shutdown().await.expect("Host shuts down");
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
async fn unary_subagent_methods_are_strict_and_use_the_frozen_wire() {
    let (mut server, _, base) = start().await;
    let client = reqwest::Client::new();

    let history: Value = client
        .post(format!("{base}/api/subagent/history"))
        .json(&json!({"requestId": "history-1", "args": {
            "parentSessionId": "parent", "childSessionId": "child", "mode": "one-shot", "maxMessages": 1
        }}))
        .send()
        .await
        .expect("history response")
        .json()
        .await
        .expect("history JSON");
    assert_eq!(history["output"]["hasMore"], false);
    assert_eq!(history["output"]["projections"]["asOfSeq"], -1);

    let wrong_mode = client
        .post(format!("{base}/api/subagent/prompt"))
        .json(&json!({"requestId": "prompt-mode", "args": {
            "parentSessionId": "parent", "childSessionId": "child", "mode": "one-shot", "content": []
        }}))
        .send()
        .await
        .expect("wrong mode response");
    assert_eq!(wrong_mode.status(), reqwest::StatusCode::BAD_REQUEST);

    let prompt: Value = client
        .post(format!("{base}/api/subagent/prompt"))
        .json(&json!({"requestId": "prompt-1", "args": {
            "parentSessionId": "parent", "childSessionId": "child", "mode": "continuable", "content": [{"type": "text", "text": "continue"}]
        }}))
        .send()
        .await
        .expect("prompt response")
        .json()
        .await
        .expect("prompt JSON");
    assert_eq!(prompt["output"]["messageId"], "subagent-message");

    let interrupt: Value = client
        .post(format!("{base}/api/subagent/interrupt"))
        .json(&json!({"requestId": "interrupt-1", "args": {
            "parentSessionId": "parent", "childSessionId": "child", "mode": "continuable"
        }}))
        .send()
        .await
        .expect("interrupt response")
        .json()
        .await
        .expect("interrupt JSON");
    assert_eq!(interrupt["output"]["accepted"], true);

    server.shutdown().await.expect("server shuts down");
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
async fn websocket_streams_notifications_preserves_inflight_prompt_and_exits_on_shutdown() {
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
    let mut disconnecting = RawWebSocket::connect(server.local_addr()).await;
    let started = host.prompt_started.notified();
    disconnecting
        .send_json(json!({
            "requestId": "ws-background",
            "namespace": "session",
            "method": "prompt",
            "args": {"sessionId": "background-session", "contentBlocks": [{"type": "text", "text": "continue"}]}
        }))
        .await;
    timeout(Duration::from_secs(1), started)
        .await
        .expect("prompt starts");
    drop(disconnecting);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        host.cancels.load(Ordering::SeqCst),
        0,
        "disconnect must not cancel accepted Agent work"
    );

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
        let authority = address.to_string();
        let origin = format!("http://{authority}");
        let origins = [origin.as_str()];
        match Self::connect_path_with_headers(address, path, &authority, &origins).await {
            Ok(socket) => socket,
            Err(status) => panic!("WebSocket handshake rejected: {status}"),
        }
    }

    async fn connect_path_with_headers(
        address: SocketAddr,
        path: &str,
        host: &str,
        origins: &[&str],
    ) -> Result<Self, String> {
        let stream = TcpStream::connect(address)
            .await
            .expect("WebSocket TCP connects");
        let mut stream = BufReader::new(stream);
        let origins = origins
            .iter()
            .map(|origin| format!("Origin: {origin}\r\n"))
            .collect::<String>();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\n{origins}Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
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
        let mut status = String::new();
        stream
            .read_line(&mut status)
            .await
            .expect("WebSocket status reads");
        if !status.contains(" 101 ") {
            return Err(status);
        }
        loop {
            let mut header = Vec::new();
            stream
                .read_until(b'\n', &mut header)
                .await
                .expect("WebSocket header reads");
            if header == b"\r\n" {
                break;
            }
        }
        Ok(Self { stream })
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

#[tokio::test]
async fn browser_dynamic_cordis_wire_cannot_execute_code() {
    let fixture = BrowserStopFixture::new();
    let runtime = Arc::new(
        HostRuntime::boot(HostConfig::new(&fixture.0, fixture.0.join("data")))
            .await
            .unwrap(),
    );
    let mut server = ApiServer::bind(runtime.clone()).await.unwrap();
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();

    let inventory = browser_call(
        &client,
        &base,
        "cordis-inventory",
        "dynamicCordisRunner/inventory",
        json!({"args": {}}),
    )
    .await;
    assert_eq!(inventory["result"]["value"], json!([]));
    let run = browser_call(
        &client,
        &base,
        "cordis-run",
        "dynamicCordisRunner/runHostHalf",
        json!({"args": {}}),
    )
    .await;
    assert_eq!(run["result"]["error"]["code"], "cordis-unavailable");

    server.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn browser_agent_mode_rpc_is_durable_and_truthful() {
    let fixture = BrowserStopFixture::new();
    let data = fixture.0.join("data");
    let system_root = fixture.0.join("mode-roots/system");
    let system = system_root.join("base");
    fs::create_dir_all(&system).unwrap();
    fs::write(
        system.join("mode.toml"),
        "schema = 1\nid = \"base\"\nname = \"Base\"\ndescription = \"system mode\"\n\n[prompt]\ncomplete = false\ntext = \"Base mode.\"\n\n[tools]\npresentation = \"direct\"\nenabled = []\n\n[capabilities]\nskills = false\nplanning = false\ncompaction = false\n",
    )
    .unwrap();

    let opened_paths = Arc::new(ParkingMutex::new(Vec::new()));
    let opened = Arc::clone(&opened_paths);
    let runtime = HostRuntime::boot(
        HostConfig::new(&fixture.0, &data)
            .with_agent_mode_root(&system_root, AgentModeTrust::System)
            .with_path_opener(Arc::new(move |path: &std::path::Path| {
                opened.lock().push(path.to_path_buf());
                Ok(())
            })),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut server = ApiServer::bind(Arc::new(handle.clone())).await.unwrap();
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();

    let listed = browser_call(&client, &base, "modes", "agentPreset.list", json!({})).await;
    let presets = listed["result"]["value"]["presets"].as_array().unwrap();
    let standard = presets
        .iter()
        .find(|mode| mode["id"] == "standard")
        .unwrap();
    assert_eq!(standard["trust"], "system");
    assert_eq!(standard["isDefault"], true);
    assert!(presets.iter().any(|mode| mode["id"] == "base"));
    assert_eq!(listed["result"]["value"]["authorable"], true);
    assert_eq!(listed["result"]["value"]["hasDocument"], true);
    let standard_read = browser_call(
        &client,
        &base,
        "read-standard",
        "agentPreset.read",
        json!({"agentPreset":"standard"}),
    )
    .await;
    assert_eq!(standard_read["result"]["value"]["trust"], "system");
    assert!(standard_read["result"]["value"]["content"]
        .as_str()
        .unwrap()
        .contains("id = \"standard\""));
    let defaulted = browser_call(
        &client,
        &base,
        "default-minimal",
        "settings.update",
        json!({"ns":"agent-presets","patch":{"default":"minimal"}}),
    )
    .await;
    assert_eq!(defaulted["result"]["value"]["value"]["default"], "minimal");
    let relisted = browser_call(
        &client,
        &base,
        "modes-defaulted",
        "agentPreset.list",
        json!({}),
    )
    .await;
    assert!(relisted["result"]["value"]["presets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|mode| mode["id"] == "minimal" && mode["isDefault"] == true));
    let copied = browser_call(
        &client,
        &base,
        "copy",
        "agentPreset.copy",
        json!({"from":"base","agentPreset":"working","name":"Working"}),
    )
    .await;
    assert_eq!(
        copied["result"],
        json!({"ok": true, "value": {"agentPreset": "working"}})
    );
    let defaulted_custom = browser_call(
        &client,
        &base,
        "default-working",
        "settings.update",
        json!({"ns":"agent-presets","patch":{"default":"working"}}),
    )
    .await;
    assert_eq!(defaulted_custom["result"]["ok"], true);
    let default_remove = browser_call(
        &client,
        &base,
        "remove-default",
        "agentPreset.remove",
        json!({"agentPreset":"working"}),
    )
    .await;
    assert_eq!(default_remove["result"]["error"]["code"], "mode-in-use");
    let reset_default = browser_call(
        &client,
        &base,
        "default-minimal-again",
        "settings.update",
        json!({"ns":"agent-presets","patch":{"default":"minimal"}}),
    )
    .await;
    assert_eq!(reset_default["result"]["ok"], true);
    let read = browser_call(
        &client,
        &base,
        "read",
        "agentPreset.read",
        json!({"agentPreset":"working"}),
    )
    .await;
    assert_eq!(read["result"]["ok"], true);
    let value = &read["result"]["value"];
    assert_eq!(value["agentPreset"], "working");
    assert_eq!(value["trust"], "user");
    assert_eq!(value["name"], "Working");
    assert_eq!(value["description"], "system mode");
    assert!(value["content"]
        .as_str()
        .unwrap()
        .contains("name = \"Working\""));
    let opened = browser_call(
        &client,
        &base,
        "open",
        "agentPreset.openDocument",
        json!({"agentPreset":"working"}),
    )
    .await;
    assert_eq!(
        opened["result"],
        json!({"ok": true, "value": {"opened": true}})
    );
    assert_eq!(opened_paths.lock().len(), 1);
    assert!(opened_paths.lock()[0].ends_with("data/modes/working/mode.toml"));
    let unknown = browser_call(
        &client,
        &base,
        "read-unknown",
        "agentPreset.read",
        json!({"agentPreset":"missing"}),
    )
    .await;
    assert_eq!(unknown["result"]["error"]["code"], "mode-not-found");
    for (rpc_id, method, payload) in [
        (
            "read-extra",
            "agentPreset.read",
            json!({"agentPreset":"working","unexpected":true}),
        ),
        (
            "copy-extra",
            "agentPreset.copy",
            json!({"from":"base","agentPreset":"next","unexpected":true}),
        ),
        (
            "open-extra",
            "agentPreset.openDocument",
            json!({"agentPreset":"working","unexpected":true}),
        ),
        (
            "remove-extra",
            "agentPreset.remove",
            json!({"agentPreset":"working","unexpected":true}),
        ),
        (
            "select-extra",
            "agentPreset.select",
            json!({"sessionId":"preset-cold","agentPreset":"working","unexpected":true}),
        ),
    ] {
        let rejected = browser_call(&client, &base, rpc_id, method, payload).await;
        assert_eq!(rejected["result"]["error"]["code"], "bad-request");
        assert_eq!(
            rejected["result"]["error"]["details"],
            json!({"issues": []})
        );
    }
    let blank = browser_call(
        &client,
        &base,
        "copy-blank",
        "agentPreset.copy",
        json!({"from":"","agentPreset":"next"}),
    )
    .await;
    assert_eq!(blank["result"]["error"]["code"], "bad-request");
    let traversal = browser_call(
        &client,
        &base,
        "traversal",
        "agentPreset.copy",
        json!({"from":"base","agentPreset":"../escape"}),
    )
    .await;
    assert_eq!(traversal["result"]["error"]["code"], "mode-invalid-id");
    let readonly = browser_call(
        &client,
        &base,
        "readonly",
        "agentPreset.remove",
        json!({"agentPreset":"base"}),
    )
    .await;
    assert_eq!(readonly["result"]["error"]["code"], "mode-read-only");
    let system_document = browser_call(
        &client,
        &base,
        "system-document",
        "agentPreset.openDocument",
        json!({"agentPreset":"base"}),
    )
    .await;
    assert_eq!(system_document["result"]["error"]["code"], "mode-read-only");

    let created = browser_call(
        &client,
        &base,
        "cold-create",
        "session.create",
        json!({"sessionId":"preset-cold"}),
    )
    .await;
    assert_eq!(created["result"]["ok"], true);
    let renamed = browser_call(
        &client,
        &base,
        "cold-rename",
        "session.rename",
        json!({"sessionId":"preset-cold","title":"Cold preset session"}),
    )
    .await;
    assert_eq!(
        renamed["result"]["value"]["title"], "Cold preset session",
        "{renamed}"
    );
    let invalid = browser_call(
        &client,
        &base,
        "invalid-mode",
        "agentPreset.select",
        json!({"sessionId":"preset-cold","agentPreset":"missing"}),
    )
    .await;
    assert_eq!(invalid["result"]["error"]["code"], "mode-not-found");
    assert!(!handle
        .events(SessionId::from("preset-cold"), 0)
        .await
        .unwrap()
        .iter()
        .any(|event| event.event_type == "agent-mode/selected"));
    let selected = browser_call(
        &client,
        &base,
        "cold-select",
        "agentPreset.select",
        json!({"sessionId":"preset-cold","agentPreset":"working"}),
    )
    .await;
    assert_eq!(
        selected["result"],
        json!({"ok": true, "value": {"agentPreset": "working"}})
    );
    let selected_events = handle
        .events(SessionId::from("preset-cold"), 0)
        .await
        .unwrap();
    assert!(selected_events.iter().any(|event| {
        event.event_type == "agent-mode/selected" && event.data == json!({"agentMode": "working"})
    }));
    let skill = fixture.0.join(".agents/skills/demo");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill\n---\nBody\n",
    )
    .unwrap();
    let skills = browser_call(
        &client,
        &base,
        "disabled-mode-skills",
        "skill.list",
        json!({"sessionId":"preset-cold"}),
    )
    .await;
    assert_eq!(skills["result"]["value"]["skills"], json!([]));
    let listed_sessions =
        browser_call(&client, &base, "selected-list", "session.list", json!({})).await;
    assert_eq!(
        listed_sessions["result"]["value"]["items"][0]["agentPreset"],
        "working"
    );
    let projections = &listed_sessions["result"]["value"]["items"][0]["projections"];
    assert_eq!(projections["asOfSeq"], selected_events.last().unwrap().seq);
    assert_eq!(projections["values"]["title"], "Cold preset session");
    assert_eq!(
        projections["values"]["permissions"]["currentValue"],
        "workspace-write"
    );
    assert_eq!(projections["values"]["plan"]["active"], false);
    assert_eq!(projections["values"]["todos"], Value::Null);
    let prompted = browser_call(&client, &base, "live-prompt", "session.prompt", json!({"sessionId":"preset-cold","mode":"queue","content":[{"type":"text","text":"hello"}]})).await;
    assert_eq!(prompted["result"]["ok"], true);
    let locked = browser_call(
        &client,
        &base,
        "live-select",
        "agentPreset.select",
        json!({"sessionId":"preset-cold","agentPreset":"working"}),
    )
    .await;
    assert_eq!(locked["result"]["error"]["code"], "mode-selection-locked");

    server.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
    let runtime = HostRuntime::boot(HostConfig::new(&fixture.0, &data))
        .await
        .unwrap();
    let handle = runtime.handle();
    assert!(handle
        .events(SessionId::from("preset-cold"), 0)
        .await
        .unwrap()
        .iter()
        .any(|event| event.event_type == "agent-mode/selected"));
    let mut server = ApiServer::bind(Arc::new(handle)).await.unwrap();
    let base = format!("http://{}", server.local_addr());
    let removed = browser_call(
        &client,
        &base,
        "remove",
        "agentPreset.remove",
        json!({"agentPreset":"working"}),
    )
    .await;
    assert_eq!(removed["result"]["ok"], true);
    server.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
    let runtime = HostRuntime::boot(HostConfig::new(&fixture.0, &data))
        .await
        .unwrap();
    assert!(!runtime
        .handle()
        .agent_mode_list()
        .await
        .unwrap()
        .iter()
        .any(|mode| mode.id == "working"));
    runtime.shutdown().await.unwrap();
}

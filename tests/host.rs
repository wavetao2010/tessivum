use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_util::stream;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tessivum::{
    agent_preset::AgentPresetTrust,
    approval::ApprovalPolicy,
    credentials::{CredentialError, CredentialRef},
    goal::GoalError,
    host::{
        HostApi, HostConfig, HostDirectoryPicker, HostLlmAdapterFactory, HostNotification,
        HostPathOpener, HostRuntime, HostSettingsMutation, SessionQueueAction,
        SessionUpdateQueueParams,
    },
    llm::{LlmAdapter, LlmStream},
    persistence_jsonl::JsonlSessionPersistence,
    protocol::{
        AgentCancelCause, ContentBlock, GenerateRequest, SessionEvent, SessionHeader,
        SessionModelSelection, SessionPromptParams, SessionStatus, SurfaceOp, ToolCallId,
        SESSION_FORMAT_VERSION,
    },
    session::SessionPersistence,
    settings::{
        Settings, SettingsError, SettingsEventKind, SettingsProvider, SettingsRegistration,
        AGENT_DEFAULT_MODEL_NAMESPACE, LLM_PI_AI_NAMESPACE,
    },
    subagent::{SubagentHistoryRequest, SubagentMode},
    SessionId, TessivumError,
};
use tessivum_core::{
    CancellationToken, ContextHandle, Entry, EntryId, EntryOptions, EntryTree, RuntimeKind,
};
use uuid::Uuid;

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes
}

const REPLAY: &str = include_str!("../fixtures/headless/recorded-replay.jsonl");
const RESUME_REPLAY: &str = concat!(
    "{\"requestId\":\"one\",\"chunk\":{\"type\":\"block-start\",\"index\":0,\"blockType\":\"text\"}}\n",
    "{\"requestId\":\"one\",\"chunk\":{\"type\":\"text-delta\",\"index\":0,\"text\":\"first\"}}\n",
    "{\"requestId\":\"one\",\"chunk\":{\"type\":\"block-end\",\"index\":0,\"block\":{\"type\":\"text\",\"text\":\"first\"}}}\n",
    "{\"requestId\":\"one\",\"chunk\":{\"type\":\"finish\",\"reason\":{\"kind\":\"stop\"}}}\n",
    "{\"requestId\":\"two\",\"chunk\":{\"type\":\"block-start\",\"index\":0,\"blockType\":\"text\"}}\n",
    "{\"requestId\":\"two\",\"chunk\":{\"type\":\"text-delta\",\"index\":0,\"text\":\"second\"}}\n",
    "{\"requestId\":\"two\",\"chunk\":{\"type\":\"block-end\",\"index\":0,\"block\":{\"type\":\"text\",\"text\":\"second\"}}}\n",
    "{\"requestId\":\"two\",\"chunk\":{\"type\":\"finish\",\"reason\":{\"kind\":\"stop\"}}}\n",
);
const DURABLE_REPLAY: &str = concat!(
    "{\"type\":\"session\",\"version\":0,\"id\":\"recorded\",\"createdAt\":0}\n",
    "{\"type\":\"assistant/chunk\",\"seq\":0,\"time\":0,\"data\":{\"turn\":1,\"step\":1,\"chunk\":{\"type\":\"block-start\",\"index\":0,\"blockType\":\"text\"}}}\n",
    "{\"type\":\"assistant/chunk\",\"seq\":1,\"time\":1,\"data\":{\"turn\":1,\"step\":1,\"chunk\":{\"type\":\"text-delta\",\"index\":0,\"text\":\"durable replay\"}}}\n",
    "{\"type\":\"assistant/chunk\",\"seq\":2,\"time\":2,\"data\":{\"turn\":1,\"step\":1,\"chunk\":{\"type\":\"block-end\",\"index\":0,\"block\":{\"type\":\"text\",\"text\":\"durable replay\"}}}}\n",
    "{\"type\":\"assistant/chunk\",\"seq\":3,\"time\":3,\"data\":{\"turn\":1,\"step\":1,\"chunk\":{\"type\":\"finish\",\"reason\":{\"kind\":\"stop\"}}}}\n",
);

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tessivum-host-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct FailingSettingsProvider;

#[async_trait]
impl SettingsProvider for FailingSettingsProvider {
    async fn load(&self, _: &str) -> Result<Option<serde_json::Value>, SettingsError> {
        Ok(None)
    }

    async fn persist(&self, _: &str, _: &serde_json::Value) -> Result<(), SettingsError> {
        Err(SettingsError::Persistence("write failed".into()))
    }
}

struct RecordingPathOpener {
    available: std::sync::atomic::AtomicBool,
    paths: Mutex<Vec<PathBuf>>,
    text_paths: Mutex<Vec<PathBuf>>,
}

impl RecordingPathOpener {
    fn new() -> Self {
        Self {
            available: std::sync::atomic::AtomicBool::new(true),
            paths: Mutex::new(Vec::new()),
            text_paths: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl HostPathOpener for RecordingPathOpener {
    fn can_open_path(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }
    async fn open_path(&self, path: PathBuf) -> Result<(), TessivumError> {
        self.paths.lock().push(path);
        Ok(())
    }
    async fn open_text_file(&self, path: PathBuf) -> Result<(), TessivumError> {
        self.text_paths.lock().push(path);
        Ok(())
    }
}

struct TestDirectoryPicker(Mutex<Option<PathBuf>>);

#[async_trait]
impl HostDirectoryPicker for TestDirectoryPicker {
    async fn pick_directory(&self) -> Result<Option<PathBuf>, TessivumError> {
        Ok(self.0.lock().clone())
    }
}

struct ProviderModelsAdapter {
    calls: Arc<AtomicUsize>,
    configs: Arc<Mutex<Vec<Value>>>,
    results: Mutex<Vec<Result<Value, TessivumError>>>,
}

impl ProviderModelsAdapter {
    fn new(results: Vec<Result<Value, TessivumError>>) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            configs: Arc::new(Mutex::new(Vec::new())),
            results: Mutex::new(results),
        }
    }
}

#[async_trait]
impl LlmAdapter for ProviderModelsAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        Err(TessivumError::new(
            "UNEXPECTED_GENERATE",
            "provider model tests do not generate",
            "test",
            Value::Null,
        ))
    }

    async fn models(&self, config: Value) -> Result<Value, TessivumError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.configs.lock().push(config);
        self.results.lock().remove(0)
    }
}

struct ProviderModelsFactory(Arc<ProviderModelsAdapter>);

impl HostLlmAdapterFactory for ProviderModelsFactory {
    fn create(&self, _: &str, _: &str) -> Result<Arc<dyn LlmAdapter>, TessivumError> {
        Ok(self.0.clone())
    }
}

struct BlockingAdapter {
    started: tokio::sync::Notify,
}

impl BlockingAdapter {
    fn new() -> Self {
        Self {
            started: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait]
impl LlmAdapter for BlockingAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        self.started.notify_one();
        Ok(Box::pin(stream::once(async move {
            cancellation.cancelled().await;
            Err(TessivumError::new(
                "CANCELLED",
                "test stream cancelled",
                "test",
                Value::Null,
            ))
        })))
    }
}

struct BlockingFactory(Arc<BlockingAdapter>);

impl HostLlmAdapterFactory for BlockingFactory {
    fn create(&self, _: &str, _: &str) -> Result<Arc<dyn LlmAdapter>, TessivumError> {
        Ok(self.0.clone())
    }
}

fn config(root: &TempDir) -> HostConfig {
    let mut config =
        HostConfig::new(root.path(), root.path().join("data")).with_recorded_replay(REPLAY);
    config.provider = "cli-mock".into();
    config.model = "cli-mock".into();
    config.enable_trusted_bash = true;
    config
}

fn dynamic_config(root: &TempDir) -> HostConfig {
    let mut config = HostConfig::new(root.path(), root.path().join("data"));
    config.provider = "openai-responses".into();
    config.model = "alpha".into();
    config.profile_patch = json!({
        "llm-pi-ai": {
            "providers": {
                "openai-responses": {
                    "displayName": "Test relay",
                    "baseURL": "http://127.0.0.1:1/v1",
                    "apiKeyEnv": "TESSIVUM_DYNAMIC_TEST_KEY",
                    "models": [
                        {"id": "alpha", "input": ["text"]},
                        {"id": "beta", "input": ["text", "image"]}
                    ]
                }
            }
        }
    });
    config
}

async fn wait_for_models_changed(
    notifications: &mut tokio::sync::broadcast::Receiver<HostNotification>,
) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                notifications.recv().await.unwrap(),
                HostNotification::ModelsChanged
            ) {
                return;
            }
        }
    })
    .await
    .expect("models change notification arrives");
}

async fn wait_for_adapters_updated(
    notifications: &mut tokio::sync::broadcast::Receiver<HostNotification>,
) {
    tokio::time::timeout(Duration::from_secs(1), async {
        let mut settings_changed = false;
        let mut adapter_updates = 0;
        let mut adapters_updated = false;
        while !settings_changed || !adapters_updated {
            match notifications.recv().await.unwrap() {
                HostNotification::SettingsChanged(event)
                    if event.namespace == LLM_PI_AI_NAMESPACE =>
                {
                    settings_changed = true;
                }
                HostNotification::AdaptersUpdated => {
                    adapters_updated = true;
                    adapter_updates += 1;
                }
                _ => {}
            }
        }
        assert_eq!(adapter_updates, 1);
    })
    .await
    .expect("committed settings and adapter update notifications arrive");
}

fn prompt(session: &str) -> SessionPromptParams {
    SessionPromptParams {
        session_id: SessionId::from(session),
        content_blocks: vec![ContentBlock::Text {
            text: "exercise host receipt".into(),
        }],
        client_time_zone: None,
    }
}

fn persisted_header(id: &str, cwd: Option<String>) -> SessionHeader {
    SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: SessionId::from(id),
        created_at: 0,
        cwd,
        parent_session: None,
        seed_length: None,
        origin: None,
        delegation_depth: Some(0),
        agent_preset: None,
    }
}

fn surface_event(event_type: &str, seq: u64, text: &str) -> SessionEvent {
    let message = match event_type {
        "user/message" => json!({
            "id": format!("message-{seq}"),
            "role": "user",
            "content": [{"type": "text", "text": text}],
            "source": {"kind": "user"},
        }),
        "assistant/message" => json!({"message": {
            "id": format!("message-{seq}"),
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "source": {"kind": "model", "provider": "cli-mock", "model": "cli-mock"},
        }}),
        _ => panic!("unsupported surface event type: {event_type}"),
    };
    SessionEvent {
        event_type: event_type.into(),
        seq,
        time: seq,
        data: message,
        ignorable: None,
        source_event_seqs: None,
        surface_op: Some(SurfaceOp::Append),
    }
}

async fn persist_session(
    persistence: &JsonlSessionPersistence,
    header: SessionHeader,
    events: impl IntoIterator<Item = SessionEvent>,
) {
    persistence
        .create(&header, ContextHandle::root().scope().cancellation())
        .await
        .unwrap();
    for event in events {
        persistence
            .append(
                &header.id,
                &event,
                ContextHandle::root().scope().cancellation(),
            )
            .await
            .unwrap();
    }
}

async fn wait_for_event(host: &impl HostApi, session: SessionId) {
    for _ in 0..100 {
        if host
            .events(session.clone(), 0)
            .await
            .unwrap()
            .iter()
            .any(|event| event.event_type == "turn/end")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("recorded prompt did not complete a turn");
}

#[test]
fn profile_patches_have_fixed_precedence() {
    let root = TempDir::new();
    let mut config = HostConfig::new(root.path(), root.path().join("data"));
    config.bundle_patch = json!({"value":"bundle","nested":{"bundle":true,"winner":"bundle"}});
    config.profile_patch = json!({"value":"profile","nested":{"profile":true,"winner":"profile"}});
    config.home_patch = json!({"value":"home","nested":{"home":true,"winner":"home"}});
    config.cli_patches = vec![json!({"value":"cli","nested":{"cli":true,"winner":"cli"}})];
    config.telemetry_patch =
        json!({"value":"telemetry","nested":{"telemetry":true,"winner":"telemetry"}});
    assert_eq!(
        config.compose_profile().unwrap(),
        json!({
            "value":"telemetry",
            "nested":{"bundle":true,"profile":true,"home":true,"cli":true,"telemetry":true,"winner":"telemetry"}
        })
    );
}

#[tokio::test]
async fn disabled_plugin_inventory_has_no_live_fiber_phase() {
    let root = TempDir::new();
    let mut host = config(&root);
    host.entries = Some(EntryTree {
        entries: vec![Entry {
            package: "disabled-fixture".into(),
            options: EntryOptions {
                id: EntryId::new("disabled-fixture").unwrap(),
                name: Some("disabled-fixture".into()),
                runtime: RuntimeKind::Native,
                config: json!({}),
                inject: Vec::new(),
                isolate: Vec::new(),
                intercept: json!({}),
                disabled: true,
                group: None,
            },
        }],
        groups: Vec::new(),
    });
    let runtime = HostRuntime::boot(host).await.unwrap();

    assert_eq!(
        runtime.handle().plugin_inventory().await.unwrap(),
        vec![json!({
            "entryId": "disabled-fixture",
            "moduleName": "disabled-fixture",
            "enabled": false,
            "fiberPhase": null,
        })]
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_subagent_history_preserves_unknown_child_error() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let error = runtime
        .handle()
        .subagent_history(SubagentHistoryRequest {
            parent_session_id: SessionId::from("parent"),
            child_session_id: SessionId::from("missing-child"),
            mode: SubagentMode::OneShot,
            before_seq: None,
            max_messages: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, "SESSION_NOT_FOUND");
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn prompt_receipt_relays_committed_events_and_flushes_on_shutdown() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root.path().join("data"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
    let handle = runtime.handle();
    let mut notifications = handle.subscribe();
    let receipt = handle.prompt(prompt("host-prompt")).await.unwrap();
    assert!(!receipt.message_id.as_str().is_empty());
    let events = handle
        .events(SessionId::from("host-prompt"), 0)
        .await
        .unwrap();
    assert!(
        events.iter().any(|event| event.event_type == "user/message"
            && event.data.get("id").and_then(serde_json::Value::as_str)
                == Some(receipt.message_id.as_str())),
        "a returned receipt must survive an immediate process crash/read race"
    );
    let notification = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let tessivum::host::HostNotification::SessionEvent(notification) =
                notifications.recv().await.unwrap()
            {
                return notification;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(notification.session_id, SessionId::from("host-prompt"));
    runtime.shutdown().await.unwrap();
    let reopened = HostRuntime::boot(config(&root)).await.unwrap();
    assert!(!reopened
        .handle()
        .events(SessionId::from("host-prompt"), 0)
        .await
        .unwrap()
        .is_empty());
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn queue_steer_persists_one_atomic_action_splice() {
    let root = TempDir::new();
    let adapter = Arc::new(BlockingAdapter::new());
    let mut host_config = HostConfig::new(root.path(), root.path().join("data"))
        .with_adapter_factory(Arc::new(BlockingFactory(adapter.clone())));
    host_config.provider = "queue-test".into();
    host_config.model = "queue-test".into();
    let runtime = HostRuntime::boot(host_config).await.unwrap();
    let handle = runtime.handle();
    let session_id = SessionId::from("atomic-queue-steer");
    handle.prompt(prompt(session_id.as_str())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), adapter.started.notified())
        .await
        .expect("first turn starts");
    let queued = handle.prompt(prompt(session_id.as_str())).await.unwrap();

    assert!(
        handle
            .update_queue(SessionUpdateQueueParams {
                session_id: session_id.clone(),
                item_id: queued.message_id.clone(),
                action: SessionQueueAction::Steer,
            })
            .await
            .unwrap()
            .accepted
    );
    let splices = handle
        .events(session_id, 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| {
            event.event_type == "agent/inbox/spliced"
                && event.data["itemId"] == json!(queued.message_id.as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(splices.len(), 1);
    assert_eq!(splices[0].data["target"], "next-step");
    assert_eq!(splices[0].data["action"], "steer");
    assert_eq!(splices[0].data["message"]["id"], queued.message_id.as_str());
    assert_eq!(splices[0].data["start"], 0);
    assert_eq!(splices[0].data["removedCount"], 0);
    assert_eq!(
        splices[0].data["inserted"][0]["id"],
        queued.message_id.as_str()
    );

    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_accepts_a_durable_session_log_as_its_replay_fixture() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(
        HostConfig::new(root.path(), root.path().join("data")).with_recorded_replay(DURABLE_REPLAY),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let session = SessionId::from("durable-replay");
    handle.prompt(prompt(session.as_str())).await.unwrap();

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if handle
                .events(session.clone(), 0)
                .await
                .unwrap()
                .iter()
                .any(|event| {
                    event.event_type == "assistant/message"
                        && event.data.to_string().contains("durable replay")
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_relay_replays_startup_asked_without_durable_tool_details() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root).with_approval_required_tool("bash"))
        .await
        .unwrap();
    let handle = runtime.handle();
    let mut notifications = handle.subscribe();
    handle.prompt(prompt("approval-relay")).await.unwrap();

    let requested = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let tessivum::host::HostNotification::ApprovalRequested(requested) =
                notifications.recv().await.unwrap()
            {
                return requested;
            }
        }
    })
    .await
    .unwrap();
    let asked = handle
        .events(SessionId::from("approval-relay"), 0)
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == "approval/asked")
        .unwrap()
        .data;
    assert_eq!(asked["approvalId"], json!(requested.approval_id.as_str()));
    assert_eq!(asked["sessionId"], json!("approval-relay"));
    assert_eq!(asked["toolName"], json!("bash"));
    assert_eq!(asked["callId"], json!("cli-smoke-call"));
    assert_eq!(asked["request"], json!({"action": "bash"}));
    assert!(!asked.to_string().contains("CLI_TOOL_ROUND_TRIP"));

    assert!(
        handle
            .approval_registry()
            .unwrap()
            .respond(
                &requested.rpc_id,
                &requested.session_id,
                &requested.approval_id,
                tessivum::approval::ApprovalOutcome::Rejected,
            )
            .accepted
    );
    let resolved = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let tessivum::host::HostNotification::ApprovalResolved(resolved) =
                notifications.recv().await.unwrap()
            {
                return resolved;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(resolved.approval_id, requested.approval_id);
    assert_eq!(
        resolved.outcome,
        tessivum::approval::ApprovalOutcome::Rejected
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn runtime_resumes_a_durable_session_without_replacing_it() {
    let root = TempDir::new();
    let first = HostRuntime::boot(config(&root)).await.unwrap();
    first.handle().prompt(prompt("host-resume")).await.unwrap();
    wait_for_event(&first.handle(), SessionId::from("host-resume")).await;
    let first_len = first
        .handle()
        .events(SessionId::from("host-resume"), 0)
        .await
        .unwrap()
        .len();
    first.shutdown().await.unwrap();
    let second = HostRuntime::boot(config(&root)).await.unwrap();
    second.handle().prompt(prompt("host-resume")).await.unwrap();
    for _ in 0..100 {
        if second
            .handle()
            .events(SessionId::from("host-resume"), 0)
            .await
            .unwrap()
            .len()
            > first_len
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        second
            .handle()
            .events(SessionId::from("host-resume"), 0)
            .await
            .unwrap()
            .len()
            > first_len
    );
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_cancel_is_a_noop_and_session_accepts_next_prompt() {
    let root = TempDir::new();
    let config =
        HostConfig::new(root.path(), root.path().join("data")).with_recorded_replay(RESUME_REPLAY);
    let runtime = HostRuntime::boot(config).await.unwrap();
    let handle = runtime.handle();
    let session = SessionId::from("cancel-resume");

    handle.prompt(prompt(session.as_str())).await.unwrap();
    for _ in 0..100 {
        if handle.status(session.clone()).await.unwrap() == Some(SessionStatus::Idle)
            && handle
                .events(session.clone(), 0)
                .await
                .unwrap()
                .iter()
                .any(|event| {
                    event.event_type == "assistant/message"
                        && event.data.to_string().contains("first")
                })
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!handle
        .cancel(session.clone(), AgentCancelCause::User)
        .await
        .unwrap());
    handle.prompt(prompt(session.as_str())).await.unwrap();
    for _ in 0..100 {
        if handle
            .events(session.clone(), 0)
            .await
            .unwrap()
            .iter()
            .any(|event| {
                event.event_type == "assistant/message" && event.data.to_string().contains("second")
            })
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(handle
        .events(session, 0)
        .await
        .unwrap()
        .iter()
        .any(|event| {
            event.event_type == "assistant/message" && event.data.to_string().contains("second")
        }));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_fences_new_admission_and_leaves_no_owned_processes() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = runtime.handle();
    handle.prompt(prompt("host-fence")).await.unwrap();
    let shutting = handle.clone();
    let task = tokio::spawn(async move { shutting.shutdown().await });
    for _ in 0..100 {
        if handle.is_shutting_down() {
            break;
        }
        tokio::task::yield_now().await;
    }
    let error = handle
        .prompt(prompt("rejected-after-shutdown"))
        .await
        .unwrap_err();
    assert_eq!(error.code, "HOST_SHUTTING_DOWN");
    task.await.unwrap().unwrap();
    assert_eq!(handle.in_flight(), 0);
    assert!(
        fs::remove_dir_all(root.path()).is_ok(),
        "all host-owned file/process resources release before shutdown returns"
    );
}

#[tokio::test]
async fn shutdown_drains_racing_settings_writes_before_relays_close() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = runtime.handle();
    let settings = handle.settings().unwrap();
    let credentials = handle.credentials().unwrap();
    let namespace = "llm-shutdown-race";
    let reference =
        CredentialRef::new(format!("TESSIVUM_RACE_{}", Uuid::new_v4().simple())).unwrap();
    settings
        .register(SettingsRegistration::new(
            namespace,
            json!({}),
            json!({}),
            json!({}),
        ))
        .await
        .unwrap();
    let mut notifications = handle.subscribe();

    let (settings_result, credentials_result, shutdown_result) = tokio::join!(
        settings.update(namespace, json!({"saved": true}), None),
        credentials.set(reference.clone(), "racing-credential".into()),
        runtime.shutdown(),
    );
    settings_result.unwrap();
    credentials_result.unwrap();
    shutdown_result.unwrap();

    let mut settings_changed = false;
    let mut credentials_changed = false;
    for _ in 0..8 {
        if settings_changed && credentials_changed {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(1), notifications.recv())
            .await
            .unwrap()
            .unwrap()
        {
            HostNotification::SettingsChanged(event)
                if event.namespace == namespace && event.kind == SettingsEventKind::Updated =>
            {
                settings_changed = true;
            }
            HostNotification::CredentialsChanged(event) if event.reference == reference => {
                credentials_changed = true;
            }
            _ => {}
        }
    }
    assert!(settings_changed && credentials_changed);
}

#[tokio::test]
async fn host_approval_registry_tracks_owned_agent_generations() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = runtime.handle();
    let approvals = handle.approval_registry().unwrap();
    let session = SessionId::from("host-approval-lifetime");
    assert!(approvals.lookup(&session).is_none());

    handle.prompt(prompt(session.as_str())).await.unwrap();
    assert!(approvals.lookup(&session).is_some());
    assert!(handle
        .cancel(session.clone(), AgentCancelCause::User)
        .await
        .unwrap());
    assert!(approvals.lookup(&session).is_none());

    handle.prompt(prompt(session.as_str())).await.unwrap();
    assert!(approvals.lookup(&session).is_some());
    runtime.shutdown().await.unwrap();
    assert!(approvals.lookup(&session).is_none());
}

#[tokio::test]
async fn permission_command_switches_durable_sandbox_and_approval_policy() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = runtime.handle();
    let session = SessionId::from("permission-command");
    handle.create_session(session.clone()).await.unwrap();

    let commands = handle.command_list(session.clone()).await.unwrap();
    assert!(commands.iter().any(|command| command.name == "permission"));

    let execution = handle
        .command_execute(session.clone(), "/permission danger-full-access".into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(execution.result).unwrap(),
        json!({"kind": "success", "text": "preset danger-full-access"})
    );
    let events = handle.events(session.clone(), 0).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.event_type.as_str(),
                "command/run"
                    | "permission/preset"
                    | "sandbox/mode"
                    | "approval/policy"
                    | "command/done"
            ))
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        [
            "permission/preset",
            "sandbox/mode",
            "approval/policy",
            "command/run",
            "permission/preset",
            "sandbox/mode",
            "approval/policy",
            "command/done",
        ]
    );
    assert_eq!(
        handle
            .approval_registry()
            .unwrap()
            .lookup(&session)
            .unwrap()
            .policy(),
        ApprovalPolicy::Never
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn permission_setting_is_pinned_only_into_future_sessions() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = runtime.handle();
    let first = SessionId::from("permission-default-first");
    handle.create_session(first.clone()).await.unwrap();

    handle
        .settings()
        .unwrap()
        .update(
            "permission",
            json!({"defaultPreset": "danger-full-access"}),
            None,
        )
        .await
        .unwrap();
    let second = SessionId::from("permission-default-second");
    handle.create_session(second.clone()).await.unwrap();

    let preset = |events: Vec<SessionEvent>| {
        events
            .into_iter()
            .rev()
            .find(|event| event.event_type == "permission/preset")
            .and_then(|event| event.data.get("preset")?.as_str().map(str::to_owned))
            .unwrap()
    };
    assert_eq!(
        preset(handle.events(first, 0).await.unwrap()),
        "workspace-write"
    );
    assert_eq!(
        preset(handle.events(second, 0).await.unwrap()),
        "danger-full-access"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_services_use_default_paths_persist_and_drain_on_shutdown() {
    let root = TempDir::new();
    let namespace = format!("host-{}", Uuid::new_v4().simple());
    let reference =
        CredentialRef::new(format!("TESSIVUM_HOST_{}", Uuid::new_v4().simple())).unwrap();
    let value = "host-credential-value";
    let first = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = first.handle();
    let settings = handle.settings().unwrap();
    let credentials = handle.credentials().unwrap();
    settings
        .register(SettingsRegistration::new(
            namespace.clone(),
            json!({}),
            json!({}),
            json!({}),
        ))
        .await
        .unwrap();
    settings
        .update(&namespace, json!({"saved": true}), None)
        .await
        .unwrap();
    let mut credential_events = credentials.subscribe();
    credentials
        .set(reference.clone(), value.into())
        .await
        .unwrap();
    assert!(!format!("{credentials:?}").contains(value));
    assert!(
        !serde_json::to_string(&credential_events.recv().await.unwrap())
            .unwrap()
            .contains(value)
    );
    let shadow_reference =
        CredentialRef::new(format!("TESSIVUM_SHADOW_{}", Uuid::new_v4().simple())).unwrap();
    let shadow_value = "host-environment-secret";
    std::env::set_var(shadow_reference.as_str(), shadow_value);
    let shadowed_set = credentials
        .set(shadow_reference.clone(), "host-file-secret".into())
        .await;
    let shadowed_unset = credentials.unset(&shadow_reference).await;
    std::env::remove_var(shadow_reference.as_str());
    let shadowed_set = shadowed_set.unwrap_err();
    assert!(matches!(&shadowed_set, CredentialError::Shadowed(_)));
    assert!(
        !shadowed_set.to_string().contains(shadow_value)
            && !shadowed_set.to_string().contains("host-file-secret")
    );
    assert!(matches!(shadowed_unset, Err(CredentialError::Shadowed(_))));
    assert!(root.path().join("data/settings.yaml").is_file());
    assert!(root.path().join("data/credentials.yaml").is_file());

    first.shutdown().await.unwrap();
    assert!(matches!(
        settings
            .update(&namespace, json!({"after": true}), None)
            .await,
        Err(SettingsError::Closed)
    ));
    assert!(matches!(
        credentials.set(reference.clone(), value.into()).await,
        Err(CredentialError::Closed)
    ));

    let second = HostRuntime::boot(config(&root)).await.unwrap();
    let settings = second.handle().settings().unwrap();
    let credentials = second.handle().credentials().unwrap();
    settings
        .register(SettingsRegistration::new(
            namespace.clone(),
            json!({}),
            json!({}),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(
        settings.get(&namespace).unwrap().value,
        json!({"saved": true})
    );
    assert_eq!(
        credentials.resolve(&reference).await.unwrap(),
        Some(value.into())
    );
    credentials.unset(&reference).await.unwrap();
    second.shutdown().await.unwrap();

    let third = HostRuntime::boot(config(&root)).await.unwrap();
    assert_eq!(
        third
            .handle()
            .credentials()
            .unwrap()
            .resolve(&reference)
            .await
            .unwrap(),
        None
    );
    third.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_uses_selected_storage_files_and_rejects_directories() {
    let root = TempDir::new();
    let settings_path = root.path().join("selected/settings.yaml");
    let credentials_path = root.path().join("selected/credentials.yaml");
    let runtime = HostRuntime::boot(
        config(&root)
            .with_settings_path(&settings_path)
            .with_credentials_path(&credentials_path),
    )
    .await
    .unwrap();
    let settings = runtime.handle().settings().unwrap();
    settings
        .register(SettingsRegistration::new(
            "selected",
            json!({}),
            json!({}),
            json!({}),
        ))
        .await
        .unwrap();
    settings
        .update("selected", json!({"on": true}), None)
        .await
        .unwrap();
    runtime
        .handle()
        .credentials()
        .unwrap()
        .set(
            CredentialRef::new("TESSIVUM_SELECTED").unwrap(),
            "value".into(),
        )
        .await
        .unwrap();
    assert!(settings_path.is_file());
    assert!(credentials_path.is_file());
    assert!(!root.path().join("data/settings.yaml").exists());
    assert!(!root.path().join("data/credentials.yaml").exists());
    runtime.shutdown().await.unwrap();

    let rejected = HostRuntime::boot(config(&root).with_settings_path(root.path()))
        .await
        .err()
        .unwrap();
    assert_eq!(rejected.code(), "INVALID_HOST_CONFIG");
}

#[tokio::test]
async fn host_persists_workspace_session_attachment_and_retries_ungrouped_blanks() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let registry = runtime.workspace_registry().unwrap();
    let default_workspace = registry.list().into_iter().next().unwrap().workspace_id;
    let direct = runtime
        .create_session(SessionId::from("workspace-direct"))
        .await
        .unwrap();
    assert_eq!(direct.workspace_id, Some(default_workspace.clone()));

    let other_dir = root.path().join("other-workspace");
    fs::create_dir(&other_dir).unwrap();
    let other_workspace = registry
        .create(&other_dir, None)
        .unwrap()
        .workspace
        .workspace_id;
    let attached = runtime
        .create_session_in(SessionId::from("workspace-other"), other_workspace.clone())
        .await
        .unwrap();
    assert_eq!(attached.workspace_id, Some(other_workspace));
    assert_eq!(
        runtime
            .create_session_in(
                SessionId::from("workspace-other"),
                default_workspace.clone()
            )
            .await
            .unwrap_err()
            .code,
        "SESSION_CONFLICT"
    );

    let registry_path = root.path().join("data/workspaces.json");
    fs::remove_file(&registry_path).unwrap();
    fs::create_dir(&registry_path).unwrap();
    let failed = runtime
        .create_session_in(
            SessionId::from("workspace-retry"),
            default_workspace.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(failed.code, "WORKSPACE_ATTACH_FAILED");
    let ungrouped = runtime
        .list_sessions()
        .await
        .unwrap()
        .into_iter()
        .find(|session| session.session_id == SessionId::from("workspace-retry"))
        .unwrap();
    assert_eq!(ungrouped.workspace_id, None);
    assert_eq!(
        runtime
            .prompt(prompt("workspace-retry"))
            .await
            .unwrap_err()
            .code,
        "SESSION_UNGROUPED"
    );
    fs::remove_dir(&registry_path).unwrap();
    let retried = runtime
        .create_session_in(
            SessionId::from("workspace-retry"),
            default_workspace.clone(),
        )
        .await
        .unwrap();
    assert_eq!(retried.workspace_id, Some(default_workspace.clone()));
    runtime.shutdown().await.unwrap();

    let restarted = HostRuntime::boot(config(&root)).await.unwrap();
    let reopened = restarted.workspace_registry().unwrap();
    assert!(reopened
        .list()
        .into_iter()
        .any(|workspace| workspace.workspace_id == default_workspace));
    assert_eq!(
        restarted
            .list_sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|session| session.session_id == SessionId::from("workspace-retry"))
            .unwrap()
            .workspace_id,
        Some(default_workspace)
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn cold_session_catalog_uses_latest_durable_event_time() {
    let root = TempDir::new();
    let persistence = JsonlSessionPersistence::new(root.path().join("data"));
    let mut header = persisted_header("cold-activity", None);
    header.created_at = 7;
    let mut event = surface_event("user/message", 0, "latest activity");
    event.time = 42;
    persist_session(&persistence, header, [event]).await;

    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let listed = runtime
        .list_sessions()
        .await
        .unwrap()
        .into_iter()
        .find(|session| session.session_id == SessionId::from("cold-activity"))
        .unwrap();
    assert_eq!(listed.created_at, 7);
    assert_eq!(listed.updated_at, 42);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_search_rename_and_fork_use_durable_workspace_visible_history() {
    let root = TempDir::new();
    let persistence = JsonlSessionPersistence::new(root.path().join("data"));
    let workspace_cwd = root
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    for index in (0..21).rev() {
        persist_session(
            &persistence,
            persisted_header(&format!("cap-{index:02}"), Some(workspace_cwd.clone())),
            [surface_event("user/message", 0, "cap-query")],
        )
        .await;
    }
    persist_session(
        &persistence,
        persisted_header("0-hidden", None),
        [surface_event("user/message", 0, "cap-query")],
    )
    .await;
    let unicode_text = format!("{} unicode-query {}", "😀".repeat(121), "界".repeat(121));
    persist_session(
        &persistence,
        persisted_header("unicode", Some(workspace_cwd.clone())),
        [surface_event("user/message", 0, &unicode_text)],
    )
    .await;
    persist_session(
        &persistence,
        persisted_header("semantic-user", Some(workspace_cwd.clone())),
        [surface_event("user/message", 0, "semantic-user-hit")],
    )
    .await;
    persist_session(
        &persistence,
        persisted_header("semantic-assistant", Some(workspace_cwd.clone())),
        [surface_event(
            "assistant/message",
            0,
            "semantic-assistant-hit",
        )],
    )
    .await;
    let steering = SessionId::from("semantic-steering");
    persist_session(
        &persistence,
        persisted_header(steering.as_str(), Some(workspace_cwd.clone())),
        [SessionEvent {
            event_type: "agent/inbox/enqueued".into(),
            seq: 0,
            time: 0,
            data: json!({
                "target": "steer",
                "message": {
                    "id": "message-0",
                    "role": "user",
                    "content": [{"type": "text", "text": "semantic-steering-hit"}],
                    "source": {"kind": "user"},
                },
            }),
            ignorable: Some(true),
            source_event_seqs: None,
            surface_op: None,
        }],
    )
    .await;
    persist_session(
        &persistence,
        persisted_header("semantic-tool", Some(workspace_cwd.clone())),
        [SessionEvent {
            event_type: "tool/result".into(),
            seq: 0,
            time: 0,
            data: json!({"message": {
                "id": "tool-message",
                "role": "user",
                "content": [{"type": "text", "text": "semantic-tool-hit"}],
                "source": {"kind": "tool", "callId": "tool-call"},
            }}),
            ignorable: None,
            source_event_seqs: None,
            surface_op: Some(SurfaceOp::Append),
        }],
    )
    .await;
    let source = SessionId::from("fork-source");
    let mut fork_assistant = surface_event("assistant/message", 2, "fork assistant marker");
    fork_assistant.data["turn"] = json!(0);
    fork_assistant.data["step"] = json!(0);
    persist_session(
        &persistence,
        persisted_header(source.as_str(), Some(workspace_cwd.clone())),
        [
            SessionEvent {
                event_type: "turn/start".into(),
                seq: 0,
                time: 0,
                data: json!({"turn": 0}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            surface_event("user/message", 1, "fork completed marker"),
            fork_assistant,
            SessionEvent {
                event_type: "turn/end".into(),
                seq: 3,
                time: 3,
                data: json!({"turn": 0, "reason": {"kind": "stop"}}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            SessionEvent {
                event_type: "turn/start".into(),
                seq: 4,
                time: 4,
                data: json!({"turn": 1}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            surface_event("user/message", 5, "fork open anchor marker"),
        ],
    )
    .await;
    let latest = SessionId::from("fork-latest");
    persist_session(
        &persistence,
        persisted_header(latest.as_str(), Some(workspace_cwd)),
        [
            SessionEvent {
                event_type: "turn/start".into(),
                seq: 0,
                time: 0,
                data: json!({"turn": 0}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            surface_event("user/message", 1, "first complete turn"),
            SessionEvent {
                event_type: "turn/end".into(),
                seq: 2,
                time: 2,
                data: json!({"turn": 0, "reason": {"kind": "stop"}}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            SessionEvent {
                event_type: "turn/start".into(),
                seq: 3,
                time: 3,
                data: json!({"turn": 1}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            surface_event("user/message", 4, "latest complete turn"),
            SessionEvent {
                event_type: "turn/end".into(),
                seq: 5,
                time: 5,
                data: json!({"turn": 1, "reason": {"kind": "stop"}}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
        ],
    )
    .await;

    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = runtime.handle();
    assert!(runtime
        .workspace_registry()
        .unwrap()
        .workspace_for_session("cap-00")
        .is_some());
    assert!(runtime
        .workspace_registry()
        .unwrap()
        .workspace_for_session("0-hidden")
        .is_none());

    let capped = handle.search_sessions("cap-query".into()).await.unwrap();
    assert_eq!(capped.items.len(), 20);
    assert_eq!(
        capped
            .items
            .iter()
            .map(|hit| hit.session_id.as_str().to_owned())
            .collect::<Vec<_>>(),
        (0..20)
            .map(|index| format!("cap-{index:02}"))
            .collect::<Vec<_>>(),
        "equal semantic hits retain the stable session-id order"
    );
    assert!(capped.has_more, "the twenty-first visible hit is lookahead");

    let unicode = handle
        .search_sessions("unicode-query".into())
        .await
        .unwrap();
    assert_eq!(unicode.items.len(), 1);
    assert_eq!(unicode.items[0].session_id.as_str(), "unicode");
    assert_eq!(unicode.items[0].snippet.chars().count(), 240);
    assert!(unicode.items[0].snippet.contains("unicode-query"));

    for (query, session) in [
        ("semantic-user-hit", "semantic-user"),
        ("semantic-assistant-hit", "semantic-assistant"),
    ] {
        let found = handle.search_sessions(query.into()).await.unwrap();
        assert_eq!(
            found
                .items
                .iter()
                .map(|hit| hit.session_id.as_str())
                .collect::<Vec<_>>(),
            vec![session]
        );
    }
    assert!(
        handle
            .search_sessions("semantic-steering-hit".into())
            .await
            .unwrap()
            .items
            .is_empty(),
        "unclaimed inbox entries are not model-visible session messages"
    );
    assert!(
        handle
            .search_sessions("semantic-tool-hit".into())
            .await
            .unwrap()
            .items
            .is_empty(),
        "tool results are not browser session.search message hits"
    );

    let renamed = handle
        .rename_session(source.clone(), "  Durable\n title  ".into())
        .await
        .unwrap();
    assert_eq!(renamed.title, "Durable title");
    assert_eq!(renamed.seq, 6);
    assert_eq!(
        handle
            .fork_session(source.clone(), Some(4))
            .await
            .unwrap_err()
            .code,
        "FORK_UNAVAILABLE",
        "an open turn is never a fork anchor"
    );
    let child = handle.fork_session(source.clone(), Some(0)).await.unwrap();
    assert_eq!(
        runtime
            .workspace_registry()
            .unwrap()
            .workspace_for_session(&child)
            .unwrap()
            .workspace_id,
        runtime
            .workspace_registry()
            .unwrap()
            .workspace_for_session(&source)
            .unwrap()
            .workspace_id
    );
    let child_events = handle.events(child.clone(), 0).await.unwrap();
    assert_eq!(
        child_events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "turn/start",
            "user/message",
            "assistant/message",
            "turn/end",
            "permission/preset",
            "sandbox/mode",
            "approval/policy",
        ],
        "forks retain the completed turn prefix and pin child permissions"
    );
    let latest_child = handle.fork_session(latest.clone(), None).await.unwrap();
    let past_end_child = handle.fork_session(latest, Some(999)).await.unwrap();
    let latest_events = handle.events(latest_child, 0).await.unwrap();
    let past_end_events = handle.events(past_end_child, 0).await.unwrap();
    assert_eq!(latest_events.len(), 9);
    assert_eq!(
        past_end_events
            .iter()
            .map(|event| (&event.event_type, &event.data))
            .collect::<Vec<_>>(),
        latest_events
            .iter()
            .map(|event| (&event.event_type, &event.data))
            .collect::<Vec<_>>()
    );
    runtime.shutdown().await.unwrap();

    let restarted = HostRuntime::boot(config(&root)).await.unwrap();
    assert!(restarted
        .handle()
        .events(source, 0)
        .await
        .unwrap()
        .iter()
        .any(|event| event.event_type == "session/title"
            && event.data["title"] == "Durable title"
            && event.data["messageSeqs"] == json!([])
            && event.data["source"] == json!({"kind": "user"})));
    assert_eq!(
        restarted.handle().events(child, 0).await.unwrap(),
        child_events
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_reads_only_durable_session_attachments_without_resuming() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let session = SessionId::from("attachment-read");
    let reference = runtime
        .upload_attachment(png(1, 1), Some("nested.png".into()))
        .await
        .unwrap();
    runtime
        .prompt(SessionPromptParams {
            session_id: session.clone(),
            content_blocks: vec![ContentBlock::ToolResult {
                tool_call_id: ToolCallId::from("attachment-tool"),
                content: vec![ContentBlock::Image {
                    attachment: serde_json::to_value(&reference).unwrap(),
                }],
                is_error: None,
            }],
            client_time_zone: None,
        })
        .await
        .unwrap();

    let live = runtime
        .read_attachment(session.clone(), reference.attachment_id.clone())
        .await
        .unwrap();
    assert_eq!(live.reference, reference);
    assert_eq!(live.data, png(1, 1));

    let unreferenced = runtime.upload_attachment(png(2, 2), None).await.unwrap();
    let unreferenced_path = root.path().join("data/attachments/v1").join(
        unreferenced
            .attachment_id
            .as_str()
            .strip_prefix("sha256:")
            .unwrap(),
    );
    assert_eq!(
        runtime
            .read_attachment(session.clone(), unreferenced.attachment_id.clone())
            .await
            .unwrap_err()
            .code,
        "ATTACHMENT_NOT_REFERENCED"
    );
    assert_eq!(
        runtime
            .read_attachment(SessionId::from(""), reference.attachment_id.clone())
            .await
            .unwrap_err()
            .code,
        "INVALID_SESSION_ID"
    );

    runtime.shutdown().await.unwrap();
    assert!(
        !unreferenced_path.exists(),
        "shutdown removes unattached blobs"
    );
    drop(runtime);

    let restarted = HostRuntime::boot(config(&root)).await.unwrap();
    let before = restarted.events(session.clone(), 0).await.unwrap();
    let persisted = restarted
        .read_attachment(session.clone(), reference.attachment_id)
        .await
        .unwrap();
    assert_eq!(persisted.reference, live.reference);
    assert_eq!(persisted.data, live.data);
    assert_eq!(restarted.events(session, 0).await.unwrap(), before);
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_boot_migrates_durable_session_cwds_once() {
    let root = TempDir::new();
    let legacy_workspace = root.path().join("legacy-workspace");
    fs::create_dir(&legacy_workspace).unwrap();
    let persistence = JsonlSessionPersistence::new(root.path().join("data"));
    let context = ContextHandle::root();
    persistence
        .create(
            &SessionHeader {
                version: SESSION_FORMAT_VERSION,
                id: SessionId::from("legacy-workspace-session"),
                created_at: 1,
                cwd: Some(
                    legacy_workspace
                        .canonicalize()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                ),
                parent_session: None,
                seed_length: None,
                origin: None,
                delegation_depth: Some(0),
                agent_preset: None,
            },
            context.scope().cancellation(),
        )
        .await
        .unwrap();

    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let first = runtime
        .workspace_registry()
        .unwrap()
        .workspace_for_session("legacy-workspace-session")
        .unwrap();
    assert_eq!(
        first.path,
        legacy_workspace.canonicalize().unwrap().to_string_lossy()
    );
    let workspace_id = first.workspace_id;
    runtime.shutdown().await.unwrap();

    let restarted = HostRuntime::boot(config(&root)).await.unwrap();
    assert_eq!(
        restarted
            .workspace_registry()
            .unwrap()
            .workspace_for_session("legacy-workspace-session")
            .unwrap()
            .workspace_id,
        workspace_id
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleting_workspace_disposes_agents_and_denies_default_sessions() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let registry = runtime.workspace_registry().unwrap();
    let default_workspace = registry.list().into_iter().next().unwrap().workspace_id;
    let other_dir = root.path().join("other-workspace");
    fs::create_dir(&other_dir).unwrap();
    let other_workspace = registry
        .create(&other_dir, None)
        .unwrap()
        .workspace
        .workspace_id;
    let deleted_session = SessionId::from("deleted-live");

    runtime
        .create_session_in(deleted_session.clone(), default_workspace.clone())
        .await
        .unwrap();
    runtime.prompt(prompt("deleted-live")).await.unwrap();
    let approvals = runtime.approval_registry().unwrap();
    assert!(approvals.lookup(&deleted_session).is_some());

    assert!(runtime
        .delete_workspace(default_workspace.clone())
        .await
        .unwrap());
    assert!(registry.workspace_for_session(&deleted_session).is_none());
    assert!(approvals.lookup(&deleted_session).is_none());
    let after_delete = runtime.events(deleted_session.clone(), 0).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        runtime.events(deleted_session.clone(), 0).await.unwrap(),
        after_delete
    );
    assert_eq!(
        runtime
            .prompt(prompt("deleted-live"))
            .await
            .unwrap_err()
            .code,
        "SESSION_UNGROUPED"
    );
    assert_eq!(
        runtime
            .create_session(SessionId::from("default-denied"))
            .await
            .unwrap_err()
            .code,
        "WORKSPACE_NOT_FOUND"
    );
    assert_eq!(
        runtime
            .create_session_in(SessionId::from("explicit-other"), other_workspace.clone())
            .await
            .unwrap()
            .workspace_id,
        Some(other_workspace)
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_creation_is_serial_idempotent_and_conflict_safe() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let registry = runtime.workspace_registry().unwrap();
    let first_dir = root.path().join("first-workspace");
    let second_dir = root.path().join("second-workspace");
    fs::create_dir(&first_dir).unwrap();
    fs::create_dir(&second_dir).unwrap();
    let first_workspace = registry
        .create(&first_dir, None)
        .unwrap()
        .workspace
        .workspace_id;
    let second_workspace = registry
        .create(&second_dir, None)
        .unwrap()
        .workspace
        .workspace_id;

    let automatic = runtime.handle();
    tokio::time::timeout(
        Duration::from_secs(1),
        automatic.prompt(prompt("automatic-session")),
    )
    .await
    .expect("automatic session creation must not deadlock")
    .unwrap();

    let left_handle = runtime.handle();
    let right_handle = runtime.handle();
    let same_session = SessionId::from("concurrent-same");
    let left_session = same_session.clone();
    let right_session = same_session.clone();
    let left_workspace = first_workspace.clone();
    let right_workspace = first_workspace.clone();
    let (left, right) = tokio::time::timeout(Duration::from_secs(1), async move {
        tokio::join!(
            left_handle.create_session_in(left_session, left_workspace),
            right_handle.create_session_in(right_session, right_workspace),
        )
    })
    .await
    .expect("same-workspace creation must not deadlock");
    assert_eq!(left.unwrap().workspace_id, Some(first_workspace.clone()));
    assert_eq!(right.unwrap().workspace_id, Some(first_workspace.clone()));

    let first_handle = runtime.handle();
    let second_handle = runtime.handle();
    let conflict_session = SessionId::from("concurrent-conflict");
    let first_session = conflict_session.clone();
    let second_session = conflict_session;
    let first_workspace_id = first_workspace;
    let second_workspace_id = second_workspace;
    let (first, second) = tokio::time::timeout(Duration::from_secs(1), async move {
        tokio::join!(
            first_handle.create_session_in(first_session, first_workspace_id),
            second_handle.create_session_in(second_session, second_workspace_id),
        )
    })
    .await
    .expect("conflicting creation must not deadlock");
    match (first, second) {
        (Ok(_), Err(error)) | (Err(error), Ok(_)) => assert_eq!(error.code, "SESSION_CONFLICT"),
        result => panic!("expected one successful create and one conflict, got {result:?}"),
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn dynamic_models_report_defaults_without_eager_legacy_migration() {
    let root = TempDir::new();
    let persistence = JsonlSessionPersistence::new(root.path().join("data"));
    let context = ContextHandle::root();
    persistence
        .create(
            &SessionHeader {
                version: SESSION_FORMAT_VERSION,
                id: SessionId::from("legacy-model"),
                created_at: 1,
                cwd: Some(
                    root.path()
                        .canonicalize()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                ),
                parent_session: None,
                seed_length: None,
                origin: None,
                delegation_depth: Some(0),
                agent_preset: None,
            },
            context.scope().cancellation(),
        )
        .await
        .unwrap();

    let runtime = HostRuntime::boot(dynamic_config(&root)).await.unwrap();
    let handle = runtime.handle();
    assert_eq!(
        handle
            .session_models(SessionId::from("legacy-model"))
            .await
            .unwrap()
            .current,
        Some(SessionModelSelection {
            provider: "openai-responses".into(),
            model: "alpha".into(),
            reasoning_effort: None,
        })
    );
    assert!(handle
        .events(SessionId::from("legacy-model"), 0)
        .await
        .unwrap()
        .is_empty());

    let mut notifications = handle.subscribe();
    handle
        .mutate_settings(
            AGENT_DEFAULT_MODEL_NAMESPACE.into(),
            HostSettingsMutation::Update {
                patch: json!({"provider": "openai-responses", "model": "beta"}),
                expected_revision: None,
            },
        )
        .await
        .unwrap();
    wait_for_models_changed(&mut notifications).await;
    assert_eq!(
        handle
            .session_models(SessionId::from("legacy-model"))
            .await
            .unwrap()
            .current
            .unwrap()
            .model,
        "beta"
    );
    assert!(
        handle
            .events(SessionId::from("legacy-model"), 0)
            .await
            .unwrap()
            .is_empty(),
        "catalog reads do not migrate legacy sessions"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn providerless_web_uses_first_configured_route_for_new_sessions() {
    let root = TempDir::new();
    let mut config = dynamic_config(&root);
    config.provider = "openai-responses".into();
    config.model = "unconfigured".into();
    let runtime = HostRuntime::boot(config).await.unwrap();
    runtime
        .handle()
        .credentials()
        .unwrap()
        .set(
            CredentialRef::new("TESSIVUM_DYNAMIC_TEST_KEY").unwrap(),
            "test-key".into(),
        )
        .await
        .unwrap();
    let session_id = SessionId::from("providerless-route");

    runtime.create_session(session_id.clone()).await.unwrap();
    assert_eq!(
        runtime
            .handle()
            .session_models(session_id)
            .await
            .unwrap()
            .current,
        Some(SessionModelSelection {
            provider: "openai-responses".into(),
            model: "alpha".into(),
            reasoning_effort: None,
        })
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn dynamic_route_notification_follows_committed_registration() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(dynamic_config(&root)).await.unwrap();
    let handle = runtime.handle();
    let mut notifications = handle.subscribe();
    handle
        .mutate_settings(
            LLM_PI_AI_NAMESPACE.into(),
            HostSettingsMutation::Update {
                patch: json!({
                    "providers": {
                        "openai-responses": {
                            "displayName": "Updated relay",
                            "baseURL": "http://127.0.0.1:2/v1",
                            "apiKeyEnv": "TESSIVUM_UPDATED_TEST_KEY",
                            "models": [{"id": "beta", "input": ["text", "image"]}]
                        }
                    }
                }),
                expected_revision: None,
            },
        )
        .await
        .unwrap();
    wait_for_models_changed(&mut notifications).await;
    assert!(handle.provider_directory()[0].active);
    assert_eq!(
        handle.model_groups("openai-responses")[0].models[0].id,
        "beta"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn dynamic_route_rejects_shared_credentials_before_publication() {
    let root = TempDir::new();
    let mut config = HostConfig::new(root.path(), root.path().join("data"));
    config.provider = "first".into();
    config.model = "alpha".into();
    config.profile_patch = json!({
        "llm-pi-ai": {
            "providers": {
                "first": {
                    "displayName": "First",
                    "baseURL": "http://127.0.0.1:1/v1",
                    "apiKeyEnv": "SHARED_SECRET_ENV",
                    "models": [{"id": "alpha", "input": ["text"]}]
                },
                "second": {
                    "displayName": "Second",
                    "baseURL": "http://127.0.0.1:2/v1",
                    "apiKeyEnv": "SHARED_SECRET_ENV",
                    "models": [{"id": "beta", "input": ["text"]}]
                }
            }
        }
    });
    let error = match HostRuntime::boot(config).await {
        Ok(_) => panic!("shared credential routes must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "INVALID_HOST_CONFIG");
    assert!(!error.to_string().contains("SHARED_SECRET_ENV"));
}

#[tokio::test]
async fn dynamic_route_rejects_invalid_provider_ids() {
    let root = TempDir::new();
    let mut config = HostConfig::new(root.path(), root.path().join("data"));
    config.provider = "Uppercase".into();
    config.model = "alpha".into();
    config.profile_patch = json!({
        "llm-pi-ai": {
            "providers": {
                "Uppercase": {
                    "displayName": "Invalid",
                    "baseURL": "http://127.0.0.1:1/v1",
                    "apiKeyEnv": "VALID_SECRET_ENV",
                    "models": [{"id": "alpha", "input": ["text"]}]
                }
            }
        }
    });
    let error = match HostRuntime::boot(config).await {
        Ok(_) => panic!("invalid provider ids must be rejected"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "INVALID_HOST_CONFIG");
}

#[tokio::test]
async fn provider_enablement_is_durable_noop_aware_and_rejects_unknown_routes() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(dynamic_config(&root)).await.unwrap();
    let handle = runtime.handle();
    let settings = handle.settings().unwrap();
    let mut notifications = handle.subscribe();

    let unknown = HostApi::set_provider_enabled(&runtime, "unknown".into(), true)
        .await
        .unwrap_err();
    assert_eq!(unknown.code, "LLM_PROVIDER_NOT_FOUND");
    assert_eq!(settings.get(LLM_PI_AI_NAMESPACE).unwrap().revision, 0);
    assert_eq!(settings.user(LLM_PI_AI_NAMESPACE).unwrap(), json!({}));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), notifications.recv())
            .await
            .is_err()
    );

    assert!(
        HostApi::set_provider_enabled(&runtime, "openai-responses".into(), true)
            .await
            .unwrap()
            .enabled
    );
    wait_for_adapters_updated(&mut notifications).await;
    assert_eq!(
        settings.user(LLM_PI_AI_NAMESPACE).unwrap(),
        json!({"providers": {"openai-responses": {"enabled": true}}})
    );
    let revision = settings.get(LLM_PI_AI_NAMESPACE).unwrap().revision;
    assert!(tokio::time::timeout(Duration::from_millis(25), async {
        loop {
            if matches!(
                notifications.recv().await.unwrap(),
                HostNotification::AdaptersUpdated
            ) {
                return;
            }
        }
    })
    .await
    .is_err());

    let mut noop_notifications = handle.subscribe();
    assert!(
        handle
            .set_provider_enabled("openai-responses".into(), true)
            .await
            .unwrap()
            .enabled
    );
    assert_eq!(
        settings.get(LLM_PI_AI_NAMESPACE).unwrap().revision,
        revision
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(25), noop_notifications.recv())
            .await
            .is_err()
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn provider_enablement_persists_false_when_user_config_is_absent() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(dynamic_config(&root)).await.unwrap();
    let handle = runtime.handle();
    let settings = handle.settings().unwrap();
    let mut notifications = handle.subscribe();

    assert_eq!(
        handle
            .set_provider_enabled("openai-responses".into(), false)
            .await
            .unwrap(),
        tessivum::host::HostProviderEnabled {
            provider: "openai-responses".into(),
            enabled: false,
        }
    );
    wait_for_adapters_updated(&mut notifications).await;
    assert_eq!(
        settings.user(LLM_PI_AI_NAMESPACE).unwrap(),
        json!({"providers": {"openai-responses": {"enabled": false}}})
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn provider_enablement_preserves_existing_provider_fields_and_siblings() {
    let root = TempDir::new();
    let mut config = dynamic_config(&root);
    config.profile_patch[LLM_PI_AI_NAMESPACE]["providers"]["secondary"] = json!({
        "displayName": "Secondary relay",
        "baseURL": "http://127.0.0.1:2/v1",
        "apiKeyEnv": "TESSIVUM_SECONDARY_TEST_KEY",
        "models": [{"id": "secondary", "input": ["text"]}]
    });
    let runtime = HostRuntime::boot(config).await.unwrap();
    let handle = runtime.handle();
    let settings = handle.settings().unwrap();
    let user = json!({
        "providers": {
            "openai-responses": {
                "displayName": "User relay",
                "models": [{"id": "user-alpha", "input": ["text"]}]
            },
            "secondary": {
                "displayName": "User secondary",
                "models": [{"id": "user-secondary", "input": ["text"]}]
            }
        }
    });
    settings
        .replace(LLM_PI_AI_NAMESPACE, user.clone(), None)
        .await
        .unwrap();
    let mut notifications = handle.subscribe();

    handle
        .set_provider_enabled("openai-responses".into(), false)
        .await
        .unwrap();
    wait_for_adapters_updated(&mut notifications).await;
    let mut expected = user;
    expected["providers"]["openai-responses"]["enabled"] = json!(false);
    assert_eq!(settings.user(LLM_PI_AI_NAMESPACE).unwrap(), expected);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_settings_write_has_no_revision_or_notification() {
    let settings = Settings::new(Arc::new(FailingSettingsProvider));
    settings
        .register(SettingsRegistration::new(
            LLM_PI_AI_NAMESPACE,
            json!({"type": "object"}),
            json!({}),
            json!({}),
        ))
        .await
        .unwrap();
    let mut notifications = settings.subscribe();

    assert_eq!(
        settings
            .update(LLM_PI_AI_NAMESPACE, json!({"providers": {}}), None)
            .await,
        Err(SettingsError::Persistence("write failed".into()))
    );
    assert_eq!(settings.get(LLM_PI_AI_NAMESPACE).unwrap().revision, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), notifications.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn provider_models_uses_active_route_normalizes_exact_wire_and_preserves_host_errors() {
    let root = TempDir::new();
    let valid = json!({
        "id": "model-a",
        "name": "Model A",
        "contextWindow": 8192,
        "maxOutput": 2048,
        "reasoning": true,
        "input": ["text", "image"]
    });
    let adapter = Arc::new(ProviderModelsAdapter::new(vec![
        Ok(json!([valid.clone()])),
        Ok(json!([valid.clone(), valid.clone()])),
        Ok(json!([{
            "id": "legacy",
            "name": "Legacy",
            "contextWindow": 4096,
            "maxTokens": 1024,
            "reasoning": false,
            "inputModalities": ["text"]
        }])),
    ]));
    let mut config = HostConfig::new(root.path(), root.path().join("data"))
        .with_adapter_factory(Arc::new(ProviderModelsFactory(Arc::clone(&adapter))));
    config.provider = "active".into();
    config.model = "model-a".into();
    let runtime = HostRuntime::boot(config).await.unwrap();
    let handle = runtime.handle();
    let custom = json!({"nested": {"credential": "draft"}});
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let models = handle
        .provider_models("active".into(), custom.clone())
        .await
        .unwrap();
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    assert!((before..=after).contains(&models.updated_at));
    let updated_at = models.updated_at;
    assert_eq!(
        serde_json::to_value(models).unwrap(),
        json!({
            "provider": "active",
            "models": [valid],
            "updatedAt": updated_at,
        })
    );

    let duplicate = HostApi::provider_models(&runtime, "active".into(), json!({}))
        .await
        .unwrap_err();
    assert_eq!(duplicate.code, "INVALID_MODEL_DISCOVERY_RESULT");
    assert_eq!(duplicate.details, json!({"provider": "active", "index": 1}));

    let malformed = HostApi::provider_models(&handle, "active".into(), json!({}))
        .await
        .unwrap_err();
    assert_eq!(malformed.code, "INVALID_MODEL_DISCOVERY_RESULT");
    assert_eq!(malformed.details["provider"], "active");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 3);
    assert_eq!(*adapter.configs.lock(), vec![custom, json!({}), json!({})]);

    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn preset_opening_is_host_injected_and_rejected_selects_do_not_commit() {
    let root = TempDir::new();
    let system_root = root.path().join("system-presets");
    let base = system_root.join("base");
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("agent.cordis.yml"), "[]\n").unwrap();
    let opened = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&opened);
    let runtime = HostRuntime::boot(
        HostConfig::new(root.path(), root.path().join("data"))
            .with_agent_preset_root(&system_root, AgentPresetTrust::System)
            .with_path_opener(Arc::new(move |path: &Path| {
                seen.lock().push(path.to_path_buf());
                Ok(())
            })),
    )
    .await
    .unwrap();
    let handle = runtime.handle();

    handle
        .agent_preset_copy("base".into(), "working".into(), None)
        .await
        .unwrap();
    assert_eq!(
        HostApi::agent_preset_open_document(&handle, "working".into())
            .await
            .unwrap(),
        tessivum::host::HostAgentPresetDocument {
            opened: true,
            path: None,
        }
    );
    assert_eq!(
        *opened.lock(),
        vec![root
            .path()
            .join("data/.agent-presets/working")
            .canonicalize()
            .unwrap()]
    );
    assert_eq!(
        HostApi::agent_preset_open_document(&handle, "base".into())
            .await
            .unwrap_err()
            .code,
        "agent-preset-read-only"
    );
    assert_eq!(opened.lock().len(), 1);

    let session = SessionId::from("preset-rejected");
    handle.create_session(session.clone()).await.unwrap();
    assert_eq!(
        HostApi::agent_preset_select(&handle, session.clone(), "missing".into())
            .await
            .unwrap_err()
            .code,
        "agent-preset-not-found"
    );
    assert!(!handle
        .events(session, 0)
        .await
        .unwrap()
        .iter()
        .any(|event| event.event_type == "agent-preset/selected"));

    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_goal_services_are_session_owned_and_relay_committed_changes() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = runtime.handle();
    let first_session = SessionId::from("host-goal-first");
    let second_session = SessionId::from("host-goal-second");
    handle.create_session(first_session.clone()).await.unwrap();
    handle.create_session(second_session.clone()).await.unwrap();
    let mut notifications = handle.subscribe();

    let first = handle.goal_service(first_session.clone()).await.unwrap();
    let created = first
        .create("first".into(), Some(2), first.cancellation())
        .await
        .unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match notifications.recv().await.unwrap() {
                HostNotification::SessionEvent(event)
                    if event.session_id == first_session
                        && event.event.event_type == "goal/change" =>
                {
                    return event.event;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("committed goal change is relayed");
    assert_eq!(observed.data["operation"], "create");

    let second = handle.goal_service(second_session).await.unwrap();
    assert!(second.snapshot(&created.reference.id).await.is_none());
    assert!(matches!(
        second
            .pause(created.reference.clone(), second.cancellation())
            .await,
        Err(GoalError::NotFound(_))
    ));

    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_desktop_operations_canonicalize_confine_and_prepare_the_settings_document() {
    let root = TempDir::new();
    let selected = root.path().join("picked");
    let target = root.path().join("target.txt");
    fs::create_dir_all(&selected).unwrap();
    fs::write(&target, "target").unwrap();
    let outside = std::env::temp_dir().join(format!("tessivum-outside-{}", Uuid::new_v4()));
    fs::write(&outside, "outside").unwrap();

    let opener = Arc::new(RecordingPathOpener::new());
    let picker = Arc::new(TestDirectoryPicker(Mutex::new(Some(selected.clone()))));
    let runtime = HostRuntime::boot(
        config(&root)
            .with_path_opener(opener.clone())
            .with_directory_picker(picker.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();

    assert_eq!(
        HostApi::pick_directory(&handle).await.unwrap(),
        Some(
            fs::canonicalize(&selected)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        )
    );
    *picker.0.lock() = None;
    assert_eq!(HostApi::pick_directory(&handle).await.unwrap(), None);

    HostApi::open_path(&handle, "target.txt".into())
        .await
        .unwrap();
    assert_eq!(
        opener.paths.lock().as_slice(),
        [fs::canonicalize(&target).unwrap()]
    );
    let unsafe_error = HostApi::open_path(&handle, outside.to_string_lossy().into_owned())
        .await
        .unwrap_err();
    assert_eq!(unsafe_error.code, "PATH_UNSAFE");
    let missing_error = HostApi::open_path(&handle, "missing.txt".into())
        .await
        .unwrap_err();
    assert_eq!(missing_error.code, "PATH_INVALID");
    assert_eq!(
        opener.paths.lock().len(),
        1,
        "rejected paths do not execute"
    );

    HostApi::open_settings_document(&handle).await.unwrap();
    let settings_path = root.path().join("data/settings.yaml");
    assert!(settings_path.is_file());
    assert_eq!(
        opener.text_paths.lock().as_slice(),
        [fs::canonicalize(&settings_path).unwrap()]
    );
    opener.available.store(false, Ordering::SeqCst);
    let path_unavailable = HostApi::open_path(&handle, "target.txt".into())
        .await
        .unwrap_err();
    assert_eq!(path_unavailable.code, "PATH_OPENER_UNAVAILABLE");
    let unavailable = HostApi::open_settings_document(&handle).await.unwrap_err();
    assert_eq!(unavailable.code, "SETTINGS_DOCUMENT_UNAVAILABLE");

    runtime.shutdown().await.unwrap();
    fs::remove_file(outside).unwrap();
}

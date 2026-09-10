use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, LazyLock, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::stream;
use serde_json::{json, Value};
use tessivum::{
    agent::{AgentCancelCause, AgentError, AgentOptions, AgentRegistry},
    agent_loop::AgentLoopFactory,
    agent_mode::{AgentModeId, AgentModeRegistry, AgentModeRoot, AgentModeTrust},
    builtin_tools::PersistentShellSessions,
    code_runtime::{ProcessCodeRuntime, ProcessCodeRuntimeConfig},
    composition::CompositionRegistry,
    legacy::ProductPackageResolver,
    llm::{LlmAdapter, LlmRetryPolicy, LlmRuntime, LlmStream, RecordedLlmAdapter},
    session::{
        MemorySessionPersistence, SessionError, SessionInspection, SessionPersistence, SessionStore,
    },
    system_prompt::{PromptRegistration, PromptSection, SystemPrompt},
    tools::{
        ToolApproval, ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRestrictions,
        ToolRunContext, ToolRuntime,
    },
    ContentBlock, FinishReason, GenerateRequest, LlmFailure, Message, MessageRole, MessageSource,
    SessionEvent, SessionHeader, SessionId, SessionOrigin, StreamChunk, SurfaceOp, ToolCallId,
    ToolSchema,
};
use tessivum_core::{
    ActivationState, CancellationToken, ContextHandle, Entry, LoaderError, LoaderFuture,
    LoaderRuntime, NativeConfigSchema, NativePlugin, NativePluginDescriptor, NativePluginError,
    NativePluginFuture, NativePluginPhase, NativePluginRuntime, PackageResolver, ResolvedPackage,
    RuntimeHandle, RuntimeKind,
};

fn cancellation() -> CancellationToken {
    ContextHandle::root().scope().cancellation()
}

fn header(id: &str) -> SessionHeader {
    SessionHeader {
        version: 0,
        id: SessionId::from(id),
        created_at: 0,
        cwd: None,
        parent_session: None,
        seed_length: None,
        origin: None,
        delegation_depth: None,
        agent_mode: None,
    }
}
fn header_with_mode(id: &str, mode: &str) -> SessionHeader {
    let mut header = header(id);
    header.agent_mode = Some(AgentModeId::new(mode).unwrap());
    header
}

static TEST_MODES_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let root = std::env::temp_dir().join(format!("tessivum-agent-loop-{}", std::process::id()));
    for (id, presentation, enabled, bun) in [
        ("test-empty", "direct", "[]", false),
        ("test-read", "direct", "[\"fs.read\"]", false),
        ("test-ptc", "programmatic", "[\"fs.read\"]", true),
        ("test-ptc-bash", "programmatic", "[\"shell.bash\"]", true),
    ] {
        let directory = root.join(id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("mode.toml"),
            format!(
                "schema = 1\nid = \"{id}\"\nname = \"{id}\"\ndescription = \"agent-loop test mode\"\n\n[prompt]\ncomplete = false\ntext = \"Use the additive Tessivum persona, workspace instructions, and runtime context.\"\n\n[tools]\npresentation = \"{presentation}\"\nenabled = {enabled}\n\n[capabilities]\nskills = false\nplanning = false\ncompaction = false\n{}",
                if bun { "bun = true\n" } else { "" },
            ),
        )
        .unwrap();
    }
    let directory = root.join("test-native-plugin");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("mode.toml"),
        "schema = 1\nid = \"test-native-plugin\"\nname = \"test-native-plugin\"\ndescription = \"declarative native plugin test\"\n\n[prompt]\ncomplete = false\ntext = \"Native plugin mode.\"\n\n[tools]\npresentation = \"direct\"\nenabled = []\n\n[capabilities]\nskills = false\nplanning = false\ncompaction = false\n\n[[plugins]]\nid = \"fixture\"\nruntime = \"native\"\nsource = \"fixture-native\"\nconfig = { value = 7 }\n",
    )
    .unwrap();
    let directory = root.join("test-missing-plugin");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("mode.toml"),
        "schema = 1\nid = \"test-missing-plugin\"\nname = \"test-missing-plugin\"\ndescription = \"missing native plugin test\"\n\n[prompt]\ncomplete = false\ntext = \"Missing plugin mode.\"\n\n[tools]\npresentation = \"direct\"\nenabled = []\n\n[capabilities]\nskills = false\nplanning = false\ncompaction = false\n\n[[plugins]]\nid = \"missing\"\nruntime = \"native\"\nsource = \"missing-native\"\n",
    )
    .unwrap();
    let directory = root.join("test-plugin-rollback");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("mode.toml"),
        "schema = 1\nid = \"test-plugin-rollback\"\nname = \"test-plugin-rollback\"\ndescription = \"plugin rollback test\"\n\n[prompt]\ncomplete = false\ntext = \"Plugin rollback mode.\"\n\n[tools]\npresentation = \"direct\"\nenabled = []\n\n[capabilities]\nskills = false\nplanning = false\ncompaction = false\n\n[[plugins]]\nid = \"active\"\nruntime = \"native\"\nsource = \"rollback-active-native\"\nconfig = { value = 7 }\n\n[[plugins]]\nid = \"failing\"\nruntime = \"native\"\nsource = \"rollback-failing-native\"\n",
    )
    .unwrap();
    root
});

fn test_modes_root() -> PathBuf {
    TEST_MODES_ROOT.clone()
}

fn modes() -> Arc<AgentModeRegistry> {
    Arc::new(AgentModeRegistry::with_roots(
        vec![AgentModeRoot {
            path: test_modes_root(),
            trust: AgentModeTrust::User,
        }],
        None,
    ))
}

fn factory(llm: LlmRuntime, prompt: SystemPrompt, tools: ToolRuntime) -> AgentLoopFactory {
    AgentLoopFactory::new(
        llm,
        prompt,
        tools,
        modes(),
        AgentModeId::new("test-empty").unwrap(),
    )
    .with_persistent_shell_sessions(PersistentShellSessions::new())
}
struct UnusedResolver;

impl PackageResolver for UnusedResolver {
    fn resolve<'a>(
        &'a self,
        _specifier: &'a str,
        _runtime: RuntimeKind,
    ) -> LoaderFuture<'a, ResolvedPackage> {
        Box::pin(async {
            Err(LoaderError::Validation(
                "composition resolver was not expected".into(),
            ))
        })
    }
}

fn composition_registry() -> CompositionRegistry {
    CompositionRegistry::new(
        Arc::new(UnusedResolver),
        Vec::<Arc<dyn LoaderRuntime>>::new(),
    )
    .unwrap()
}

#[derive(Default)]
struct TurnEndGatePersistence {
    inner: MemorySessionPersistence,
    block_turn_end: AtomicBool,
    turn_end_started: tokio::sync::Notify,
    release_turn_end: tokio::sync::Notify,
}

#[async_trait]
impl SessionPersistence for TurnEndGatePersistence {
    async fn create(
        &self,
        header: &SessionHeader,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.inner.create(header, cancellation).await
    }

    async fn append(
        &self,
        session_id: &SessionId,
        event: &SessionEvent,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        if event.event_type == "turn/end" && self.block_turn_end.swap(false, Ordering::AcqRel) {
            self.turn_end_started.notify_one();
            self.release_turn_end.notified().await;
        }
        self.inner.append(session_id, event, cancellation).await
    }

    async fn create_seeded(
        &self,
        header: &SessionHeader,
        events: &[SessionEvent],
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.inner.create_seeded(header, events, cancellation).await
    }

    async fn load(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionHeader>, SessionError> {
        self.inner.load(session_id, cancellation).await
    }

    async fn inspect(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<Option<SessionInspection>, SessionError> {
        self.inner.inspect(session_id, cancellation).await
    }

    async fn read_from(
        &self,
        session_id: &SessionId,
        from_seq: u64,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        self.inner
            .read_from(session_id, from_seq, cancellation)
            .await
    }

    async fn flush(
        &self,
        session_id: &SessionId,
        cancellation: CancellationToken,
    ) -> Result<(), SessionError> {
        self.inner.flush(session_id, cancellation).await
    }

    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> Result<Vec<SessionInspection>, SessionError> {
        self.inner.list(cancellation).await
    }
}
struct LifecyclePlugin {
    live: Arc<AtomicUsize>,
}

impl NativePlugin for LifecyclePlugin {
    fn descriptor(&self) -> NativePluginDescriptor {
        NativePluginDescriptor {
            name: "mode-lifecycle-fixture".into(),
            version: "1".into(),
            dependencies: Vec::new(),
            config_schema: NativeConfigSchema::Any,
        }
    }

    fn start<'a>(
        &'a mut self,
        _context: ContextHandle,
        config: &'a Value,
    ) -> NativePluginFuture<'a> {
        assert_eq!(config["value"], 7);
        self.live.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Ok(()) })
    }

    fn update<'a>(
        &'a mut self,
        _context: ContextHandle,
        _config: &'a Value,
    ) -> NativePluginFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn stop<'a>(&'a mut self, _context: ContextHandle) -> NativePluginFuture<'a> {
        self.live.fetch_sub(1, Ordering::AcqRel);
        Box::pin(async { Ok(()) })
    }
}

struct GatedLifecyclePlugin {
    live: Arc<AtomicUsize>,
    block_next_start: Arc<AtomicBool>,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    stopped: Arc<tokio::sync::Notify>,
}

impl NativePlugin for GatedLifecyclePlugin {
    fn descriptor(&self) -> NativePluginDescriptor {
        NativePluginDescriptor {
            name: "gated-mode-lifecycle-fixture".into(),
            version: "1".into(),
            dependencies: Vec::new(),
            config_schema: NativeConfigSchema::Any,
        }
    }

    fn start<'a>(
        &'a mut self,
        _context: ContextHandle,
        config: &'a Value,
    ) -> NativePluginFuture<'a> {
        assert_eq!(config["value"], 7);
        self.live.fetch_add(1, Ordering::AcqRel);
        Box::pin(async move {
            if self.block_next_start.swap(false, Ordering::AcqRel) {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok(())
        })
    }

    fn update<'a>(
        &'a mut self,
        _context: ContextHandle,
        _config: &'a Value,
    ) -> NativePluginFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn stop<'a>(&'a mut self, _context: ContextHandle) -> NativePluginFuture<'a> {
        self.live.fetch_sub(1, Ordering::AcqRel);
        self.stopped.notify_one();
        Box::pin(async { Ok(()) })
    }
}

struct RollbackRuntime {
    inner: NativePluginRuntime,
    fail_next_cleanup: Arc<AtomicBool>,
    block_next_retry: Arc<AtomicBool>,
    retry_started: Arc<tokio::sync::Notify>,
    release_retry: Arc<tokio::sync::Notify>,
}

impl LoaderRuntime for RollbackRuntime {
    fn kind(&self) -> RuntimeKind {
        self.inner.kind()
    }

    fn instantiate<'a>(
        &'a self,
        package: ResolvedPackage,
        entry: Entry,
        context: ContextHandle,
    ) -> LoaderFuture<'a, Box<dyn RuntimeHandle>> {
        Box::pin(async move {
            let inner = self.inner.instantiate(package, entry, context).await?;
            Ok(Box::new(RollbackHandle {
                inner,
                fail_next_cleanup: Arc::clone(&self.fail_next_cleanup),
                block_next_retry: Arc::clone(&self.block_next_retry),
                retry_started: Arc::clone(&self.retry_started),
                release_retry: Arc::clone(&self.release_retry),
                activated: false,
            }) as Box<dyn RuntimeHandle>)
        })
    }
}

struct RollbackHandle {
    inner: Box<dyn RuntimeHandle>,
    fail_next_cleanup: Arc<AtomicBool>,
    block_next_retry: Arc<AtomicBool>,
    retry_started: Arc<tokio::sync::Notify>,
    release_retry: Arc<tokio::sync::Notify>,
    activated: bool,
}

impl RuntimeHandle for RollbackHandle {
    fn activate<'a>(&'a mut self) -> LoaderFuture<'a, ()> {
        Box::pin(async move {
            self.inner.activate().await?;
            self.activated = true;
            Ok(())
        })
    }

    fn activation<'a>(&'a mut self) -> LoaderFuture<'a, ActivationState> {
        Box::pin(async move {
            let state = self.inner.activation().await?;
            self.activated = state == ActivationState::Active;
            Ok(state)
        })
    }

    fn dispose<'a>(&'a mut self) -> LoaderFuture<'a, ()> {
        Box::pin(async move {
            if self.activated && self.fail_next_cleanup.swap(false, Ordering::AcqRel) {
                return Err(LoaderError::Validation(
                    "fixture transient cleanup failure".into(),
                ));
            }
            if self.activated && self.block_next_retry.swap(false, Ordering::AcqRel) {
                self.retry_started.notify_one();
                self.release_retry.notified().await;
            }
            self.inner.dispose().await
        })
    }
}

struct FailingStartPlugin {
    fail_next_start: Arc<AtomicBool>,
}

impl NativePlugin for FailingStartPlugin {
    fn descriptor(&self) -> NativePluginDescriptor {
        NativePluginDescriptor {
            name: "failing-start-fixture".into(),
            version: "1".into(),
            dependencies: Vec::new(),
            config_schema: NativeConfigSchema::Any,
        }
    }

    fn start<'a>(
        &'a mut self,
        _context: ContextHandle,
        _config: &'a Value,
    ) -> NativePluginFuture<'a> {
        Box::pin(async move {
            if self.fail_next_start.swap(false, Ordering::AcqRel) {
                Err(NativePluginError::plugin(
                    NativePluginPhase::Start,
                    "fixture activation failure",
                ))
            } else {
                Ok(())
            }
        })
    }

    fn update<'a>(
        &'a mut self,
        _context: ContextHandle,
        _config: &'a Value,
    ) -> NativePluginFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn stop<'a>(&'a mut self, _context: ContextHandle) -> NativePluginFuture<'a> {
        Box::pin(async { Ok(()) })
    }
}

fn ptc_runtime() -> ProcessCodeRuntime {
    ProcessCodeRuntime::new(ProcessCodeRuntimeConfig::ptc_javascript().unwrap()).unwrap()
}

fn user(id: &str) -> Message {
    Message {
        id: id.into(),
        role: MessageRole::User,
        content: vec![ContentBlock::Text { text: id.into() }],
        source: MessageSource::User {
            client_time_zone: None,
        },
    }
}

#[derive(Clone)]
struct DeterministicAdapter {
    streams: Arc<Mutex<VecDeque<Vec<StreamChunk>>>>,
}

#[async_trait]
impl LlmAdapter for DeterministicAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, tessivum::TessivumError> {
        Ok(Box::pin(stream::iter(
            self.streams
                .lock()
                .unwrap()
                .pop_front()
                .unwrap()
                .into_iter()
                .map(Ok),
        )))
    }
}

#[derive(Clone)]
struct RecordingAdapter {
    requests: Arc<parking_lot::Mutex<Vec<GenerateRequest>>>,
    streams: Arc<parking_lot::Mutex<VecDeque<Vec<StreamChunk>>>>,
}

#[async_trait]
impl LlmAdapter for RecordingAdapter {
    async fn generate(
        &self,
        request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, tessivum::TessivumError> {
        self.requests.lock().push(request);
        Ok(Box::pin(stream::iter(
            self.streams.lock().pop_front().unwrap().into_iter().map(Ok),
        )))
    }
}

struct BlockingAdapter;

#[async_trait]
impl LlmAdapter for BlockingAdapter {
    async fn generate(
        &self,
        _request: GenerateRequest,
        _cancellation: CancellationToken,
    ) -> Result<LlmStream, tessivum::TessivumError> {
        Ok(Box::pin(stream::pending()))
    }
}

struct Echo;

#[async_trait]
impl ToolHandler for Echo {
    async fn run(&self, _context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: arguments["value"].as_str().unwrap().into(),
            }],
            false,
            Value::Null,
        ))
    }
}

struct GatedEcho {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ToolHandler for GatedEcho {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        self.started.notify_one();
        self.release.notified().await;
        Echo.run(context, arguments).await
    }
}

struct BlockingTool;

#[async_trait]
impl ToolHandler for BlockingTool {
    async fn run(&self, context: ToolRunContext, _arguments: Value) -> ToolHandlerResult {
        context.cancellation.cancelled().await;
        Ok(ToolOutput::new(Vec::new(), false, Value::Null))
    }
}

struct PromptChangingEcho {
    prompt: SystemPrompt,
    registration: Arc<Mutex<Option<PromptRegistration>>>,
}

#[async_trait]
impl ToolHandler for PromptChangingEcho {
    async fn run(&self, _context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let registration = self
            .prompt
            .register(PromptSection::new("changed", 0, "changed"))?;
        *self.registration.lock().unwrap() = Some(registration);
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: arguments["value"].as_str().unwrap().into(),
            }],
            false,
            Value::Null,
        ))
    }
}

fn tool_turn() -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".into(),
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: ToolCallId::from("call-1"),
            name: Some("read".into()),
            arguments_delta: r#"{"value":"round-trip"}"#.into(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: ToolCallId::from("call-1"),
                name: "read".into(),
                arguments: r#"{"value":"round-trip"}"#.into(),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
            replay_state: None,
        },
    ]
}

fn text_turn(text: &str) -> Vec<StreamChunk> {
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

async fn durable_events(adapter: Arc<dyn LlmAdapter>) -> Vec<SessionEvent> {
    let llm = LlmRuntime::new();
    let _provider = llm.register("test", adapter).unwrap();
    let tools = ToolRuntime::new();
    let _tool = tools
        .register(ToolDefinition::new(
            "read",
            "reads",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            Echo,
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, SystemPrompt::new(), tools)))
        .unwrap();
    let agent = registry
        .create(
            header_with_mode("replay-equivalence", "test-read"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    agent.followup(user("question")).await.unwrap();
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    agent.dispose().await.unwrap();
    events
}

fn normalize_generated_message_ids(events: Vec<SessionEvent>) -> Vec<Value> {
    events
        .into_iter()
        .map(|event| {
            let mut value = serde_json::to_value(event).unwrap();
            value["time"] = json!("<generated>");
            if let Some(message) = value["data"]["message"].as_object_mut() {
                message.insert("id".into(), json!("<generated>"));
            }
            value
        })
        .collect()
}

#[tokio::test]
async fn recorded_replay_matches_a_native_adapter_through_the_durable_tool_loop() {
    let native = Arc::new(DeterministicAdapter {
        streams: Arc::new(Mutex::new(VecDeque::from([
            tool_turn(),
            text_turn("native and replay agree"),
        ]))),
    });
    let recording = [
        json!({
            "sessionId": "replay-equivalence",
            "provider": "test",
            "model": "deterministic",
            "requestId": "tool",
            "chunks": tool_turn(),
        }),
        json!({
            "sessionId": "replay-equivalence",
            "provider": "test",
            "model": "deterministic",
            "requestId": "text",
            "chunks": text_turn("native and replay agree"),
        }),
    ]
    .into_iter()
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    let replay = Arc::new(RecordedLlmAdapter::from_jsonl(&recording).unwrap());

    let native_events = durable_events(native).await;
    let replay_events = durable_events(replay.clone()).await;
    assert!(native_events.iter().all(|event| event.time > 0));
    assert!(replay_events.iter().all(|event| event.time > 0));
    assert_eq!(
        normalize_generated_message_ids(replay_events),
        normalize_generated_message_ids(native_events),
    );
    replay.assert_consumed().unwrap();
}

fn failed_turn(code: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".into(),
        },
        StreamChunk::TextDelta {
            index: 0,
            text: "discarded partial output".into(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text {
                text: "discarded partial output".into(),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::Error {
                failure: LlmFailure {
                    message: "transient provider failure".into(),
                    code: code.into(),
                    status: None,
                    provider_retry_after_ms: None,
                    request_id: None,
                },
            },
            replay_state: None,
        },
    ]
}

fn failed_tool_turn() -> Vec<StreamChunk> {
    let mut chunks = tool_turn();
    *chunks.last_mut().unwrap() = StreamChunk::Finish {
        reason: FinishReason::Error {
            failure: LlmFailure {
                message: "malformed provider termination".into(),
                code: "MALFORMED_STREAM".into(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            },
        },
        replay_state: None,
    };
    chunks
}

#[tokio::test]
async fn durable_tool_round_trip_records_balanced_model_ordered_events() {
    let llm = LlmRuntime::new();
    let adapter = DeterministicAdapter {
        streams: Arc::new(Mutex::new(VecDeque::from([tool_turn(), text_turn("done")]))),
    };
    let _provider = llm.register("test", Arc::new(adapter)).unwrap();
    let tools = ToolRuntime::new();
    let _tool = tools
        .register(ToolDefinition::new(
            "read",
            "reads",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            Echo,
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, SystemPrompt::new(), tools)))
        .unwrap();
    let agent = registry
        .create(
            header_with_mode("round-trip", "test-read"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("question")).await.unwrap();
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec![
            "turn/start",
            "step/start",
            "user/message",
            "user/message",
            "request/header",
            "request/context",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/message",
            "tool/call",
            "tool/result",
            "step/end",
            "step/start",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/message",
            "step/end",
            "turn/end",
        ],
    );
    let user_message = events
        .iter()
        .find(|event| event.event_type == "user/message")
        .unwrap();
    assert_eq!(
        user_message.data,
        serde_json::to_value(user("question")).unwrap()
    );
    assert_eq!(user_message.surface_op, Some(SurfaceOp::Append));
    assert_eq!(user_message.source_event_seqs, None);
    let request_headers = events
        .iter()
        .filter(|event| event.event_type == "request/header")
        .collect::<Vec<_>>();
    assert_eq!(request_headers.len(), 1);
    assert_eq!(
        request_headers[0].data,
        json!({
            "header": {
                "config": {"provider": "test", "model": "deterministic"},
                "system": "Use the additive Tessivum persona, workspace instructions, and runtime context.",
                "tools": [{
                    "name": "read",
                    "description": "reads",
                    "parameters": {"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}
                }]
            },
            "reason": "initial"
        })
    );
    let assistant = events
        .iter()
        .find(|event| event.event_type == "assistant/message")
        .unwrap();
    assert_eq!(assistant.source_event_seqs.as_ref().unwrap().len(), 4);
    assert_eq!(
        agent.session().derive_messages().last().unwrap().content,
        vec![ContentBlock::Text {
            text: "done".into()
        }],
    );
    assert!(events.iter().all(
        |event| event.event_type != "turn/end" || event.data["reason"]["kind"] != "interrupted"
    ));
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn durable_inbox_claims_precede_their_fifo_user_messages() {
    let llm = LlmRuntime::new();
    let adapter = DeterministicAdapter {
        streams: Arc::new(Mutex::new(VecDeque::from([
            text_turn("first"),
            text_turn("second"),
        ]))),
    };
    let _provider = llm.register("test", Arc::new(adapter)).unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("durable-inbox-claims"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let steering = user("steering");
    let followup = user("followup");
    let session = agent.session();
    for (target, message) in [("next-step", &steering), ("next-turn", &followup)] {
        session
            .append(
                SessionEvent {
                    event_type: "agent/inbox/enqueued".into(),
                    seq: session.next_seq().unwrap(),
                    time: 0,
                    data: json!({"target": target, "message": message}),
                    ignorable: None,
                    source_event_seqs: None,
                    surface_op: None,
                },
                cancellation(),
            )
            .await
            .unwrap();
    }
    agent.steer(steering).await.unwrap();
    agent.followup(followup).await.unwrap();
    agent.when_idle().await.unwrap();

    let events = session.events();
    let claims = events
        .iter()
        .filter(|event| event.event_type == "agent/inbox/spliced")
        .map(|event| event.data.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        claims,
        vec![
            json!({"target": "next-turn", "start": 0, "removedCount": 1, "inserted": []}),
            json!({"target": "next-step", "start": 0, "removedCount": 1, "inserted": []}),
        ]
    );
    let claimed = events
        .iter()
        .filter(|event| event.event_type == "user/message")
        .filter_map(|event| event.data["id"].as_str())
        .filter(|id| matches!(*id, "steering" | "followup"))
        .collect::<Vec<_>>();
    assert_eq!(claimed, vec!["steering", "followup"]);
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn changed_effective_header_emits_change_event() {
    let llm = LlmRuntime::new();
    let adapter = DeterministicAdapter {
        streams: Arc::new(Mutex::new(VecDeque::from([tool_turn(), text_turn("done")]))),
    };
    let _provider = llm.register("test", Arc::new(adapter)).unwrap();
    let tools = ToolRuntime::new();
    let prompt = SystemPrompt::new();
    let registrations = Arc::new(Mutex::new(None));
    let _tool = tools
        .register(ToolDefinition::new(
            "read",
            "reads",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            PromptChangingEcho {
                prompt: prompt.clone(),
                registration: Arc::clone(&registrations),
            },
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, prompt, tools)))
        .unwrap();
    let agent = registry
        .create(
            header_with_mode("changed-header", "test-read"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("question")).await.unwrap();
    agent.when_idle().await.unwrap();
    let headers = agent
        .session()
        .events()
        .into_iter()
        .filter(|event| event.event_type == "request/header")
        .collect::<Vec<_>>();
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].data["reason"], "initial");
    assert_eq!(headers[1].data["reason"], "change");
    assert_eq!(
        headers[0]
            .data
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["header", "reason"]
    );
    assert_eq!(
        headers[1]
            .data
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["header", "reason"]
    );
    assert_ne!(headers[0].data["header"], headers[1].data["header"]);
    assert_eq!(
        headers[1].data["header"]["system"],
        "Use the additive Tessivum persona, workspace instructions, and runtime context.\n\nchanged"
    );
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn preloaded_request_header_makes_first_runtime_header_resume() {
    let store = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let session = store
        .create(header("resumed"), cancellation())
        .await
        .unwrap();
    session
        .append(
            SessionEvent {
                event_type: "request/header".into(),
                seq: 0,
                time: 0,
                data: json!({
                    "header": {"config": {"provider": "previous", "model": "previous"}},
                    "reason": "initial"
                }),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let llm = LlmRuntime::new();
    let adapter = DeterministicAdapter {
        streams: Arc::new(Mutex::new(VecDeque::from([text_turn("resumed")]))),
    };
    let _provider = llm.register("test", Arc::new(adapter)).unwrap();
    let registry = AgentRegistry::new(store);
    let _factory = registry
        .register_factory(Arc::new(factory(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .resume(
            SessionId::from("resumed"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("question")).await.unwrap();
    agent.when_idle().await.unwrap();
    let headers = agent
        .session()
        .events()
        .into_iter()
        .filter(|event| event.event_type == "request/header")
        .collect::<Vec<_>>();
    assert_eq!(headers.len(), 2);
    assert_eq!(
        headers[1].data,
        json!({
            "header": {
                "config": {"provider": "test", "model": "deterministic"},
                "system": "Use the additive Tessivum persona, workspace instructions, and runtime context."
            },
            "reason": "resume"
        })
    );
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn restored_legacy_runtime_context_gets_native_snapshots_only_when_state_changes() {
    let persistence = Arc::new(MemorySessionPersistence::new());
    let writer = SessionStore::new(persistence.clone());
    let mut restored_header = header_with_mode("legacy-runtime-context", "test-read");
    restored_header.cwd = Some("/workspace/project".into());
    let session = writer
        .create(restored_header, cancellation())
        .await
        .unwrap();
    session
        .append(
            SessionEvent {
                event_type: "user/message".into(),
                seq: 0,
                time: 7,
                data: json!({
                    "id": "legacy-context",
                    "role": "user",
                    "content": [{"type": "text", "text": "legacy runtime context"}],
                    "source": {
                        "kind": "plugin",
                        "plugin": "@deepseek-ai/dsh-system-prompt"
                    }
                }),
                ignorable: None,
                source_event_seqs: None,
                surface_op: Some(SurfaceOp::Append),
            },
            cancellation(),
        )
        .await
        .unwrap();
    let history = session.events();
    let history_bytes = serde_json::to_vec(&history).unwrap();
    drop(session);
    drop(writer);

    let llm = LlmRuntime::new();
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let tool_started = Arc::new(tokio::sync::Notify::new());
    let tool_release = Arc::new(tokio::sync::Notify::new());
    let adapter = RecordingAdapter {
        requests: Arc::clone(&requests),
        streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([
            text_turn("first"),
            text_turn("second"),
            tool_turn(),
            text_turn("changed"),
        ]))),
    };
    let _provider = llm.register("test", Arc::new(adapter)).unwrap();
    let tools = ToolRuntime::new();
    let _tool = tools
        .register(ToolDefinition::new(
            "read",
            "reads",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            GatedEcho {
                started: Arc::clone(&tool_started),
                release: Arc::clone(&tool_release),
            },
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(persistence));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, SystemPrompt::new(), tools)))
        .unwrap();
    let agent = registry
        .resume(
            SessionId::from("legacy-runtime-context"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let user_at = |id: &str| Message {
        id: id.into(),
        role: MessageRole::User,
        content: vec![ContentBlock::Text { text: id.into() }],
        source: MessageSource::User {
            client_time_zone: Some("Asia/Shanghai".into()),
        },
    };
    let native_contexts = || {
        agent
            .session()
            .events()
            .into_iter()
            .filter(|event| {
                event.event_type == "user/message"
                    && event.data["source"]["plugin"] == "tessivum/runtime-context"
            })
            .collect::<Vec<_>>()
    };

    agent.followup(user_at("first-user")).await.unwrap();
    agent.when_idle().await.unwrap();
    let first = native_contexts();
    assert_eq!(first.len(), 1);
    let first_text = first[0].data["content"][0]["text"].as_str().unwrap();
    assert!(first_text.contains("Session workspace: \"/workspace/project\"."));
    assert!(first_text.contains("Standing Tessivum sandbox policy: workspace-write."));
    assert!(first_text.contains("Approval policy: ask."));
    assert!(first_text.contains("Browser time zone for this request: Asia/Shanghai."));

    agent.followup(user_at("second-user")).await.unwrap();
    agent.when_idle().await.unwrap();
    assert_eq!(native_contexts().len(), 1);

    agent.followup(user_at("third-user")).await.unwrap();
    tool_started.notified().await;
    for (event_type, data) in [
        ("sandbox/mode", json!({"mode": "danger-full-access"})),
        ("approval/policy", json!({"policy": "never"})),
    ] {
        agent
            .session()
            .append_next(
                |seq| SessionEvent {
                    event_type: event_type.into(),
                    seq,
                    time: 8,
                    data,
                    ignorable: None,
                    source_event_seqs: None,
                    surface_op: None,
                },
                cancellation(),
            )
            .await
            .unwrap();
    }
    tool_release.notify_one();
    agent.when_idle().await.unwrap();
    let contexts = native_contexts();
    assert_eq!(contexts.len(), 2);
    let changed = contexts[1].data["content"][0]["text"].as_str().unwrap();
    assert!(changed.contains("Standing Tessivum sandbox policy: danger-full-access."));
    assert!(changed.contains("Approval policy: never."));
    assert_ne!(contexts[0].data["id"], contexts[1].data["id"]);
    {
        let requests = requests.lock();
        assert_eq!(requests.len(), 4);
        let next_context = requests[3]
        .messages
        .iter()
        .rev()
        .find(|message| {
            matches!(&message.source, MessageSource::Plugin { plugin, .. } if plugin == "tessivum/runtime-context")
        })
        .unwrap();
        assert!(matches!(
            next_context.content.as_slice(),
            [ContentBlock::Text { text }] if text == changed
        ));
    }

    let events = agent.session().events();
    assert_eq!(
        serde_json::to_vec(&events[..history.len()]).unwrap(),
        history_bytes
    );
    assert_eq!(
        events[0].data["source"]["plugin"],
        "@deepseek-ai/dsh-system-prompt"
    );
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn retry_preserves_partial_chunks_without_committing_or_executing_them() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register_with_retry_policy(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([
                    failed_turn("TRANSPORT"),
                    text_turn("recovered"),
                ]))),
            }),
            Some(LlmRetryPolicy::resolve(None).unwrap()),
        )
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("partial-retry"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("retry-input")).await.unwrap();
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    let retry_seq = events
        .iter()
        .find(|event| event.event_type == "llm/retry")
        .unwrap()
        .seq;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/chunk" && event.seq < retry_seq)
            .count(),
        4
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/message")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "llm/retry-started")
            .count(),
        1
    );
    assert!(events.iter().all(|event| event.event_type != "tool/call"));
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn retry_budget_is_reconstructed_from_the_durable_ledger() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register_with_retry_policy(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([
                    failed_turn("SERVER"),
                    failed_turn("SERVER"),
                    failed_turn("SERVER"),
                ]))),
            }),
            Some(LlmRetryPolicy::resolve(None).unwrap()),
        )
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("retry-exhausted"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("retry-input")).await.unwrap();
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "llm/retry")
            .count(),
        2
    );
    let retry = events
        .iter()
        .find(|event| event.event_type == "llm/retry")
        .unwrap();
    assert_eq!(retry.data["mode"], "normal");
    assert_eq!(retry.data["maxRetries"], 2);
    assert!(retry.data["policyKey"].is_string());
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "step/end")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "turn/end")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "error");
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn cancellation_during_backoff_wins_without_starting_another_attempt() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register_with_retry_policy(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([
                    failed_turn("RATE_LIMIT"),
                    text_turn("must not run"),
                ]))),
            }),
            Some(LlmRetryPolicy::resolve(None).unwrap()),
        )
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("cancel-retry"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let session = agent.session();
    let mut updates = session.subscribe();

    agent.followup(user("retry-input")).await.unwrap();
    loop {
        if updates.recv().await.unwrap().event_type == "llm/retry" {
            break;
        }
    }
    assert!(agent.cancel(AgentCancelCause::User, false));
    agent.when_idle().await.unwrap();
    let events = session.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "llm/retry-started")
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "step/end")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "aborted");
    assert_eq!(
        events.last().unwrap().data["reason"]["reason"]["kind"],
        "user"
    );
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn cancellation_during_provider_wait_closes_one_step_and_turn() {
    let llm = LlmRuntime::new();
    let _provider = llm.register("test", Arc::new(BlockingAdapter)).unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("cancel-provider"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let session = agent.session();
    let mut updates = session.subscribe();

    agent.followup(user("wait")).await.unwrap();
    loop {
        if updates.recv().await.unwrap().event_type == "step/start" {
            break;
        }
    }
    assert!(agent.cancel(AgentCancelCause::User, false));
    agent.when_idle().await.unwrap();
    let events = session.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/message")
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "step/end")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "turn/end")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "aborted");
    assert_eq!(
        events.last().unwrap().data["reason"]["reason"]["kind"],
        "user"
    );
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn cancellation_during_tool_wait_settles_the_started_call_once() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([tool_turn()]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _tool = tools
        .register(ToolDefinition::new(
            "read",
            "waits",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}}}),
            BlockingTool,
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, SystemPrompt::new(), tools)))
        .unwrap();
    let agent = registry
        .create(
            header_with_mode("cancel-tool", "test-read"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();
    let session = agent.session();
    let mut updates = session.subscribe();

    agent.followup(user("tool")).await.unwrap();
    loop {
        if updates.recv().await.unwrap().event_type == "tool/call" {
            break;
        }
    }
    assert!(agent.cancel(AgentCancelCause::User, false));
    agent.when_idle().await.unwrap();
    let events = session.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool/call")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool/result")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "step/end")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "turn/end")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "aborted");
    assert_eq!(
        events.last().unwrap().data["reason"]["reason"]["kind"],
        "user"
    );
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn failed_tool_stream_never_starts_durable_tool_lifecycle() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([failed_tool_turn()]))),
            }),
        )
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(
            llm,
            SystemPrompt::new(),
            ToolRuntime::new(),
        )))
        .unwrap();
    let agent = registry
        .create(
            header("failed-tool-stream"),
            AgentOptions {
                provider: "test".into(),
                model: "deterministic".into(),
                reasoning_effort: None,
                max_tokens: None,
            },
            cancellation(),
        )
        .await
        .unwrap();

    agent.followup(user("tool")).await.unwrap();
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/chunk")
            .count(),
        4
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "assistant/message")
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool/call")
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool/result")
            .count(),
        0
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "step/end")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "turn/end")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "error");
    agent.dispose().await.unwrap();
}

fn install_tools(runtime: &ToolRuntime, names: &[&str]) -> Vec<tessivum::tools::ToolRegistration> {
    names
        .iter()
        .map(|name| {
            runtime
                .register(ToolDefinition::new(
                    *name,
                    *name,
                    json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}},"additionalProperties":false}),
                    Echo,
                ))
                .unwrap()
        })
        .collect()
}
fn standard_tool_names() -> Vec<&'static str> {
    vec![
        "ask_user_question",
        "bash",
        "create_goal",
        "edit",
        "exit_plan_mode",
        "get_goal",
        "glob",
        "grep",
        "interrupt_agent",
        "jobs.kill",
        "jobs.list",
        "jobs.read",
        "jobs.wait",
        "list_agents",
        "ralph",
        "read",
        "read_image",
        "schedule_create",
        "schedule_delete",
        "schedule_list",
        "send_message",
        "str_replace_editor",
        "subagent",
        "subagent_fork",
        "todo_write",
        "update_goal",
        "web_fetch",
        "web_search",
        "workflow",
        "write",
    ]
}

fn options() -> AgentOptions {
    AgentOptions {
        provider: "test".into(),
        model: "deterministic".into(),
        reasoning_effort: None,
        max_tokens: None,
    }
}

fn tool_names(request: &GenerateRequest) -> Vec<String> {
    request
        .tools
        .as_ref()
        .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
        .unwrap_or_default()
}

fn request_for<'a>(requests: &'a [GenerateRequest], session: &str) -> &'a GenerateRequest {
    requests
        .iter()
        .find(|request| {
            request
                .session_id
                .as_ref()
                .is_some_and(|id| id.as_str() == session)
        })
        .unwrap()
}

fn run_code_turn(code: &str) -> Vec<StreamChunk> {
    let arguments = json!({"description":"nested test","code":code}).to_string();
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".into(),
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: ToolCallId::from("code-call"),
            name: Some("run_code".into()),
            arguments_delta: arguments.clone(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: ToolCallId::from("code-call"),
                name: "run_code".into(),
                arguments,
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
            replay_state: None,
        },
    ]
}

#[tokio::test]
async fn four_session_runtime_specs_are_isolated() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([
                    text_turn("standard"),
                    text_turn("minimal"),
                    text_turn("composition"),
                    text_turn("ptc"),
                ]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let mut host_tools = standard_tool_names();
    host_tools.extend([
        "composition_define",
        "composition_inspect",
        "composition_run",
        "composition_stop",
        "composition_validate",
    ]);
    host_tools.sort_unstable();
    let _tools = install_tools(&tools, &host_tools);
    let prompt = SystemPrompt::new();
    let _host_prompt = prompt
        .register(PromptSection::new("host", 0, "host prompt"))
        .unwrap();
    let persistent_shells = PersistentShellSessions::new();
    let compositions = composition_registry();
    let root_context = ContextHandle::root();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            AgentLoopFactory::new(llm, prompt, tools, modes(), AgentModeId::standard())
                .with_code_runtime(ptc_runtime())
                .with_persistent_shell_sessions(persistent_shells.clone())
                .with_composition_registry(compositions.clone())
                .with_root_context(root_context.clone()),
        ))
        .unwrap();

    let standard = registry
        .create(header("mode-standard"), options(), cancellation())
        .await
        .unwrap();
    assert!(standard.session().events().iter().any(|event| {
        event.event_type == "agent-mode/selected" && event.data == json!({"agentMode": "standard"})
    }));
    let mut minimal_header = header("mode-minimal");
    minimal_header.agent_mode = Some(AgentModeId::minimal());
    let minimal = registry
        .create(minimal_header, options(), cancellation())
        .await
        .unwrap();
    let mut composition_header = header("mode-composition");
    composition_header.agent_mode = Some(AgentModeId::composition());
    let composition = registry
        .create(composition_header, options(), cancellation())
        .await
        .unwrap();
    let mut ptc_header = header("mode-ptc");
    ptc_header.agent_mode = Some(AgentModeId::ptc());
    let ptc = registry
        .create(ptc_header, options(), cancellation())
        .await
        .unwrap();
    assert_eq!(
        format!("{persistent_shells:?}"),
        "PersistentShellSessions { session_count: 1 }"
    );
    assert!(compositions
        .inspect(&SessionId::from("mode-composition"), None)
        .await
        .is_ok());

    let (standard_result, minimal_result, composition_result, ptc_result) = tokio::join!(
        standard.followup(user("standard")),
        minimal.followup(user("minimal")),
        composition.followup(user("composition")),
        ptc.followup(user("ptc")),
    );
    standard_result.unwrap();
    minimal_result.unwrap();
    composition_result.unwrap();
    ptc_result.unwrap();
    let (standard_idle, minimal_idle, composition_idle, ptc_idle) = tokio::join!(
        standard.when_idle(),
        minimal.when_idle(),
        composition.when_idle(),
        ptc.when_idle(),
    );
    standard_idle.unwrap();
    minimal_idle.unwrap();
    composition_idle.unwrap();
    ptc_idle.unwrap();

    {
        let requests = requests.lock();
        assert_eq!(
            tool_names(request_for(&requests, "mode-standard")),
            standard_tool_names()
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            tool_names(request_for(&requests, "mode-minimal")),
            ["bash", "str_replace_editor"]
        );
        let mut composition_tools = standard_tool_names();
        composition_tools.extend([
            "composition_define",
            "composition_inspect",
            "composition_run",
            "composition_stop",
            "composition_validate",
        ]);
        composition_tools.sort_unstable();
        assert_eq!(
            tool_names(request_for(&requests, "mode-composition")),
            composition_tools
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        );
        assert_eq!(tool_names(request_for(&requests, "mode-ptc")), ["run_code"]);
        drop(requests);
    }
    standard.dispose().await.unwrap();
    minimal.dispose().await.unwrap();
    composition.dispose().await.unwrap();
    ptc.dispose().await.unwrap();
    assert_eq!(
        format!("{persistent_shells:?}"),
        "PersistentShellSessions { session_count: 0 }"
    );
    assert_eq!(
        compositions
            .inspect(&SessionId::from("mode-composition"), None)
            .await
            .unwrap_err()
            .code,
        "COMPOSITION_SESSION_UNAVAILABLE"
    );
    root_context.scope().dispose().await.unwrap();
}

#[tokio::test]
async fn programmatic_catalog_tracks_visible_tool_contract_changes() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([
                    text_turn("first"),
                    text_turn("updated"),
                    text_turn("removed"),
                ]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let initial = json!({"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"],"additionalProperties":false});
    let changed = json!({"type":"object","properties":{"file_path":{"type":"string"},"offset":{"type":"integer"}},"required":["file_path","offset"],"additionalProperties":false});
    let registration = tools
        .register(ToolDefinition::new(
            "read",
            "Read file content",
            initial.clone(),
            Echo,
        ))
        .unwrap();
    let _hidden = tools
        .register(ToolDefinition::new(
            "write",
            "hidden operation",
            json!({"type":"object","properties":{}}),
            Echo,
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            factory(llm, SystemPrompt::new(), tools.clone()).with_code_runtime(ptc_runtime()),
        ))
        .unwrap();
    let agent = registry
        .create(
            header_with_mode("live-catalog", "test-ptc"),
            options(),
            cancellation(),
        )
        .await
        .unwrap();
    agent.followup(user("first")).await.unwrap();
    agent.when_idle().await.unwrap();
    let replacements = tools
        .replace(
            &[registration],
            vec![ToolDefinition::new(
                "read",
                "Read from an offset",
                changed.clone(),
                Echo,
            )],
        )
        .unwrap();
    agent.followup(user("updated")).await.unwrap();
    agent.when_idle().await.unwrap();
    drop(replacements);
    agent.followup(user("removed")).await.unwrap();
    agent.when_idle().await.unwrap();
    {
        let requests = requests.lock();
        for (request, expected) in requests.iter().zip([Some(initial), Some(changed), None]) {
            let schemas = request.tools.as_ref().unwrap();
            assert_eq!(schemas.len(), 1);
            assert_eq!(schemas[0].name, "run_code");
            let catalog = schemas[0]
                .description
                .lines()
                .find_map(|line| serde_json::from_str::<Vec<ToolSchema>>(line).ok())
                .expect("model must receive the current nested tool schemas");
            match expected {
                Some(parameters) => {
                    assert_eq!(catalog.len(), 1, "hidden tools must not enter the SDK");
                    assert_eq!(catalog[0].name, "read");
                    assert_eq!(catalog[0].parameters, parameters);
                }
                None => assert!(catalog.is_empty(), "removed tools must leave the SDK"),
            }
        }
        assert_eq!(requests.len(), 3);
    }
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn mode_plugins_activate_before_agent_start_and_stop_with_the_session() {
    let live = Arc::new(AtomicUsize::new(0));
    let factory_live = Arc::clone(&live);
    let mut native = NativePluginRuntime::new();
    native
        .register("fixture-native", move || LifecyclePlugin {
            live: Arc::clone(&factory_live),
        })
        .unwrap();
    let compositions = CompositionRegistry::new(
        Arc::new(ProductPackageResolver::new().with_native_packages(["fixture-native".into()])),
        [Arc::new(native) as Arc<dyn LoaderRuntime>],
    )
    .unwrap();
    let root = ContextHandle::root();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            factory(LlmRuntime::new(), SystemPrompt::new(), ToolRuntime::new())
                .with_composition_registry(compositions.clone())
                .with_root_context(root.clone()),
        ))
        .unwrap();

    let session = SessionId::from("mode-native-plugin");
    let agent = registry
        .create(
            header_with_mode(session.as_str(), "test-native-plugin"),
            options(),
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(live.load(Ordering::Acquire), 1);
    let inspection = compositions
        .inspect(&session, Some("fixture"))
        .await
        .unwrap();
    assert_eq!(
        inspection.descriptors[0].lifecycle,
        tessivum::composition::CompositionLifecycle::Active
    );

    agent.dispose().await.unwrap();
    assert_eq!(live.load(Ordering::Acquire), 0);
    assert_eq!(
        compositions.inspect(&session, None).await.unwrap_err().code,
        "COMPOSITION_SESSION_UNAVAILABLE"
    );
    root.scope().dispose().await.unwrap();
}

#[tokio::test]
async fn dropped_native_factory_setup_finishes_and_releases_attached_resources() {
    let live = Arc::new(AtomicUsize::new(0));
    let block_next_start = Arc::new(AtomicBool::new(true));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let stopped = Arc::new(tokio::sync::Notify::new());
    let mut native = NativePluginRuntime::new();
    native
        .register("fixture-native", {
            let live = Arc::clone(&live);
            let block_next_start = Arc::clone(&block_next_start);
            let started = Arc::clone(&started);
            let release = Arc::clone(&release);
            let stopped = Arc::clone(&stopped);
            move || GatedLifecyclePlugin {
                live: Arc::clone(&live),
                block_next_start: Arc::clone(&block_next_start),
                started: Arc::clone(&started),
                release: Arc::clone(&release),
                stopped: Arc::clone(&stopped),
            }
        })
        .unwrap();
    let compositions = CompositionRegistry::new(
        Arc::new(ProductPackageResolver::new().with_native_packages(["fixture-native".into()])),
        [Arc::new(native) as Arc<dyn LoaderRuntime>],
    )
    .unwrap();
    let root = ContextHandle::root();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            factory(LlmRuntime::new(), SystemPrompt::new(), ToolRuntime::new())
                .with_composition_registry(compositions)
                .with_root_context(root.clone()),
        ))
        .unwrap();
    let session = SessionId::from("dropped-native-setup");
    let setup_started = started.notified();
    let setup = tokio::spawn({
        let registry = registry.clone();
        let session = session.clone();
        async move {
            registry
                .create(
                    header_with_mode(session.as_str(), "test-native-plugin"),
                    options(),
                    cancellation(),
                )
                .await
        }
    });
    setup_started.await;
    assert_eq!(live.load(Ordering::Acquire), 1);
    setup.abort();
    assert!(setup.await.unwrap_err().is_cancelled());

    let cleanup_finished = stopped.notified();
    release.notify_one();
    cleanup_finished.await;
    assert_eq!(live.load(Ordering::Acquire), 0);
    assert!(registry.get(&session).is_none());
    let replacement = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match registry
                .create_or_resume(
                    header_with_mode(session.as_str(), "test-native-plugin"),
                    options(),
                    cancellation(),
                )
                .await
            {
                Err(AgentError::Session(tessivum::session::SessionError::DuplicateLive(_))) => {
                    tokio::task::yield_now().await;
                }
                result => break result,
            }
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(live.load(Ordering::Acquire), 1);
    replacement.dispose().await.unwrap();
    assert_eq!(live.load(Ordering::Acquire), 0);
    root.scope().dispose().await.unwrap();
}

#[tokio::test]
async fn dropped_dispose_keeps_the_worker_join_before_resource_cleanup() {
    let live = Arc::new(AtomicUsize::new(0));
    let factory_live = Arc::clone(&live);
    let mut native = NativePluginRuntime::new();
    native
        .register("fixture-native", move || LifecyclePlugin {
            live: Arc::clone(&factory_live),
        })
        .unwrap();
    let compositions = CompositionRegistry::new(
        Arc::new(ProductPackageResolver::new().with_native_packages(["fixture-native".into()])),
        [Arc::new(native) as Arc<dyn LoaderRuntime>],
    )
    .unwrap();
    let persistence = Arc::new(TurnEndGatePersistence::default());
    persistence.block_turn_end.store(true, Ordering::Release);
    let root = ContextHandle::root();
    let registry = AgentRegistry::new(SessionStore::new(persistence.clone()));
    let _factory = registry
        .register_factory(Arc::new(
            factory(LlmRuntime::new(), SystemPrompt::new(), ToolRuntime::new())
                .with_composition_registry(compositions.clone())
                .with_root_context(root.clone()),
        ))
        .unwrap();
    let session = SessionId::from("dropped-dispose-worker");
    let agent = Arc::new(
        registry
            .create(
                header_with_mode(session.as_str(), "test-native-plugin"),
                options(),
                cancellation(),
            )
            .await
            .unwrap(),
    );
    let turn_end_started = persistence.turn_end_started.notified();
    agent.followup(user("dispose")).await.unwrap();
    turn_end_started.await;

    let first_dispose = tokio::spawn({
        let agent = Arc::clone(&agent);
        async move { agent.dispose().await }
    });
    while agent.cancel_options().is_none() {
        tokio::task::yield_now().await;
    }
    first_dispose.abort();
    assert!(first_dispose.await.unwrap_err().is_cancelled());

    assert!(
        tokio::time::timeout(Duration::from_millis(10), agent.dispose())
            .await
            .is_err()
    );
    assert_eq!(live.load(Ordering::Acquire), 1);
    persistence.release_turn_end.notify_one();
    agent.dispose().await.unwrap();
    assert_eq!(live.load(Ordering::Acquire), 0);
    assert!(registry.get(&session).is_none());
    root.scope().dispose().await.unwrap();
}

#[tokio::test]
async fn failed_plugin_activation_retries_rollback_before_releasing_setup() {
    let live = Arc::new(AtomicUsize::new(0));
    let fail_next_cleanup = Arc::new(AtomicBool::new(true));
    let block_next_retry = Arc::new(AtomicBool::new(true));
    let retry_started = Arc::new(tokio::sync::Notify::new());
    let release_retry = Arc::new(tokio::sync::Notify::new());
    let fail_next_start = Arc::new(AtomicBool::new(true));
    let mut native = NativePluginRuntime::new();
    native
        .register("rollback-active-native", {
            let live = Arc::clone(&live);
            move || LifecyclePlugin {
                live: Arc::clone(&live),
            }
        })
        .unwrap();
    native
        .register("rollback-failing-native", {
            let fail_next_start = Arc::clone(&fail_next_start);
            move || FailingStartPlugin {
                fail_next_start: Arc::clone(&fail_next_start),
            }
        })
        .unwrap();
    let runtime = RollbackRuntime {
        inner: native,
        fail_next_cleanup: Arc::clone(&fail_next_cleanup),
        block_next_retry: Arc::clone(&block_next_retry),
        retry_started: Arc::clone(&retry_started),
        release_retry: Arc::clone(&release_retry),
    };
    let compositions = CompositionRegistry::new(
        Arc::new(ProductPackageResolver::new().with_native_packages([
            "rollback-active-native".into(),
            "rollback-failing-native".into(),
        ])),
        [Arc::new(runtime) as Arc<dyn LoaderRuntime>],
    )
    .unwrap();
    let root = ContextHandle::root();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            factory(LlmRuntime::new(), SystemPrompt::new(), ToolRuntime::new())
                .with_composition_registry(compositions.clone())
                .with_root_context(root.clone()),
        ))
        .unwrap();
    let session = SessionId::from("failed-plugin-rollback");
    let rollback_started = retry_started.notified();
    let setup_cancellation = cancellation();
    let mut setup = tokio::spawn({
        let registry = registry.clone();
        let session = session.clone();
        let setup_cancellation = setup_cancellation.clone();
        async move {
            registry
                .create(
                    header_with_mode(session.as_str(), "test-plugin-rollback"),
                    options(),
                    setup_cancellation,
                )
                .await
        }
    });
    tokio::select! {
        _ = rollback_started => {},
        result = &mut setup => panic!("setup ended before rollback retry: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            panic!("failed activation never reached the rollback retry");
        }
    }
    setup_cancellation.cancel();

    assert_eq!(live.load(Ordering::Acquire), 1);
    assert!(!setup.is_finished());
    let duplicate = tokio::time::timeout(
        Duration::from_secs(1),
        registry.create_or_resume(
            header_with_mode(session.as_str(), "test-plugin-rollback"),
            options(),
            cancellation(),
        ),
    )
    .await
    .expect("same-ID create blocked instead of observing the retained reservation");
    assert!(matches!(
        duplicate,
        Err(AgentError::Session(SessionError::DuplicateLive(id))) if id == session
    ));

    release_retry.notify_one();
    let error = setup.await.unwrap().unwrap_err();
    let AgentError::Message(error) = error else {
        panic!("unexpected plugin activation error: {error:?}");
    };
    assert_eq!(error.code, "MODE_PLUGIN_ACTIVATION_FAILED");
    let failures = error.details["failures"].as_array().unwrap();
    assert!(failures.iter().any(|failure| failure
        .as_str()
        .unwrap()
        .contains("fixture activation failure")));
    assert!(failures.iter().any(|failure| failure
        .as_str()
        .unwrap()
        .contains("fixture transient cleanup failure")));
    assert_eq!(live.load(Ordering::Acquire), 0);
    assert_eq!(
        compositions.inspect(&session, None).await.unwrap_err().code,
        "COMPOSITION_SESSION_UNAVAILABLE"
    );

    let replacement = registry
        .create_or_resume(
            header_with_mode(session.as_str(), "test-plugin-rollback"),
            options(),
            cancellation(),
        )
        .await
        .unwrap();
    assert_eq!(live.load(Ordering::Acquire), 1);
    replacement.dispose().await.unwrap();
    assert_eq!(live.load(Ordering::Acquire), 0);
    root.scope().dispose().await.unwrap();
}

#[tokio::test]
async fn unknown_mode_plugin_fails_before_agent_start() {
    let compositions = CompositionRegistry::new(
        Arc::new(ProductPackageResolver::new()),
        [Arc::new(NativePluginRuntime::new()) as Arc<dyn LoaderRuntime>],
    )
    .unwrap();
    let root = ContextHandle::root();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            factory(LlmRuntime::new(), SystemPrompt::new(), ToolRuntime::new())
                .with_composition_registry(compositions.clone())
                .with_root_context(root.clone()),
        ))
        .unwrap();
    let session = SessionId::from("mode-missing-plugin");
    let error = match registry
        .create(
            header_with_mode(session.as_str(), "test-missing-plugin"),
            options(),
            cancellation(),
        )
        .await
    {
        Ok(_) => panic!("mode unexpectedly started with an unknown plugin"),
        Err(error) => error,
    };
    match error {
        AgentError::Message(error) => {
            assert_eq!(error.code, "MODE_PLUGIN_ACTIVATION_FAILED");
            assert_eq!(error.details["agentMode"], "test-missing-plugin");
        }
        other => panic!("unexpected plugin activation error: {other:?}"),
    }
    assert_eq!(
        compositions.inspect(&session, None).await.unwrap_err().code,
        "COMPOSITION_SESSION_UNAVAILABLE"
    );
    root.scope().dispose().await.unwrap();
}

#[tokio::test]
async fn complete_mode_replaces_host_prompt_while_additive_mode_contributes_once() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([
                    text_turn("standard"),
                    text_turn("minimal"),
                ]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _tools = install_tools(&tools, &["bash", "read", "str_replace_editor"]);
    let prompt = SystemPrompt::new();
    let _host_prompt = prompt
        .register(PromptSection::new("host", 0, "host prompt"))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, prompt, tools)))
        .unwrap();
    let standard = registry
        .create(header("prompt-standard"), options(), cancellation())
        .await
        .unwrap();
    let mut minimal_header = header("prompt-minimal");
    minimal_header.agent_mode = Some(AgentModeId::minimal());
    let minimal = registry
        .create(minimal_header, options(), cancellation())
        .await
        .unwrap();

    standard.followup(user("standard")).await.unwrap();
    minimal.followup(user("minimal")).await.unwrap();
    standard.when_idle().await.unwrap();
    minimal.when_idle().await.unwrap();
    {
        let requests = requests.lock();
        assert_eq!(
        request_for(&requests, "prompt-standard").system.as_deref(),
        Some("Use the additive Tessivum persona, workspace instructions, and runtime context.\n\nhost prompt")
    );
        assert_eq!(
        request_for(&requests, "prompt-minimal").system.as_deref(),
        Some("You are a helpful software engineer assistant.\n\nUse only bash and str_replace_editor to complete the task.")
    );
        drop(requests);
    }
    standard.dispose().await.unwrap();
    minimal.dispose().await.unwrap();
}

#[tokio::test]
async fn latest_mode_selection_event_overrides_header_and_programmatic_catalog_cannot_recurse() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([text_turn("ptc")]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _tools = install_tools(&tools, &["read", "run_code"]);
    let store = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let registry = AgentRegistry::new(store.clone());
    let _factory = registry
        .register_factory(Arc::new(
            factory(llm, SystemPrompt::new(), tools).with_code_runtime(ptc_runtime()),
        ))
        .unwrap();
    let mut selected = header("mode-event-wins");
    selected.agent_mode = Some(AgentModeId::minimal());
    selected.seed_length = Some(1);
    store
        .create_seeded(
            selected,
            vec![SessionEvent {
                event_type: "agent-mode/selected".into(),
                seq: 0,
                time: 0,
                data: json!({"agentMode":"test-ptc"}),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            }],
            cancellation(),
        )
        .await
        .unwrap();
    let agent = registry
        .resume(
            SessionId::from("mode-event-wins"),
            options(),
            cancellation(),
        )
        .await
        .unwrap();
    agent.followup(user("ptc")).await.unwrap();
    agent.when_idle().await.unwrap();

    {
        let requests = requests.lock();

        assert_eq!(
            tool_names(request_for(&requests, "mode-event-wins")),
            ["run_code"]
        );
        drop(requests);
    }
    agent.dispose().await.unwrap();
}

#[tokio::test]
async fn programmatic_mode_requires_configured_code_runtime() {
    let tools = ToolRuntime::new();
    let _tools = install_tools(&tools, &["read"]);
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(
            LlmRuntime::new(),
            SystemPrompt::new(),
            tools,
        )))
        .unwrap();
    let mut selected = header("missing-ptc-runtime");
    selected.agent_mode = Some(AgentModeId::new("test-ptc").unwrap());
    let error = match registry.create(selected, options(), cancellation()).await {
        Ok(_) => panic!("programmatic mode unexpectedly started without a code runtime"),
        Err(error) => error,
    };
    match error {
        AgentError::Message(error) => assert_eq!(error.code, "PTC_RUNTIME_UNAVAILABLE"),
        other => panic!("unexpected programmatic setup error: {other:?}"),
    }
}
#[tokio::test]
async fn mode_native_tools_must_exist_before_session_start() {
    let persistent_shells = PersistentShellSessions::new();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            AgentLoopFactory::new(
                LlmRuntime::new(),
                SystemPrompt::new(),
                ToolRuntime::new(),
                modes(),
                AgentModeId::new("test-empty").unwrap(),
            )
            .with_persistent_shell_sessions(persistent_shells.clone()),
        ))
        .unwrap();
    let error = match registry
        .create(
            header_with_mode("missing-native-tool", "minimal"),
            options(),
            cancellation(),
        )
        .await
    {
        Ok(_) => panic!("minimal mode unexpectedly started without its native tools"),
        Err(error) => error,
    };
    match error {
        AgentError::Message(error) => {
            assert_eq!(error.code, "MODE_NATIVE_TOOL_UNAVAILABLE");
            assert_eq!(error.details["agentMode"], "minimal");
            assert_eq!(
                error.details["missing"],
                json!(["bash", "str_replace_editor"])
            );
        }
        other => panic!("unexpected missing native tool error: {other:?}"),
    }
    assert_eq!(
        format!("{persistent_shells:?}"),
        "PersistentShellSessions { session_count: 0 }"
    );
}

#[tokio::test]
async fn native_child_mode_excludes_owner_bound_tools() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(VecDeque::from([text_turn(
                    "child",
                )]))),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _tools = install_tools(
        &tools,
        &[
            "ask_user_question",
            "bash",
            "create_goal",
            "exit_plan_mode",
            "get_goal",
            "jobs.kill",
            "jobs.list",
            "jobs.read",
            "jobs.wait",
            "read",
            "schedule_create",
            "schedule_delete",
            "schedule_list",
            "todo_write",
            "update_goal",
        ],
    );
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(factory(llm, SystemPrompt::new(), tools)))
        .unwrap();
    let mut child = header("native-child");
    child.origin = Some(SessionOrigin::Subagent);
    child.agent_mode = Some(AgentModeId::new("test-read").unwrap());
    let child = registry
        .create(child, options(), cancellation())
        .await
        .unwrap();
    child.followup(user("child")).await.unwrap();
    child.when_idle().await.unwrap();
    assert_eq!(tool_names(&requests.lock()[0]), ["read"]);
    child.dispose().await.unwrap();
}

#[tokio::test]
async fn optional_terminal_tools_follow_registration_without_widening_other_modes() {
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(RecordingAdapter {
                requests: Arc::clone(&requests),
                streams: Arc::new(parking_lot::Mutex::new(
                    (0..12).map(|_| text_turn("done")).collect(),
                )),
            }),
        )
        .unwrap();
    let tools = ToolRuntime::new();
    let _base = install_tools(
        &tools,
        &[
            "read",
            "bash",
            "str_replace_editor",
            "unrelated_plugin_tool",
        ],
    );
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            factory(
                llm,
                SystemPrompt::new(),
                tools
                    .scoped(ToolRestrictions::new().deny("terminal_list"))
                    .unwrap(),
            )
            .with_code_runtime(ptc_runtime()),
        ))
        .unwrap();
    let mut agents = Vec::new();
    for mode in ["standard", "ptc", "minimal", "test-read"] {
        agents.push((
            mode,
            registry
                .create(header_with_mode(mode, mode), options(), cancellation())
                .await
                .unwrap(),
        ));
    }
    let mut terminal_registration = Vec::new();
    for phase in 0..3 {
        if phase == 1 {
            terminal_registration = install_tools(&tools, &["terminal_create", "terminal_list"]);
        } else if phase == 2 {
            terminal_registration.clear();
        }
        for (_, agent) in &agents {
            agent
                .followup(user("inspect available tools"))
                .await
                .unwrap();
            agent.when_idle().await.unwrap();
        }
        let captured = std::mem::take(&mut *requests.lock());
        for (mode, _) in &agents {
            let request = request_for(&captured, mode);
            let visible = |name: &str| {
                request.tools.as_ref().unwrap().iter().any(|tool| {
                    tool.name == name
                        || (tool.name == "run_code"
                            && tool.description.contains(&format!("\"name\":\"{name}\"")))
                })
            };
            assert_eq!(
                visible("terminal_create"),
                phase == 1 && matches!(*mode, "standard" | "ptc"),
                "{mode}, phase {phase}"
            );
            assert!(!visible("unrelated_plugin_tool"), "{mode}");
            assert!(!visible("terminal_list"), "{mode}");
        }
    }
    for (_, agent) in agents {
        agent.dispose().await.unwrap();
    }
}

struct AlwaysApprove;

#[async_trait]
impl ToolApproval for AlwaysApprove {
    async fn approve(
        &self,
        _context: &ToolRunContext,
        _schema: &ToolSchema,
        _arguments: &Value,
    ) -> Result<Option<bool>, tessivum::TessivumError> {
        Ok(Some(true))
    }
}

#[tokio::test]
async fn programmatic_nested_tools_preserve_denial_and_approval() {
    for (approved, parent_ask) in [(false, false), (true, false), (false, true), (true, true)] {
        let llm = LlmRuntime::new();
        let _provider = llm
            .register(
                "test",
                Arc::new(DeterministicAdapter {
                    streams: Arc::new(Mutex::new(VecDeque::from([
                        run_code_turn("return await tools.read({value: 'nested'});"),
                        text_turn("done"),
                    ]))),
                }),
            )
            .unwrap();
        let tools = ToolRuntime::new();
        if approved {
            tools.set_approval(Some(Arc::new(AlwaysApprove)));
        }
        let _tool = tools
            .register(ToolDefinition::new(
                "read",
                "read",
                json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}},"additionalProperties":false}),
                Echo,
            ))
            .unwrap();
        let registry =
            AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
        let _factory = registry
            .register_factory(Arc::new(
                factory(
                    llm,
                    SystemPrompt::new(),
                    if parent_ask {
                        tools.scoped(ToolRestrictions::new().ask("read")).unwrap()
                    } else {
                        tools
                    },
                )
                .with_code_runtime(ptc_runtime())
                .with_approval_required_tools(if parent_ask {
                    Vec::new()
                } else {
                    vec!["read".into()]
                }),
            ))
            .unwrap();
        let mut selected = header(if approved {
            "nested-approval"
        } else {
            "nested-denial"
        });
        selected.agent_mode = Some(AgentModeId::new("test-ptc").unwrap());
        let agent = registry
            .create(selected, options(), cancellation())
            .await
            .unwrap();
        agent.followup(user("nested")).await.unwrap();
        agent.when_idle().await.unwrap();
        let result = agent
            .session()
            .events()
            .into_iter()
            .find(|event| event.event_type == "tool/result")
            .unwrap();
        assert_eq!(
            result.data["meta"]["codeDispatches"][1]["data"]["isError"],
            json!(!approved)
        );
        agent.dispose().await.unwrap();
    }
}

struct NotifyingBlockingTool(Arc<tokio::sync::Notify>);

#[async_trait]
impl ToolHandler for NotifyingBlockingTool {
    async fn run(&self, context: ToolRunContext, _arguments: Value) -> ToolHandlerResult {
        self.0.notify_one();
        context.cancellation.cancelled().await;
        Ok(ToolOutput::new(Vec::new(), false, Value::Null))
    }
}

#[tokio::test]
async fn programmatic_nested_tool_cancellation_reaches_the_native_dispatcher() {
    let llm = LlmRuntime::new();
    let _provider = llm
        .register(
            "test",
            Arc::new(DeterministicAdapter {
                streams: Arc::new(Mutex::new(VecDeque::from([run_code_turn(
                    "return await tools.bash({value: 'wait'});",
                )]))),
            }),
        )
        .unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let tools = ToolRuntime::new();
    let _tool = tools
        .register(ToolDefinition::new(
            "bash",
            "bash",
            json!({"type":"object","required":["value"],"properties":{"value":{"type":"string"}},"additionalProperties":false}),
            NotifyingBlockingTool(Arc::clone(&started)),
        ))
        .unwrap();
    let registry = AgentRegistry::new(SessionStore::new(Arc::new(MemorySessionPersistence::new())));
    let _factory = registry
        .register_factory(Arc::new(
            factory(llm, SystemPrompt::new(), tools).with_code_runtime(ptc_runtime()),
        ))
        .unwrap();
    let mut selected = header("nested-cancel");
    selected.agent_mode = Some(AgentModeId::new("test-ptc-bash").unwrap());
    let agent = registry
        .create(selected, options(), cancellation())
        .await
        .unwrap();
    let wait_for_start = started.notified();
    agent.followup(user("cancel")).await.unwrap();
    wait_for_start.await;
    assert!(agent.cancel(AgentCancelCause::User, false));
    agent.when_idle().await.unwrap();
    let events = agent.session().events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "tool/result")
            .count(),
        1
    );
    assert_eq!(events.last().unwrap().data["reason"]["kind"], "aborted");
    agent.dispose().await.unwrap();
}

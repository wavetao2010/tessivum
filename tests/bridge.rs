use std::{
    io::Cursor,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};

use tessivum::{
    agent::{
        AgentError, AgentFactory, AgentHandle, AgentOptions, AgentRegistry, AgentRuntime,
        AgentStatus, Inbox,
    },
    bridge::{
        BridgeLimits, BridgeServices, DomainBridge, DomainEventSink, DomainHost, DomainLogger,
        DomainRequest, LogLevel, WasmEffectivePolicy, WasmPolicyRegistry, AGENTS_SERVICE,
        AGENT_MODES_SERVICE, COMMANDS_SERVICE, CREDENTIALS_SERVICE, HOST_EVENTS_SERVICE,
        LLM_SERVICE, LOGGER_SERVICE, MODELS_SERVICE, SESSIONS_SERVICE, SETTINGS_SERVICE,
        SYSTEM_PROMPT_SERVICE, TIMERS_SERVICE, TOOLS_SERVICE,
    },
    credentials::{Credentials, YamlCredentialFile},
    host::{HostApi, HostNotification, HostSessionInfo},
    llm::LlmRuntime,
    plugins::ServiceMethodPermission,
    protocol::{
        InitializeParams, InitializeResult, SessionEvent, SessionHeader, SessionId,
        SessionPromptParams, SessionPromptResult, SessionStatus, SESSION_FORMAT_VERSION,
    },
    session::{MemorySessionPersistence, Session, SessionPersistence, SessionStore},
    settings::{MemorySettingsProvider, Settings},
    subagent::{NativeSubagentProvider, SubagentService},
    system_prompt::{PromptSection, SystemPrompt},
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRunContext, ToolRuntime,
    },
    TessivumError,
};
use tessivum_core::{CancellationToken, ContextHandle};
use tessivum_extism::{Capability, CapabilityHandler, CapabilityRequest, PluginError};
use tessivum_node_bridge::{
    BridgeClient, BridgeError, BridgeHandler, ClientConfig, Frame, FrameKind,
};
use tokio::sync::broadcast;

fn bridge_services() -> (BridgeServices, ToolRuntime, SystemPrompt, SessionStore) {
    let tools = ToolRuntime::new();
    let prompt = SystemPrompt::new();
    let llm = LlmRuntime::new();
    let sessions = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let agents = AgentRegistry::new(sessions.clone());
    (
        BridgeServices::new(tools.clone(), prompt.clone(), llm, sessions.clone(), agents),
        tools,
        prompt,
        sessions,
    )
}

fn disconnected_client(generation: u64) -> BridgeClient {
    BridgeClient::from_io(
        Cursor::new(Vec::<u8>::new()),
        Vec::<u8>::new(),
        generation,
        ClientConfig::default(),
    )
    .unwrap()
}

fn remote_code(error: BridgeError) -> String {
    match error {
        BridgeError::Remote(error) => error.code,
        other => panic!("expected remote error, got {other:?}"),
    }
}

fn wasm_policy(plugin_id: &str, permissions: &[(&str, &str)]) -> WasmEffectivePolicy {
    wasm_policy_for(
        plugin_id,
        &format!("{plugin_id}-instance"),
        &format!("{plugin_id}-entry"),
        permissions,
    )
}

fn wasm_policy_for(
    plugin_id: &str,
    instance_id: &str,
    entry_id: &str,
    permissions: &[(&str, &str)],
) -> WasmEffectivePolicy {
    WasmEffectivePolicy::new(
        plugin_id,
        instance_id,
        entry_id,
        permissions
            .iter()
            .map(|(service, method)| ServiceMethodPermission {
                service: (*service).into(),
                method: (*method).into(),
            }),
    )
}

fn wasm_service_call(plugin_id: &str, payload: Value) -> CapabilityRequest {
    CapabilityRequest {
        capability: Capability::ServiceCall,
        plugin_id: plugin_id.into(),
        instance_id: format!("{plugin_id}-instance"),
        payload,
    }
}

fn plugin_code(error: PluginError) -> String {
    error.code
}

fn rejected<T>(result: Result<T, PluginError>) -> PluginError {
    match result {
        Ok(_) => panic!("expected PluginError"),
        Err(error) => error,
    }
}

fn cancellation() -> CancellationToken {
    ContextHandle::root().scope().cancellation()
}

fn session_header(id: &str) -> SessionHeader {
    SessionHeader {
        version: SESSION_FORMAT_VERSION,
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

fn agent_options() -> AgentOptions {
    AgentOptions {
        provider: "test".into(),
        model: "test".into(),
        reasoning_effort: None,
        max_tokens: Some(1),
    }
}

struct IdleAgent;

#[async_trait]
impl AgentRuntime for IdleAgent {
    fn status(&self) -> AgentStatus {
        AgentStatus::Idle
    }

    async fn wake(&self) -> Result<(), AgentError> {
        Ok(())
    }

    async fn when_idle(&self) -> Result<(), AgentError> {
        Ok(())
    }

    async fn dispose(&self) -> Result<(), AgentError> {
        Ok(())
    }
}

struct IdleAgentFactory;

#[async_trait]
impl AgentFactory for IdleAgentFactory {
    async fn create(
        &self,
        _: Arc<Session>,
        _: AgentOptions,
        _: Inbox,
        _: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        Ok(Arc::new(IdleAgent))
    }
}

struct CountingTool(Arc<AtomicUsize>);

#[async_trait]
impl ToolHandler for CountingTool {
    async fn run(&self, _: ToolRunContext, _: serde_json::Value) -> ToolHandlerResult {
        self.0.fetch_add(1, Ordering::AcqRel);
        Ok(ToolOutput::new(Vec::new(), false, serde_json::Value::Null))
    }
}

async fn owned_bridge() -> (DomainBridge, ToolRuntime, AgentHandle, AgentHandle) {
    let tools = ToolRuntime::new();
    let prompt = SystemPrompt::new();
    let sessions = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let agents = AgentRegistry::new(sessions.clone());
    let registration = agents.register_factory(Arc::new(IdleAgentFactory)).unwrap();
    let owner = agents
        .create(session_header("owner"), agent_options(), cancellation())
        .await
        .unwrap();
    let foreign = agents
        .create(session_header("foreign"), agent_options(), cancellation())
        .await
        .unwrap();
    drop(registration);
    let bridge = DomainBridge::new(
        BridgeServices::new(tools.clone(), prompt, LlmRuntime::new(), sessions, agents)
            .with_owner(owner.authority()),
    )
    .unwrap();
    (bridge, tools, owner, foreign)
}

#[derive(Default)]
struct RecordingLogger(Mutex<Vec<(LogLevel, String)>>);
impl DomainLogger for RecordingLogger {
    fn log(&self, level: LogLevel, message: &str, _: &serde_json::Value) {
        self.0.lock().push((level, message.into()));
    }
}

struct EchoEvents;
impl DomainEventSink for EchoEvents {
    fn emit(
        &self,
        event: &str,
        payload: &serde_json::Value,
    ) -> Result<serde_json::Value, TessivumError> {
        Ok(json!({"event": event, "payload": payload}))
    }
}

struct FacadeHost {
    notices: broadcast::Sender<HostNotification>,
    events: Vec<SessionEvent>,
}

#[async_trait]
impl HostApi for FacadeHost {
    async fn initialize(
        &self,
        _params: InitializeParams,
    ) -> Result<InitializeResult, TessivumError> {
        unreachable!("not used by this test")
    }

    async fn prompt(
        &self,
        _params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        unreachable!("not used by this test")
    }

    async fn cancel(
        &self,
        _session: SessionId,
        _cause: tessivum::agent::AgentCancelCause,
    ) -> Result<bool, TessivumError> {
        Ok(true)
    }

    async fn events(
        &self,
        _session: SessionId,
        _from_seq: u64,
    ) -> Result<Vec<SessionEvent>, TessivumError> {
        Ok(self.events.clone())
    }

    async fn status(&self, _session: SessionId) -> Result<Option<SessionStatus>, TessivumError> {
        Ok(Some(SessionStatus::Idle))
    }

    async fn list_sessions(&self) -> Result<Vec<HostSessionInfo>, TessivumError> {
        Ok(vec![HostSessionInfo {
            session_id: SessionId::from("session-1"),
            workspace_id: None,
            created_at: 1,
            updated_at: 6,
            running: false,
            cwd: Some("/tmp/project".into()),
            parent_session: None,
            origin: None,
            agent_mode: None,
            event_count: self.events.len() as u64,
            blank: false,
        }])
    }

    fn subscribe(&self) -> broadcast::Receiver<HostNotification> {
        self.notices.subscribe()
    }

    async fn shutdown(&self) -> Result<(), TessivumError> {
        Ok(())
    }
}

fn facade_event(seq: u64, event_type: &str) -> SessionEvent {
    SessionEvent {
        event_type: event_type.into(),
        seq,
        time: seq + 1,
        data: json!({}),
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn product_facades_use_the_bound_host_and_remain_node_only() {
    let (notices, _) = broadcast::channel(8);
    let host = Arc::new(FacadeHost {
        notices,
        events: vec![
            facade_event(0, "turn/start"),
            facade_event(1, "user/message"),
            facade_event(2, "turn/end"),
            facade_event(3, "turn/start"),
            facade_event(4, "assistant/message"),
            facade_event(5, "turn/end"),
        ],
    });
    let domain_host = DomainHost::new();
    domain_host.bind(host).unwrap();
    let (services, _, _, _) = bridge_services();
    let bridge = DomainBridge::new(services.with_domain_host(domain_host)).unwrap();
    bridge.attach_client(disconnected_client(21), 21).unwrap();

    let listed = bridge
        .dispatch(
            21,
            DomainRequest {
                service: SESSIONS_SERVICE.into(),
                method: "list".into(),
                params: json!({}),
            },
        )
        .unwrap();
    assert_eq!(listed["items"][0]["sessionId"], "session-1");
    assert_eq!(listed["items"][0]["blank"], false);

    let history = bridge
        .dispatch(
            21,
            DomainRequest {
                service: SESSIONS_SERVICE.into(),
                method: "history".into(),
                params: json!({"sessionId": "session-1", "maxMessages": 1}),
            },
        )
        .unwrap();
    assert_eq!(history["hasMore"], true);
    assert_eq!(history["events"][0]["event"]["seq"], 3);

    for service in [AGENT_MODES_SERVICE, MODELS_SERVICE] {
        assert!(bridge
            .dispatch(
                21,
                DomainRequest {
                    service: service.into(),
                    method: "list".into(),
                    params: json!({}),
                },
            )
            .is_ok());
    }
    assert_eq!(
        bridge
            .dispatch(
                21,
                DomainRequest {
                    service: COMMANDS_SERVICE.into(),
                    method: "list".into(),
                    params: json!({"sessionId": "session-1"}),
                },
            )
            .unwrap(),
        json!({"items": []})
    );
    assert_eq!(
        bridge
            .dispatch(
                21,
                DomainRequest {
                    service: HOST_EVENTS_SERVICE.into(),
                    method: "subscribe".into(),
                    params: json!({
                        "registrationId": "mux-events",
                        "callbackId": "on-event",
                        "stream": "mux",
                    }),
                },
            )
            .unwrap(),
        json!({"registrationId": "mux-events"})
    );
    assert_eq!(
        bridge
            .dispatch(
                21,
                DomainRequest {
                    service: HOST_EVENTS_SERVICE.into(),
                    method: "unsubscribe".into(),
                    params: json!({"registrationId": "mux-events"}),
                },
            )
            .unwrap(),
        json!({"removed": true})
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: SESSIONS_SERVICE.into(),
                    method: "list".into(),
                    params: json!({}),
                })
                .unwrap_err(),
        ),
        "REGISTRATION_DENIED"
    );
}

#[test]
fn construction_outside_tokio_is_rejected() {
    let (services, _, _, _) = bridge_services();
    assert!(matches!(
        DomainBridge::new(services),
        Err(BridgeError::InvalidFrame(message)) if message.contains("Tokio runtime")
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn bridge_drop_is_async_context_safe() {
    let (services, _, _, _) = bridge_services();
    drop(DomainBridge::new(services).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_allowlists_and_bounded_envelopes() {
    let (services, _, _, _) = bridge_services();
    let logger = Arc::new(RecordingLogger::default());
    let bridge = DomainBridge::with_limits(
        services
            .with_logger(logger.clone())
            .with_event_sink(Arc::new(EchoEvents), ["notice"]),
        BridgeLimits {
            max_json_bytes: 128,
            max_callback_bytes: 64,
            max_callback_concurrency: 1,
            max_timers_per_generation: 1,
            request_timeout: std::time::Duration::from_millis(10),
            callback_timeout: std::time::Duration::from_millis(10),
        },
    )
    .unwrap();
    bridge.attach_client(disconnected_client(1), 1).unwrap();

    assert_eq!(
        bridge
            .dispatch_native(DomainRequest {
                service: TOOLS_SERVICE.into(),
                method: "schemas".into(),
                params: json!({}),
            })
            .unwrap(),
        json!({"tools": []})
    );
    bridge
        .dispatch_native(DomainRequest {
            service: LOGGER_SERVICE.into(),
            method: "log".into(),
            params: json!({"level": "info", "message": "hello"}),
        })
        .unwrap();
    assert_eq!(
        logger.0.lock().as_slice(),
        &[(LogLevel::Info, "hello".into())]
    );

    assert_eq!(
        BridgeHandler::handle(
            &bridge,
            Frame::request(
                1,
                1,
                FrameKind::EventEmit,
                json!({"event": "notice", "payload": {"ok": true}}),
            )
        )
        .unwrap(),
        json!({"event": "notice", "payload": {"ok": true}})
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: LOGGER_SERVICE.into(),
                    method: "setLevel".into(),
                    params: json!({}),
                })
                .unwrap_err()
        ),
        "UNKNOWN_METHOD"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: "context@1".into(),
                    method: "get".into(),
                    params: json!({}),
                })
                .unwrap_err()
        ),
        "UNKNOWN_SERVICE"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: TOOLS_SERVICE.into(),
                    method: "schemas".into(),
                    params: json!({"unexpected": true}),
                })
                .unwrap_err()
        ),
        "INVALID_SCHEMA"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: TOOLS_SERVICE.into(),
                    method: "schemas".into(),
                    params: json!({"oversized": "x".repeat(128)}),
                })
                .unwrap_err()
        ),
        "PAYLOAD_TOO_LARGE"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn node_contributions_are_typed_and_generation_owned() {
    let (services, tools, prompt, _) = bridge_services();
    let bridge = DomainBridge::new(services).unwrap();
    bridge.attach_client(disconnected_client(7), 7).unwrap();

    bridge.dispatch(7, DomainRequest {
        service: SYSTEM_PROMPT_SERVICE.into(),
        method: "register".into(),
        params: json!({"registrationId": "prompt", "id": "legacy", "order": 0, "text": "from Node"}),
    }).unwrap();
    bridge
        .dispatch(
            7,
            DomainRequest {
                service: TOOLS_SERVICE.into(),
                method: "register".into(),
                params: json!({
                    "registrationId": "tool",
                    "callbackId": "run-tool",
                    "name": "legacy-tool",
                    "description": "legacy callback",
                    "parameters": {"type": "object", "properties": {}}
                }),
            },
        )
        .unwrap();

    assert_eq!(tools.schemas()[0].name, "legacy-tool");
    assert_eq!(
        prompt
            .assemble(Vec::<PromptSection>::new(), Vec::new())
            .unwrap()
            .text,
        "from Node"
    );

    let removed = BridgeHandler::handle(
        &bridge,
        Frame::request(
            7,
            1,
            FrameKind::RegistrationDispose,
            json!({"registrationId": "tool"}),
        ),
    )
    .unwrap();
    assert_eq!(removed, json!({"removed": true}));
    assert!(tools.schemas().is_empty());

    bridge.cleanup_generation(7);
    assert!(prompt
        .assemble(Vec::<PromptSection>::new(), Vec::new())
        .unwrap()
        .text
        .is_empty());
    assert_eq!(
        remote_code(bridge.attach_client(disconnected_client(7), 7).unwrap_err()),
        "DUPLICATE_GENERATION"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch(
                    7,
                    DomainRequest {
                        service: TOOLS_SERVICE.into(),
                        method: "schemas".into(),
                        params: json!({}),
                    }
                )
                .unwrap_err()
        ),
        "STALE_GENERATION"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_read_only_proxies_and_timer_lifetime() {
    let (services, _, _, _) = bridge_services();
    let settings = Arc::new(Settings::new(Arc::new(MemorySettingsProvider::new())));
    let credentials = Arc::new(Credentials::new(Arc::new(YamlCredentialFile::read_only(
        "/tmp/tessivum-bridge-test-credentials.yaml",
    ))));
    let services = services
        .with_settings(settings)
        .with_credentials(credentials);
    let bridge = DomainBridge::with_limits(
        services,
        BridgeLimits {
            max_timers_per_generation: 1,
            ..BridgeLimits::default()
        },
    )
    .unwrap();
    bridge.attach_client(disconnected_client(9), 9).unwrap();

    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: SETTINGS_SERVICE.into(),
                    method: "update".into(),
                    params: json!({"namespace": "plugin"}),
                })
                .unwrap_err()
        ),
        "UNKNOWN_METHOD"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: CREDENTIALS_SERVICE.into(),
                    method: "resolve".into(),
                    params: json!({"reference": "TOKEN"}),
                })
                .unwrap_err()
        ),
        "UNKNOWN_METHOD"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: SETTINGS_SERVICE.into(),
                    method: "get".into(),
                    params: json!({"namespace": "plugin"}),
                })
                .unwrap_err()
        ),
        "SETTINGS_NOT_REGISTERED"
    );
    assert_eq!(
        bridge
            .dispatch_native(DomainRequest {
                service: CREDENTIALS_SERVICE.into(),
                method: "describe".into(),
                params: json!({"reference": "TESSIVUM_BRIDGE_TEST_MISSING"}),
            })
            .unwrap(),
        json!({
            "reference": "TESSIVUM_BRIDGE_TEST_MISSING",
            "configured": false,
            "writable": false,
        })
    );

    assert_eq!(
        bridge.dispatch(9, DomainRequest {
            service: TIMERS_SERVICE.into(),
            method: "schedule".into(),
            params: json!({"registrationId": "timer", "callbackId": "tick", "delayMs": 60_000}),
        }).unwrap(),
        json!({"timerId": "timer"})
    );
    assert_eq!(
        remote_code(bridge.dispatch(9, DomainRequest {
            service: TIMERS_SERVICE.into(),
            method: "schedule".into(),
            params: json!({"registrationId": "second", "callbackId": "tick", "delayMs": 60_000}),
        }).unwrap_err()),
        "CONCURRENCY_LIMIT"
    );
    assert_eq!(
        bridge
            .dispatch(
                9,
                DomainRequest {
                    service: TIMERS_SERVICE.into(),
                    method: "cancel".into(),
                    params: json!({"registrationId": "timer"}),
                }
            )
            .unwrap(),
        json!({"removed": true})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn session_bound_services_reject_cross_session_access() {
    let (bridge, tools, owner, foreign) = owned_bridge().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let _tool = tools
        .register(ToolDefinition::new(
            "count",
            "counts executions",
            json!({"type": "object", "properties": {}}),
            CountingTool(Arc::clone(&calls)),
        ))
        .unwrap();
    let foreign_id = foreign.id();

    assert_eq!(
        remote_code(bridge.dispatch_native(DomainRequest {
            service: TOOLS_SERVICE.into(),
            method: "execute".into(),
            params: json!({"session": foreign_id, "call": "foreign-call", "name": "count", "arguments": {}}),
        }).unwrap_err()),
        "OWNER_DENIED"
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: SESSIONS_SERVICE.into(),
                    method: "read".into(),
                    params: json!({"session": foreign_id}),
                })
                .unwrap_err()
        ),
        "OWNER_DENIED"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: SESSIONS_SERVICE.into(),
                    method: "append".into(),
                    params: json!({
                        "session": foreign_id,
                        "event": {"type": "turn/start", "seq": 0, "time": 0, "data": null},
                    }),
                })
                .unwrap_err()
        ),
        "OWNER_DENIED"
    );
    assert_eq!(foreign.session().events(), Vec::new());
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: AGENTS_SERVICE.into(),
                    method: "get".into(),
                    params: json!({"session": foreign_id}),
                })
                .unwrap_err()
        ),
        "OWNER_DENIED"
    );

    assert!(bridge.dispatch_native(DomainRequest {
        service: TOOLS_SERVICE.into(),
        method: "execute".into(),
        params: json!({"session": owner.id(), "call": "owner-call", "name": "count", "arguments": {}}),
    }).is_ok());
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(
        bridge
            .dispatch_native(DomainRequest {
                service: SESSIONS_SERVICE.into(),
                method: "append".into(),
                params: json!({
                    "session": owner.id(),
                    "event": {"type": "turn/start", "seq": 0, "time": 0, "data": null},
                }),
            })
            .unwrap(),
        json!({"appended": true})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_compat_agents_preserve_seed_cleanup_generation_and_cold_resume() {
    let persistence: Arc<dyn SessionPersistence> = Arc::new(MemorySessionPersistence::new());
    let sessions = SessionStore::new(Arc::clone(&persistence));
    let agents = AgentRegistry::new(sessions.clone());
    let _factory = agents.register_factory(Arc::new(IdleAgentFactory)).unwrap();
    let parent = agents
        .create(session_header("parent"), agent_options(), cancellation())
        .await
        .unwrap();
    let subagents =
        SubagentService::new(agents.clone(), sessions.clone(), Arc::clone(&persistence));
    let _provider = subagents
        .register(
            "native",
            Arc::new(NativeSubagentProvider::new(agents.clone(), [])),
        )
        .unwrap();
    let bridge = DomainBridge::new(
        BridgeServices::new(
            ToolRuntime::new(),
            SystemPrompt::new(),
            LlmRuntime::new(),
            sessions,
            agents.clone(),
        )
        .with_subagents(subagents),
    )
    .unwrap();
    bridge.attach_client(disconnected_client(13), 13).unwrap();
    let seed = vec![
        SessionEvent {
            event_type: "request/header".into(),
            seq: 0,
            time: 1,
            data: json!({"provider": "mock"}),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
        },
        SessionEvent {
            event_type: "subagent/descriptor".into(),
            seq: 1,
            time: 2,
            data: json!({"label": "Pinned side chat"}),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
        },
    ];
    let request = |method: &str, registration_id: &str| {
        let mut params = json!({
            "registrationId": registration_id,
            "parentSession": parent.id(),
            "childSession": "side-chat",
            "options": agent_options(),
            "createdAt": 1,
        });
        if method == "createCompat" {
            params["label"] = json!("Pinned side chat");
            params["seed"] = serde_json::to_value(&seed).unwrap();
        }
        DomainRequest {
            service: AGENTS_SERVICE.into(),
            method: method.into(),
            params,
        }
    };

    assert_eq!(
        bridge
            .dispatch(13, request("createCompat", "side-chat:create"))
            .unwrap()["live"],
        true
    );
    let child_id = SessionId::from("side-chat");
    assert_eq!(agents.get(&child_id).unwrap().session().events(), seed);

    bridge.cleanup_generation(13);
    tokio::time::timeout(Duration::from_secs(1), async {
        while agents.get(&child_id).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let restarted_sessions = SessionStore::new(Arc::clone(&persistence));
    let restarted_agents = AgentRegistry::new(restarted_sessions.clone());
    let _restarted_factory = restarted_agents
        .register_factory(Arc::new(IdleAgentFactory))
        .unwrap();
    let _restarted_parent = restarted_agents
        .resume(parent.id(), agent_options(), cancellation())
        .await
        .unwrap();
    let restarted_subagents = SubagentService::new(
        restarted_agents.clone(),
        restarted_sessions.clone(),
        Arc::clone(&persistence),
    );
    let _restarted_provider = restarted_subagents
        .register(
            "native",
            Arc::new(NativeSubagentProvider::new(restarted_agents.clone(), [])),
        )
        .unwrap();
    let restarted_bridge = DomainBridge::new(
        BridgeServices::new(
            ToolRuntime::new(),
            SystemPrompt::new(),
            LlmRuntime::new(),
            restarted_sessions.clone(),
            restarted_agents.clone(),
        )
        .with_subagents(restarted_subagents),
    )
    .unwrap();
    restarted_bridge
        .attach_client(disconnected_client(14), 14)
        .unwrap();

    assert!(restarted_sessions.get(&child_id).is_none());
    assert_eq!(
        restarted_bridge
            .dispatch(
                14,
                DomainRequest {
                    service: SESSIONS_SERVICE.into(),
                    method: "snapshot".into(),
                    params: json!({"session": child_id}),
                },
            )
            .unwrap()["session"]["events"],
        serde_json::to_value(&seed).unwrap()
    );
    assert!(restarted_sessions.get(&child_id).is_none());
    assert_eq!(
        restarted_bridge
            .dispatch(14, request("resumeCompat", "side-chat:resume"))
            .unwrap()["live"],
        true
    );
    assert_eq!(
        restarted_bridge
            .dispatch(
                14,
                DomainRequest {
                    service: AGENTS_SERVICE.into(),
                    method: "disposeCompat".into(),
                    params: json!({"registrationId": "side-chat:resume", "session": child_id}),
                },
            )
            .unwrap(),
        json!({"disposed": true})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_authority_rejects_a_replacement_agent_generation() {
    let tools = ToolRuntime::new();
    let prompt = SystemPrompt::new();
    let sessions = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let agents = AgentRegistry::new(sessions.clone());
    let _factory = agents.register_factory(Arc::new(IdleAgentFactory)).unwrap();
    let first = agents
        .create(
            session_header("same-session"),
            agent_options(),
            cancellation(),
        )
        .await
        .unwrap();
    let stale = first.authority();
    first.dispose().await.unwrap();
    let replacement = agents
        .create_or_resume(
            session_header("same-session"),
            agent_options(),
            cancellation(),
        )
        .await
        .unwrap();
    assert!(replacement.authority().is_live());

    let calls = Arc::new(AtomicUsize::new(0));
    let _tool = tools
        .register(ToolDefinition::new(
            "count",
            "counts executions",
            json!({"type": "object", "properties": {}}),
            CountingTool(Arc::clone(&calls)),
        ))
        .unwrap();
    let bridge = DomainBridge::new(
        BridgeServices::new(tools, prompt, LlmRuntime::new(), sessions, agents).with_owner(stale),
    )
    .unwrap();

    assert_eq!(
        remote_code(bridge.dispatch_native(DomainRequest {
            service: TOOLS_SERVICE.into(),
            method: "execute".into(),
            params: json!({"session": "same-session", "call": "stale-call", "name": "count", "arguments": {}}),
        }).unwrap_err()),
        "OWNER_STALE"
    );
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: SESSIONS_SERVICE.into(),
                    method: "read".into(),
                    params: json!({"session": "same-session"}),
                })
                .unwrap_err()
        ),
        "OWNER_STALE"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: SESSIONS_SERVICE.into(),
                    method: "append".into(),
                    params: json!({
                        "session": "same-session",
                        "event": {"type": "turn/start", "seq": 0, "time": 0, "data": null},
                    }),
                })
                .unwrap_err()
        ),
        "OWNER_STALE"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: AGENTS_SERVICE.into(),
                    method: "get".into(),
                    params: json!({"session": "same-session"}),
                })
                .unwrap_err()
        ),
        "OWNER_STALE"
    );
}

async fn schedule_timer_until_admitted(
    bridge: &DomainBridge,
    generation: u64,
    registration_id: &str,
    delay_ms: u64,
) {
    for _ in 0..100 {
        match bridge.dispatch(
            generation,
            DomainRequest {
                service: TIMERS_SERVICE.into(),
                method: "schedule".into(),
                params: json!({
                    "registrationId": registration_id,
                    "callbackId": "tick",
                    "delayMs": delay_ms,
                }),
            },
        ) {
            Ok(value) => {
                assert_eq!(value, json!({"timerId": registration_id}));
                return;
            }
            Err(error) => {
                let code = remote_code(error);
                assert!(matches!(
                    code.as_str(),
                    "CONCURRENCY_LIMIT" | "DUPLICATE_REGISTRATION"
                ));
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    }
    panic!("timer never released its registration or capacity");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_hundred_zero_delay_timers_release_registration_and_capacity() {
    let (services, _, _, _) = bridge_services();
    let bridge = DomainBridge::with_limits(
        services,
        BridgeLimits {
            max_callback_concurrency: 1,
            max_timers_per_generation: 1,
            callback_timeout: Duration::from_millis(1),
            ..BridgeLimits::default()
        },
    )
    .unwrap();
    bridge.attach_client(disconnected_client(10), 10).unwrap();

    for _ in 0..100 {
        schedule_timer_until_admitted(&bridge, 10, "zero", 0).await;
    }
    schedule_timer_until_admitted(&bridge, 10, "drain", 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn timer_registration_reuse_and_cleanup_race_are_safe() {
    let (services, _, _, _) = bridge_services();
    let bridge = DomainBridge::with_limits(
        services,
        BridgeLimits {
            max_timers_per_generation: 1,
            callback_timeout: Duration::from_millis(1),
            ..BridgeLimits::default()
        },
    )
    .unwrap();
    bridge.attach_client(disconnected_client(11), 11).unwrap();

    schedule_timer_until_admitted(&bridge, 11, "reused", 0).await;
    schedule_timer_until_admitted(&bridge, 11, "reused", 60_000).await;
    assert_eq!(
        bridge
            .dispatch(
                11,
                DomainRequest {
                    service: TIMERS_SERVICE.into(),
                    method: "cancel".into(),
                    params: json!({"registrationId": "reused"}),
                }
            )
            .unwrap(),
        json!({"removed": true})
    );

    assert!(bridge.dispatch(11, DomainRequest {
        service: TIMERS_SERVICE.into(),
        method: "schedule".into(),
        params: json!({"registrationId": "cleanup-race", "callbackId": "tick", "delayMs": 0}),
    }).is_ok());
    bridge.cleanup_generation(11);
    tokio::task::yield_now().await;
    assert_eq!(
        remote_code(
            bridge
                .dispatch(
                    11,
                    DomainRequest {
                        service: TIMERS_SERVICE.into(),
                        method: "cancel".into(),
                        params: json!({"registrationId": "cleanup-race"}),
                    }
                )
                .unwrap_err()
        ),
        "STALE_GENERATION"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn remaining_service_proxies_reject_invalid_ownership_or_routes() {
    let (services, _, _, _) = bridge_services();
    let bridge = DomainBridge::new(services).unwrap();

    let missing_provider = bridge
        .dispatch_native(DomainRequest {
            service: LLM_SERVICE.into(),
            method: "generate".into(),
            params: json!({"request": {"provider": "missing", "model": "m", "messages": []}}),
        })
        .unwrap();
    assert_eq!(missing_provider["finishReason"]["kind"], "error");
    assert_eq!(
        missing_provider["finishReason"]["failure"]["code"],
        "LLM_PROVIDER_NOT_FOUND"
    );
    assert_eq!(
        remote_code(bridge.dispatch_native(DomainRequest {
            service: TOOLS_SERVICE.into(),
            method: "execute".into(),
            params: json!({"session": "missing", "call": "missing", "name": "missing", "arguments": {}}),
        }).unwrap_err()),
        "OWNER_REQUIRED"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: SESSIONS_SERVICE.into(),
                    method: "read".into(),
                    params: json!({"session": "missing"}),
                })
                .unwrap_err()
        ),
        "OWNER_REQUIRED"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: SESSIONS_SERVICE.into(),
                    method: "append".into(),
                    params: json!({
                        "session": "missing",
                        "event": {"type": "turn/start", "seq": 0, "time": 0, "data": null},
                    }),
                })
                .unwrap_err()
        ),
        "OWNER_REQUIRED"
    );
    assert_eq!(
        remote_code(
            bridge
                .dispatch_native(DomainRequest {
                    service: AGENTS_SERVICE.into(),
                    method: "get".into(),
                    params: json!({"session": "missing"}),
                })
                .unwrap_err()
        ),
        "OWNER_REQUIRED"
    );
}

#[test]
fn wasm_policy_registry_revokes_and_drains_admitted_calls() {
    let registry = WasmPolicyRegistry::new();
    assert_eq!(
        plugin_code(rejected(registry.install(wasm_policy(" ", &[])))),
        "MANIFEST_PERMISSION_INVALID"
    );

    let registration = registry
        .install(wasm_policy("plugin-a", &[(LOGGER_SERVICE, "log")]))
        .unwrap();
    assert_eq!(
        plugin_code(rejected(registry.install(wasm_policy("plugin-a", &[])))),
        "PLUGIN_POLICY_ALREADY_REGISTERED"
    );
    let lease = registry
        .authorize("plugin-a-instance", "plugin-a", LOGGER_SERVICE, "log")
        .unwrap();
    assert_eq!(
        plugin_code(rejected(registry.authorize(
            "plugin-a-instance",
            "plugin-a",
            LOGGER_SERVICE,
            "other",
        ))),
        "SERVICE_PERMISSION_DENIED"
    );
    assert_eq!(
        plugin_code(rejected(registry.authorize(
            "plugin-a-instance",
            "plugin-a",
            TOOLS_SERVICE,
            "schemas",
        ))),
        "SERVICE_PERMISSION_DENIED"
    );
    assert_eq!(
        plugin_code(rejected(registration.drain(Duration::ZERO))),
        "RESOURCE_LIMIT"
    );
    let registry_for_check = registry.clone();
    let (revoked, observed) = std::sync::mpsc::sync_channel(0);
    let shutdown = std::thread::spawn(move || {
        assert!(registration.revoke());
        revoked.send(()).unwrap();
        registration.drain(Duration::from_secs(1))
    });
    observed.recv().unwrap();
    assert_eq!(
        plugin_code(rejected(registry_for_check.authorize(
            "plugin-a-instance",
            "plugin-a",
            LOGGER_SERVICE,
            "log",
        ))),
        "PLUGIN_POLICY_NOT_FOUND"
    );
    drop(lease);
    shutdown.join().unwrap().unwrap();
}

#[test]
fn wasm_policy_allows_same_entry_candidate_and_rejects_duplicate_owner() {
    let registry = WasmPolicyRegistry::new();
    let committed = registry
        .install(wasm_policy_for("plugin", "old", "entry-a", &[]))
        .unwrap();
    let candidate = registry
        .install(wasm_policy_for("plugin", "new", "entry-a", &[]))
        .unwrap();
    assert_eq!(registry.active_instances("plugin"), vec!["new", "old"]);
    assert_eq!(
        plugin_code(rejected(registry.install(wasm_policy_for(
            "plugin",
            "duplicate",
            "entry-b",
            &[],
        )))),
        "PLUGIN_POLICY_ALREADY_REGISTERED"
    );

    drop(candidate);
    drop(committed);
    let replacement = registry
        .install(wasm_policy_for("plugin", "replacement", "entry-b", &[]))
        .unwrap();
    assert_eq!(replacement.instance_id(), "replacement");
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_service_calls_require_exact_live_policy_and_hide_payloads() {
    let (services, _, _, _) = bridge_services();
    let logger = Arc::new(RecordingLogger::default());
    let registry = WasmPolicyRegistry::new();
    let registration = registry
        .install(wasm_policy("plugin-a", &[(LOGGER_SERVICE, "log")]))
        .unwrap();
    let _other = registry
        .install(wasm_policy("plugin-b", &[(TOOLS_SERVICE, "schemas")]))
        .unwrap();
    let _system_prompt = registry
        .install(wasm_policy(
            "plugin-prompt",
            &[(SYSTEM_PROMPT_SERVICE, "assemble")],
        ))
        .unwrap();
    let _unknown_method = registry
        .install(wasm_policy(
            "plugin-unknown",
            &[(SYSTEM_PROMPT_SERVICE, "unknown")],
        ))
        .unwrap();
    let limits = BridgeLimits {
        max_json_bytes: 128,
        ..BridgeLimits::default()
    };
    let bridge = DomainBridge::with_limits_and_policy_registry(
        services.with_logger(logger.clone()),
        limits,
        registry,
    )
    .unwrap();

    assert_eq!(
        CapabilityHandler::call(
            &bridge,
            wasm_service_call(
                "plugin-a",
                json!({
                    "service": LOGGER_SERVICE,
                    "method": "log",
                    "payload": {"level": "info", "message": "allowed"},
                }),
            ),
        )
        .unwrap(),
        json!({"logged": true})
    );
    assert_eq!(logger.0.lock().len(), 1);

    assert_eq!(
        CapabilityHandler::call(
            &bridge,
            wasm_service_call(
                "plugin-prompt",
                json!({
                    "service": SYSTEM_PROMPT_SERVICE,
                    "method": "assemble",
                    "payload": {"sections": [{"id": "p", "order": 0, "text": "x"}]},
                }),
            ),
        )
        .unwrap(),
        json!({"text": "x", "tools": []})
    );
    assert_eq!(
        plugin_code(rejected(CapabilityHandler::call(
            &bridge,
            wasm_service_call(
                "plugin-prompt",
                json!({"service": SYSTEM_PROMPT_SERVICE, "method": "register", "payload": {}}),
            ),
        ))),
        "SERVICE_PERMISSION_DENIED"
    );
    assert_eq!(
        plugin_code(rejected(CapabilityHandler::call(
            &bridge,
            wasm_service_call(
                "plugin-unknown",
                json!({"service": SYSTEM_PROMPT_SERVICE, "method": "unknown", "payload": {}}),
            ),
        ))),
        "UNKNOWN_METHOD"
    );

    assert_eq!(
        plugin_code(rejected(CapabilityHandler::call(
            &bridge,
            wasm_service_call(
                "plugin-a",
                json!({"service": LOGGER_SERVICE, "method": "other", "payload": {}}),
            ),
        ))),
        "SERVICE_PERMISSION_DENIED"
    );
    assert_eq!(
        plugin_code(rejected(CapabilityHandler::call(
            &bridge,
            wasm_service_call(
                "plugin-b",
                json!({"service": LOGGER_SERVICE, "method": "log", "payload": {}}),
            ),
        ))),
        "SERVICE_PERMISSION_DENIED"
    );
    assert_eq!(
        plugin_code(rejected(CapabilityHandler::call(
            &bridge,
            wasm_service_call(
                "missing",
                json!({"service": LOGGER_SERVICE, "method": "log", "payload": {}}),
            ),
        ))),
        "PLUGIN_POLICY_NOT_FOUND"
    );

    let spoofed = rejected(CapabilityHandler::call(
        &bridge,
        wasm_service_call(
            "plugin-a",
            json!({
                "service": LOGGER_SERVICE,
                "method": "log",
                "payload": {"level": "info", "message": "payload-secret"},
                "pluginId": "plugin-b",
            }),
        ),
    ));
    assert_eq!(spoofed.code, "INVALID_SCHEMA");
    assert!(spoofed.details.is_none());
    assert!(!spoofed.message.contains("payload-secret"));

    let oversized = rejected(CapabilityHandler::call(
        &bridge,
        wasm_service_call(
            "plugin-a",
            json!({
                "service": LOGGER_SERVICE,
                "method": "log",
                "payload": {"level": "info", "message": "payload-secret".repeat(32)},
            }),
        ),
    ));
    assert_eq!(oversized.code, "PAYLOAD_TOO_LARGE");
    assert!(oversized.details.is_none());
    assert!(!oversized.message.contains("payload-secret"));
    assert_eq!(logger.0.lock().len(), 1);

    registration.revoke();
    assert_eq!(
        plugin_code(rejected(CapabilityHandler::call(
            &bridge,
            wasm_service_call(
                "plugin-a",
                json!({"service": LOGGER_SERVICE, "method": "log", "payload": {}}),
            ),
        ))),
        "PLUGIN_POLICY_NOT_FOUND"
    );
}

use std::{path::PathBuf, sync::Arc};

use tessivum_core::{ContextHandle, CoreError, ServiceHandle};
use thiserror::Error;

use crate::{
    agent::{AgentError, AgentFactoryRegistration, AgentHandle, AgentOptions, AgentRegistry},
    agent_loop::AgentLoopFactory,
    agent_mode::{AgentModeId, AgentModeRegistry},
    builtin_tools::{BuiltinTools, BuiltinToolsConfig},
    llm::{LlmAdapter, LlmProviderRegistration, LlmRuntime, RecordedLlmAdapter},
    persistence_jsonl::JsonlSessionPersistence,
    protocol::{
        ContentBlock, Message, MessageId, MessageRole, MessageSource, SessionEvent, SessionHeader,
        SessionId, SESSION_FORMAT_VERSION,
    },
    session::{session_service_key, RestoreMode, SessionError, SessionStore},
    system_prompt::{PromptRegistration, PromptSection, SystemPrompt},
    tools::ToolRuntime,
    TessivumError,
};

/// All caller-owned inputs for one isolated headless session run.
#[derive(Clone, Debug)]
pub struct HeadlessConfig {
    pub data_dir: PathBuf,
    pub cwd: PathBuf,
    pub session_id: SessionId,
    pub agent_mode: AgentModeId,
    pub resume: bool,
    pub provider: String,
    pub model: String,
    pub max_tokens: Option<u64>,
    pub replay_jsonl: String,
    pub enable_trusted_bash: bool,
    pub system_prompt: Option<String>,
}

/// The durable output produced by one headless invocation.
#[derive(Clone, Debug)]
pub struct HeadlessResult {
    pub session_id: SessionId,
    pub final_text: String,
    pub events: Vec<SessionEvent>,
}

/// One cleanup failure retained alongside any primary run failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadlessTeardownFailure {
    pub stage: &'static str,
    pub message: String,
}

/// Structured failures at the headless composition boundary.
#[derive(Debug, Error)]
pub enum HeadlessError {
    #[error("headless task must not be blank")]
    InvalidTask,
    #[error("headless {field} route component must not be blank")]
    InvalidRoute { field: &'static str },
    #[error("headless session id must not be blank")]
    InvalidSessionId,
    #[error("max_tokens must be positive when present")]
    InvalidMaxTokens,
    #[error("recorded replay must not be blank")]
    InvalidReplay,
    #[error("cannot canonicalize headless cwd {path}: {source}")]
    CanonicalCwd {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("headless cwd is not a directory: {0}")]
    CwdNotDirectory(PathBuf),
    #[error("cannot create headless data directory {path}: {source}")]
    CreateDataDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Runtime(#[from] TessivumError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error("headless teardown failed: {failures:?}")]
    Teardown {
        failures: Vec<HeadlessTeardownFailure>,
    },
    #[error("{primary}; headless teardown also failed: {teardown:?}")]
    RunAndTeardown {
        #[source]
        primary: Box<HeadlessError>,
        teardown: Vec<HeadlessTeardownFailure>,
    },
}

impl HeadlessError {
    /// Stable code suitable for a process boundary.
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidTask => "INVALID_HEADLESS_TASK",
            Self::InvalidRoute { .. } => "INVALID_HEADLESS_ROUTE",
            Self::InvalidSessionId => "INVALID_SESSION_ID",
            Self::InvalidMaxTokens => "INVALID_MAX_TOKENS",
            Self::InvalidReplay => "INVALID_HEADLESS_REPLAY",
            Self::CanonicalCwd { .. } | Self::CwdNotDirectory(_) => "INVALID_HEADLESS_CWD",
            Self::CreateDataDir { .. } => "HEADLESS_DATA_DIR_CREATE_FAILED",
            Self::Runtime(error) => &error.code,
            Self::Session(error) => error.code(),
            Self::Agent(AgentError::Cancelled) | Self::Core(CoreError::Cancelled) => "CANCELLED",
            Self::Agent(AgentError::Session(error)) => error.code(),
            Self::Agent(AgentError::Message(error)) => &error.code,
            Self::Agent(_) => "HEADLESS_AGENT_ERROR",
            Self::Core(_) => "HEADLESS_CORE_ERROR",
            Self::Teardown { .. } => "HEADLESS_TEARDOWN_FAILED",
            Self::RunAndTeardown { primary, .. } => primary.code(),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.code() == "CANCELLED"
    }
}

/// Runs one durable recorded-model session in a fresh root context.
pub async fn run_headless(
    config: HeadlessConfig,
    task: String,
) -> Result<HeadlessResult, HeadlessError> {
    let cwd = validate(&config, &task, true)?;
    let adapter = Arc::new(RecordedLlmAdapter::from_jsonl_with_route(
        &config.replay_jsonl,
        Some(config.session_id.clone()),
        config.provider.clone(),
        config.model.clone(),
    )?);
    run_headless_validated(config, task, cwd, adapter).await
}

/// Runs one durable live-model session with a caller-owned adapter.
pub async fn run_headless_with_adapter(
    config: HeadlessConfig,
    task: String,
    adapter: Arc<dyn LlmAdapter>,
) -> Result<HeadlessResult, HeadlessError> {
    let cwd = validate(&config, &task, false)?;
    run_headless_validated(config, task, cwd, adapter).await
}

async fn run_headless_validated(
    config: HeadlessConfig,
    task: String,
    cwd: PathBuf,
    adapter: Arc<dyn LlmAdapter>,
) -> Result<HeadlessResult, HeadlessError> {
    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .map_err(|source| HeadlessError::CreateDataDir {
            path: config.data_dir.clone(),
            source,
        })?;
    let mut scope = HeadlessScope::new();
    let mut agent = None;
    let operation = async {
        let sessions = SessionStore::new(Arc::new(JsonlSessionPersistence::new(&config.data_dir)));
        scope.session_service = Some(
            scope
                .root
                .provide(session_service_key(), sessions.clone())?,
        );

        let llm = LlmRuntime::new();
        scope.provider_registration = Some(llm.register(config.provider.clone(), adapter)?);
        scope.llm_service = Some(llm.clone().publish(&scope.root)?);

        let prompt = SystemPrompt::new();
        if let Some(text) = &config.system_prompt {
            scope.prompt_registration =
                Some(prompt.register(PromptSection::new("headless", 0, text.clone()))?);
        }
        scope.prompt_service = Some(prompt.clone().publish(&scope.root)?);

        let tools = ToolRuntime::new();
        scope.builtin_tools = Some(BuiltinTools::new(
            &tools,
            BuiltinToolsConfig {
                enable_bash: config.enable_trusted_bash,
                cwd: cwd.clone(),
                ..BuiltinToolsConfig::default()
            },
        )?);
        scope.tools_service = Some(tools.publish(&scope.root)?);

        let modes = Arc::new(AgentModeRegistry::with_roots(Vec::new(), None));
        let registry = AgentRegistry::new(sessions.clone());
        scope.registry = Some(registry.clone());
        scope.registry_service = Some(registry.clone().publish(&scope.root)?);
        scope.factory_registration = Some(registry.register_factory(Arc::new(
            AgentLoopFactory::new(llm, prompt, tools, modes, config.agent_mode.clone()),
        ))?);

        let options = AgentOptions {
            provider: config.provider.clone(),
            model: config.model.clone(),
            reasoning_effort: None,
            max_tokens: config.max_tokens,
        };
        let cancellation = scope.root.scope().cancellation();
        let header = session_header(&config.session_id, &cwd, config.agent_mode.clone());
        let handle = if config.resume {
            sessions
                .restore(&config.session_id, RestoreMode::Cold, cancellation.clone())
                .await?;
            registry
                .resume(config.session_id.clone(), options, cancellation)
                .await?
        } else {
            registry.create(header, options, cancellation).await?
        };
        let session = handle.session();
        let first_event_seq = session.next_seq()?;
        agent = Some(handle);
        let handle = agent
            .as_ref()
            .expect("headless agent is retained for teardown");
        handle
            .followup(Message {
                id: MessageId::from(config.session_id.as_str()),
                role: MessageRole::User,
                content: vec![ContentBlock::Text { text: task }],
                source: MessageSource::User {
                    client_time_zone: None,
                },
            })
            .await?;
        handle.when_idle().await?;
        session.flush(scope.root.scope().cancellation()).await?;

        let events = session
            .events()
            .into_iter()
            .filter(|event| event.seq >= first_event_seq)
            .collect::<Vec<_>>();
        Ok(HeadlessResult {
            session_id: config.session_id.clone(),
            final_text: final_assistant_text(&events),
            events,
        })
    }
    .await;

    let teardown = scope.dispose(agent.take()).await;
    match (operation, teardown) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(failures)) => Err(HeadlessError::Teardown { failures }),
        (Err(primary), Err(teardown)) => Err(HeadlessError::RunAndTeardown {
            primary: Box::new(primary),
            teardown,
        }),
    }
}

fn validate(
    config: &HeadlessConfig,
    task: &str,
    require_replay: bool,
) -> Result<PathBuf, HeadlessError> {
    if task.trim().is_empty() {
        return Err(HeadlessError::InvalidTask);
    }
    if config.provider.trim().is_empty() {
        return Err(HeadlessError::InvalidRoute { field: "provider" });
    }
    if config.model.trim().is_empty() {
        return Err(HeadlessError::InvalidRoute { field: "model" });
    }
    if config.session_id.as_str().trim().is_empty() {
        return Err(HeadlessError::InvalidSessionId);
    }
    if config.max_tokens == Some(0) {
        return Err(HeadlessError::InvalidMaxTokens);
    }
    if require_replay && config.replay_jsonl.trim().is_empty() {
        return Err(HeadlessError::InvalidReplay);
    }
    let cwd = config
        .cwd
        .canonicalize()
        .map_err(|source| HeadlessError::CanonicalCwd {
            path: config.cwd.clone(),
            source,
        })?;
    if !cwd.is_dir() {
        return Err(HeadlessError::CwdNotDirectory(cwd));
    }
    Ok(cwd)
}

fn session_header(
    session_id: &SessionId,
    cwd: &std::path::Path,
    agent_mode: AgentModeId,
) -> SessionHeader {
    SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: session_id.clone(),
        created_at: 0,
        cwd: Some(cwd.to_string_lossy().into_owned()),
        parent_session: None,
        seed_length: None,
        origin: None,
        delegation_depth: Some(0),
        agent_mode: Some(agent_mode),
    }
}

fn final_assistant_text(events: &[SessionEvent]) -> String {
    events
        .iter()
        .rev()
        .find(|event| event.event_type == "assistant/message")
        .and_then(|event| event.data.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                })
                .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

struct HeadlessScope {
    root: ContextHandle,
    registry: Option<AgentRegistry>,
    session_service: Option<ServiceHandle<SessionStore>>,
    llm_service: Option<ServiceHandle<LlmRuntime>>,
    prompt_service: Option<ServiceHandle<SystemPrompt>>,
    tools_service: Option<ServiceHandle<ToolRuntime>>,
    registry_service: Option<ServiceHandle<AgentRegistry>>,
    provider_registration: Option<LlmProviderRegistration>,
    prompt_registration: Option<PromptRegistration>,
    builtin_tools: Option<BuiltinTools>,
    factory_registration: Option<AgentFactoryRegistration>,
}

impl HeadlessScope {
    fn new() -> Self {
        Self {
            root: ContextHandle::root(),
            registry: None,
            session_service: None,
            llm_service: None,
            prompt_service: None,
            tools_service: None,
            registry_service: None,
            provider_registration: None,
            prompt_registration: None,
            builtin_tools: None,
            factory_registration: None,
        }
    }

    async fn dispose(
        &mut self,
        agent: Option<AgentHandle>,
    ) -> Result<(), Vec<HeadlessTeardownFailure>> {
        let mut failures = Vec::new();
        if let Some(agent) = agent {
            if let Err(error) = agent.dispose().await {
                failures.push(teardown_failure("agent", error));
            }
        }
        if let Some(registry) = &self.registry {
            if let Err(error) = registry.dispose_all().await {
                failures.push(teardown_failure("registry", error));
            }
        }

        self.factory_registration.take();
        self.builtin_tools.take();
        self.prompt_registration.take();
        self.provider_registration.take();

        if let Err(error) = self.root.scope().dispose().await {
            failures.push(teardown_failure("root-context", error));
        }

        self.registry_service.take();
        self.tools_service.take();
        self.prompt_service.take();
        self.llm_service.take();
        self.session_service.take();
        self.registry.take();

        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures)
        }
    }
}

fn teardown_failure(
    error_stage: &'static str,
    error: impl std::fmt::Display,
) -> HeadlessTeardownFailure {
    HeadlessTeardownFailure {
        stage: error_stage,
        message: error.to_string(),
    }
}

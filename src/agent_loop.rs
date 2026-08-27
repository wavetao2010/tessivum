//! Durable, wake-coalesced agent turn driver.

use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle};
use tokio::{sync::Notify, task::JoinHandle};

use crate::{
    agent::{
        AgentCancelCause, AgentError, AgentFactory, AgentOptions, AgentRuntime, AgentStatus, Inbox,
        InboxClaimReservation,
    },
    agent_mode::{AgentModeId, AgentModeRegistry, ToolCapabilityId, ToolPresentation},
    builtin_tools::PersistentShellSessions,
    code_runtime::{register_code_tool, ProcessCodeRuntime, PTC_RUNTIME_UNAVAILABLE},
    composition::CompositionRegistry,
    compaction::{CompactionOutcome, CompactionService, CompactionTrigger},
    llm::{BlockAssembler, LlmRuntime},
    permissions::runtime_context,
    protocol::{
        ContentBlock, ContextForm, EpochHeader, FinishReason, GenerateRequest, LlmCallConfig,
        LlmFailure, Message, MessageId, MessageRole, MessageSource, SessionEvent, SessionOrigin,
        SurfaceOp, TurnEndCancelCause, TurnEndReason,
    },
    session::Session,
    skills::{model_catalog, skill_result_tag, SkillRuntime, SkillSessionScopes},
    system_prompt::{PromptSection, SystemPrompt},
    tools::{ToolOutput, ToolRegistration, ToolRestrictions, ToolRunContext, ToolRuntime},
    TessivumError,
};

/// Resolves the advertised prompt capacity for an exact provider/model route.
pub type ContextWindowResolver = Arc<dyn Fn(&str, &str) -> Option<u64> + Send + Sync>;
const CHILD_OWNER_BOUND_TOOL_NAMES: &[&str] = &[
    "ask_user_question",
    "bash",
    "create_goal",
    "exit_plan_mode",
    "get_goal",
    "jobs.kill",
    "jobs.list",
    "jobs.read",
    "jobs.wait",
    "schedule_create",
    "schedule_delete",
    "schedule_list",
    "todo_write",
    "update_goal",
    "subagent",
    "subagent_fork",
];

/// Constructs durable agent runtimes backed by the current LLM, prompt, and tool services.
#[derive(Clone)]
pub struct AgentLoopFactory {
    llm: LlmRuntime,
    prompt: SystemPrompt,
    native_tools: ToolRuntime,
    modes: Arc<AgentModeRegistry>,
    default_mode: AgentModeId,
    code_runtime: Option<ProcessCodeRuntime>,
    persistent_shells: Option<PersistentShellSessions>,
    composition: Option<CompositionRegistry>,
    root_context: Option<ContextHandle>,
    approval_required_tools: BTreeSet<String>,
    compaction: Option<CompactionService>,
    skills: Option<(SkillRuntime, SkillSessionScopes)>,
    context_window: Option<ContextWindowResolver>,
    max_parallel_tool_calls: usize,
    max_steps: u64,
}
impl std::fmt::Debug for AgentLoopFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentLoopFactory")
            .finish_non_exhaustive()
    }
}

impl AgentLoopFactory {
    pub const DEFAULT_MAX_PARALLEL_TOOL_CALLS: usize = 4;
    pub const DEFAULT_MAX_STEPS: u64 = 32;

    pub fn new(
        llm: LlmRuntime,
        prompt: SystemPrompt,
        native_tools: ToolRuntime,
        modes: Arc<AgentModeRegistry>,
        default_mode: AgentModeId,
    ) -> Self {
        Self {
            llm,
            prompt,
            native_tools,
            modes,
            default_mode,
            code_runtime: None,
            persistent_shells: None,
            composition: None,
            root_context: None,
            approval_required_tools: BTreeSet::new(),
            compaction: None,
            skills: None,
            context_window: None,
            max_parallel_tool_calls: Self::DEFAULT_MAX_PARALLEL_TOOL_CALLS,
            max_steps: Self::DEFAULT_MAX_STEPS,
        }
    }

    pub fn with_code_runtime(mut self, code_runtime: ProcessCodeRuntime) -> Self {
        self.code_runtime = Some(code_runtime);
        self
    }
    pub fn with_persistent_shell_sessions(mut self, sessions: PersistentShellSessions) -> Self {
        self.persistent_shells = Some(sessions);
        self
    }

    pub fn with_composition_registry(mut self, composition: CompositionRegistry) -> Self {
        self.composition = Some(composition);
        self
    }

    pub fn with_root_context(mut self, root_context: ContextHandle) -> Self {
        self.root_context = Some(root_context);
        self
    }


    pub fn with_approval_required_tools(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.approval_required_tools = names.into_iter().collect();
        self
    }

    pub fn with_compaction(mut self, compaction: CompactionService) -> Self {
        self.compaction = Some(compaction);
        self
    }

    pub fn with_skills(mut self, skills: SkillRuntime, scopes: SkillSessionScopes) -> Self {
        self.skills = Some((skills, scopes));
        self
    }

    pub fn with_context_window_resolver(mut self, resolver: ContextWindowResolver) -> Self {
        self.context_window = Some(resolver);
        self
    }

    pub fn with_max_parallel_tool_calls(mut self, max_parallel_tool_calls: usize) -> Self {
        self.max_parallel_tool_calls = max_parallel_tool_calls.max(1);
        self
    }

    pub fn with_max_steps(mut self, max_steps: u64) -> Self {
        self.max_steps = max_steps.max(1);
        self
    }

    pub fn max_parallel_tool_calls(&self) -> usize {
        self.max_parallel_tool_calls
    }

    pub fn max_steps(&self) -> u64 {
        self.max_steps
    }
    async fn attach_resources(
        &self,
        runtime: &SessionRuntimeSpec,
        session: &Session,
    ) -> Result<SessionResources, AgentError> {
        let persistent_shells = if runtime.persistent_shell {
            Some(self.persistent_shells.clone().ok_or_else(|| {
                resource_error(
                    "PERSISTENT_SHELL_SESSIONS_UNAVAILABLE",
                    "the selected mode requires injected persistent-shell sessions",
                    runtime,
                    session,
                    Vec::new(),
                )
            })?)
        } else {
            None
        };
        let composition = if runtime.composition {
            Some((
                self.composition.clone().ok_or_else(|| {
                    resource_error(
                        "COMPOSITION_REGISTRY_UNAVAILABLE",
                        "the selected mode requires an injected composition registry",
                        runtime,
                        session,
                        Vec::new(),
                    )
                })?,
                self.root_context.clone().ok_or_else(|| {
                    resource_error(
                        "COMPOSITION_CONTEXT_UNAVAILABLE",
                        "the selected mode requires an injected root context",
                        runtime,
                        session,
                        Vec::new(),
                    )
                })?,
            ))
        } else {
            None
        };
        if let Some(shells) = &persistent_shells {
            shells.enable(session.id());
        }
        let composition = if let Some((registry, root)) = composition {
            let context = match root.child() {
                Ok(context) => context,
                Err(error) => {
                    if let Some(shells) = &persistent_shells {
                        shells.disable(&session.id()).await;
                    }
                    return Err(resource_error(
                        "COMPOSITION_SESSION_SCOPE_UNAVAILABLE",
                        "the selected mode could not create its composition session scope",
                        runtime,
                        session,
                        vec![error.to_string()],
                    ));
                }
            };
            if let Err(error) = registry.attach_session(session.id(), context.clone()) {
                let mut cleanup = vec![error.to_string()];
                if let Err(error) = context.scope().dispose().await {
                    cleanup.push(error.to_string());
                }
                if let Some(shells) = &persistent_shells {
                    shells.disable(&session.id()).await;
                }
                return Err(resource_error(
                    "COMPOSITION_SESSION_ATTACH_FAILED",
                    "the selected mode could not attach its composition session scope",
                    runtime,
                    session,
                    cleanup,
                ));
            }
            Some(CompositionSession { registry, context })
        } else {
            None
        };
        Ok(SessionResources {
            persistent_shells,
            composition,
        })
    }
}

#[async_trait]
impl AgentFactory for AgentLoopFactory {
    async fn create(
        &self,
        session: Arc<Session>,
        options: AgentOptions,
        inbox: Inbox,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn AgentRuntime>, AgentError> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let runtime = SessionRuntimeSpec::resolve(self, &session)?;
        let resources = self.attach_resources(&runtime, &session).await?;
        if cancellation.is_cancelled() {
            let failures = resources.dispose(&session.id()).await;
            return if failures.is_empty() {
                Err(AgentError::Cancelled)
            } else {
                Err(resource_error(
                    "AGENT_RESOURCE_DISPOSE_FAILED",
                    "the cancelled agent setup could not release every session resource",
                    &runtime,
                    &session,
                    failures,
                ))
            };
        }
        Ok(AgentLoop::spawn(
            session,
            options,
            inbox,
            cancellation,
            self.llm.clone(),
            self.prompt.clone(),
            runtime,
            resources,
            self.context_window.clone(),
            self.max_parallel_tool_calls,
            self.max_steps,
        ))
    }
}

/// Immutable mode policy and model-facing runtime for one live session.
struct SessionRuntimeSpec {
    mode: AgentModeId,
    persistent_shell: bool,
    composition: bool,
    prompt: ModePrompt,
    tools: ToolRuntime,
    compaction: Option<CompactionService>,
    skills: Option<(SkillRuntime, SkillSessionScopes)>,
    _tool_registrations: Vec<ToolRegistration>,
}

struct SessionResources {
    persistent_shells: Option<PersistentShellSessions>,
    composition: Option<CompositionSession>,
}

struct CompositionSession {
    registry: CompositionRegistry,
    context: ContextHandle,
}
impl SessionResources {
    async fn dispose(&self, owner: &crate::SessionId) -> Vec<String> {
        let mut failures = Vec::new();
        if let Some(shells) = &self.persistent_shells {
            shells.disable(owner).await;
        }
        if let Some(composition) = &self.composition {
            if let Err(error) = composition.registry.dispose_session(owner).await {
                failures.push(error.to_string());
            }
            if let Err(error) = composition.context.scope().dispose().await {
                failures.push(error.to_string());
            }
        }
        failures
    }
}

fn resource_error(
    code: &str,
    message: &str,
    runtime: &SessionRuntimeSpec,
    session: &Session,
    failures: Vec<String>,
) -> AgentError {
    AgentError::Message(TessivumError::new(
        code,
        message,
        "agent-loop",
        json!({
            "agentMode": runtime.mode.as_str(),
            "sessionId": session.id(),
            "failures": failures,
        }),
    ))
}

struct ModePrompt {
    complete: bool,
    section: PromptSection,
}

impl SessionRuntimeSpec {
    fn resolve(factory: &AgentLoopFactory, session: &Session) -> Result<Self, AgentError> {
        let mode_id = selected_mode(session, &factory.default_mode)?;
        let mode = factory
            .modes
            .resolve(mode_id.as_str())
            .map_err(AgentError::Message)?;
        let mode_id = mode.spec.id.clone();
        let mode_prompt = mode.spec.prompt.clone();
        let skills_enabled = mode.spec.skills;
        let compaction_enabled = mode.spec.compaction.is_some();
        let persistent_shell = mode.spec.capabilities.persistent_shell;
        let composition = mode.spec.capabilities.composition;
        let presentation = mode.spec.presentation;
        let skills = factory
            .skills
            .as_ref()
            .filter(|(_, scopes)| skills_enabled && lock(scopes).contains_key(&session.id()));
        let mut names = mode.nested_tools.clone();
        if !mode.spec.planning {
            let planning_tools = [ToolCapabilityId::PlanExit, ToolCapabilityId::PlanTodo]
                .into_iter()
                .flat_map(ToolCapabilityId::native_tools)
                .map(|name| (*name).to_owned())
                .collect::<BTreeSet<_>>();
            names.retain(|name| !planning_tools.contains(name));
        }
        if session.header().origin == Some(SessionOrigin::Subagent) {
            names.retain(|name| !CHILD_OWNER_BOUND_TOOL_NAMES.contains(&name.as_str()));
        }
        if skills.is_none() {
            names.retain(|name| name != "skill");
        }
        let available = factory
            .native_tools
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<BTreeSet<_>>();
        let missing = names
            .iter()
            .filter(|name| !available.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(AgentError::Message(TessivumError::new(
                "MODE_NATIVE_TOOL_UNAVAILABLE",
                "the selected mode requires native tools absent from the Host registry",
                "agent-loop",
                json!({"agentMode": mode_id.as_str(), "missing": missing}),
            )));
        }
        let approval_required = factory
            .approval_required_tools
            .iter()
            .filter(|name| names.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        let restrictions = approval_required
            .into_iter()
            .fold(ToolRestrictions::allow_only(names), ToolRestrictions::ask);
        let native_tools = factory
            .native_tools
            .scoped(restrictions)
            .map_err(AgentError::Message)?;
        let (tools, registrations) = match presentation {
            ToolPresentation::Direct => (native_tools, Vec::new()),
            ToolPresentation::Programmatic => {
                let code_runtime = factory.code_runtime.clone().ok_or_else(|| {
                    AgentError::Message(TessivumError::new(
                        PTC_RUNTIME_UNAVAILABLE,
                        "programmatic mode requires an available PTC runtime",
                        "agent-loop",
                        json!({"agentMode": mode_id.as_str()}),
                    ))
                })?;
                let tools = ToolRuntime::new();
                let registration = register_code_tool(&tools, native_tools, code_runtime)
                    .map_err(AgentError::Message)?;
                (tools, vec![registration])
            }
        };
        Ok(Self {
            mode: mode_id.clone(),
            persistent_shell,
            composition,
            prompt: ModePrompt {
                complete: mode_prompt.complete,
                section: PromptSection::new(format!("agent-mode/{mode_id}"), 0, mode_prompt.text),
            },
            tools,
            compaction: compaction_enabled
                .then(|| factory.compaction.clone())
                .flatten(),
            skills: skills.cloned(),
            _tool_registrations: registrations,
        })
    }
}

fn selected_mode(session: &Session, default_mode: &AgentModeId) -> Result<AgentModeId, AgentError> {
    let selected = session
        .events()
        .into_iter()
        .rev()
        .find(|event| event.event_type == "agent-mode/selected")
        .map(|event| {
            let value = event
                .data
                .get("agentMode")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    AgentError::Message(TessivumError::new(
                        "MODE_SELECTION_INVALID",
                        "agent-mode/selected requires an agentMode string",
                        "agent-loop",
                        event.data,
                    ))
                })?;
            AgentModeId::new(value).map_err(AgentError::Message)
        })
        .transpose()?;
    Ok(selected
        .or_else(|| session.header().agent_mode)
        .unwrap_or_else(|| default_mode.clone()))
}

/// A single-session driver. One persistent worker waits for coalesced wakeups.
pub struct AgentLoop {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AgentLoop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentLoop")
            .field("session", &self.inner.session.id())
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

struct Inner {
    session: Arc<Session>,
    options: AgentOptions,
    inbox: Inbox,
    cancellation: CancellationToken,
    cancellation_cause: Mutex<Option<AgentCancelCause>>,
    finalization: CancellationToken,
    llm: LlmRuntime,
    prompt: SystemPrompt,
    runtime: SessionRuntimeSpec,
    resources: SessionResources,
    context_window: Option<ContextWindowResolver>,
    max_parallel_tool_calls: usize,
    max_steps: u64,
    running: AtomicBool,
    wake: Notify,
    idle: Notify,
    state: Mutex<State>,
}

struct State {
    disposed: bool,
    wake_revision: u64,
    settled_revision: u64,
    worker: Option<JoinHandle<()>>,
    last_error: Option<AgentError>,
    last_request_header: Option<Value>,
    next_request_header_reason: Option<&'static str>,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        session: Arc<Session>,
        options: AgentOptions,
        inbox: Inbox,
        cancellation: CancellationToken,
        llm: LlmRuntime,
        prompt: SystemPrompt,
        runtime: SessionRuntimeSpec,
        resources: SessionResources,
        context_window: Option<ContextWindowResolver>,
        max_parallel_tool_calls: usize,
        max_steps: u64,
    ) -> Arc<Self> {
        let has_prior_request_header = session
            .events()
            .iter()
            .any(|event| event.event_type == "request/header");
        let inner = Arc::new(Inner {
            session,
            options,
            inbox,
            cancellation,
            cancellation_cause: Mutex::new(None),
            finalization: ContextHandle::root().scope().cancellation(),
            llm,
            prompt,
            runtime,
            resources,
            context_window,
            max_parallel_tool_calls,
            max_steps,
            running: AtomicBool::new(false),
            wake: Notify::new(),
            idle: Notify::new(),
            state: Mutex::new(State {
                disposed: false,
                wake_revision: 0,
                settled_revision: 0,
                worker: None,
                last_error: None,
                last_request_header: None,
                next_request_header_reason: Some(if has_prior_request_header {
                    "resume"
                } else {
                    "initial"
                }),
            }),
        });
        let worker = tokio::spawn(drive(Arc::clone(&inner)));
        lock(&inner.state).worker = Some(worker);
        Arc::new(Self { inner })
    }
}

#[async_trait]
impl AgentRuntime for AgentLoop {
    fn status(&self) -> AgentStatus {
        if self.inner.running.load(Ordering::Acquire) {
            AgentStatus::Running
        } else {
            AgentStatus::Idle
        }
    }

    fn cancel(&self, cause: AgentCancelCause) {
        let mut cancellation_cause = lock(&self.inner.cancellation_cause);
        if cancellation_cause.is_none() {
            *cancellation_cause = Some(cause);
        }
    }

    async fn wake(&self) -> Result<(), AgentError> {
        {
            let mut state = lock(&self.inner.state);
            if state.disposed {
                return Err(AgentError::Disposed);
            }
            if self.inner.cancellation.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            state.wake_revision = state.wake_revision.wrapping_add(1);
            state.last_error = None;
        }
        self.inner.wake.notify_one();
        Ok(())
    }

    async fn when_idle(&self) -> Result<(), AgentError> {
        loop {
            let notified = self.inner.idle.notified();
            let result = {
                let state = lock(&self.inner.state);
                (!self.inner.running.load(Ordering::Acquire)
                    && state.wake_revision == state.settled_revision)
                    .then(|| state.last_error.clone())
            };
            if let Some(result) = result {
                return result.map_or(Ok(()), Err);
            }
            notified.await;
        }
    }

    async fn dispose(&self) -> Result<(), AgentError> {
        let worker = {
            let mut state = lock(&self.inner.state);
            if state.disposed {
                return Ok(());
            }
            state.disposed = true;
            state.worker.take()
        };
        self.cancel(AgentCancelCause::Disposed);
        self.inner.cancellation.cancel();
        self.inner.wake.notify_waiters();
        self.inner.idle.notify_waiters();
        let mut failures = Vec::new();
        if let Some(worker) = worker {
            if let Err(error) = worker.await {
                failures.push(format!("agent loop worker failed: {error}"));
            }
        }
        failures.extend(self.inner.resources.dispose(&self.inner.session.id()).await);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(AgentError::Message(TessivumError::new(
                "AGENT_RESOURCE_DISPOSE_FAILED",
                "the agent worker or one or more session resources could not be disposed cleanly",
                "agent-loop",
                json!({"sessionId": self.inner.session.id(), "failures": failures}),
            )))
        }
    }
}

async fn drive(inner: Arc<Inner>) {
    let mut seen_wake = 0;
    loop {
        while !inner.cancellation.is_cancelled() {
            let notified = inner.wake.notified();
            let revision = lock(&inner.state).wake_revision;
            if revision != seen_wake {
                seen_wake = revision;
                break;
            }
            tokio::select! {
                _ = notified => {},
                _ = inner.cancellation.cancelled() => break,
            }
        }
        if inner.cancellation.is_cancelled() || lock(&inner.state).disposed {
            break;
        }

        inner.running.store(true, Ordering::Release);
        loop {
            if inner.cancellation.is_cancelled() {
                break;
            }
            let message = match claim_next_turn(&inner).await {
                Ok(Some(message)) => message,
                Ok(None) => {
                    let revision = lock(&inner.state).wake_revision;
                    if revision != seen_wake {
                        seen_wake = revision;
                        continue;
                    }
                    lock(&inner.state).settled_revision = revision;
                    inner.running.store(false, Ordering::Release);
                    inner.idle.notify_waiters();
                    break;
                }
                Err(error) => {
                    let revision = lock(&inner.state).wake_revision;
                    let mut state = lock(&inner.state);
                    state.last_error = Some(error);
                    state.settled_revision = revision;
                    inner.running.store(false, Ordering::Release);
                    inner.idle.notify_waiters();
                    break;
                }
            };

            if let Err(error) = run_turn(&inner, message).await {
                lock(&inner.state).last_error = Some(error);
            }
        }
    }
    inner.running.store(false, Ordering::Release);
    {
        let mut state = lock(&inner.state);
        state.settled_revision = state.wake_revision;
    }
    inner.idle.notify_waiters();
}

async fn run_turn(inner: &Inner, initial_message: Message) -> Result<(), AgentError> {
    if inner.cancellation.is_cancelled() {
        return Ok(());
    }
    let turn = next_turn(&inner.session);
    append(inner, "turn/start", json!({"turn": turn}), None, None).await?;

    let mut pending_initial = Some(initial_message);
    for step in 1..=inner.max_steps {
        if inner.cancellation.is_cancelled() {
            return end_turn(inner, turn, aborted(inner)).await;
        }
        append(
            inner,
            "step/start",
            json!({"turn": turn, "step": step}),
            None,
            None,
        )
        .await?;

        let mut messages = claim_step_batch(inner).await?;
        if let Some(message) = pending_initial.take() {
            messages.push(message);
        }
        for message in &messages {
            append_message(
                inner,
                "user/message",
                turn,
                step,
                message.clone(),
                None,
                None,
            )
            .await?;
        }
        append_skill_context(inner, turn, step, &messages).await?;
        if step == 1 {
            append_workspace_instructions(inner, turn, step).await?;
            append_runtime_context(inner, turn, step).await?;
        }
        if inner.cancellation.is_cancelled() {
            return close_cancelled_step(inner, turn, step).await;
        }
        if let Some(compaction) = &inner.runtime.compaction {
            let has_prior_request = inner
                .session
                .events()
                .iter()
                .any(|event| event.event_type == "request/header");
            if has_prior_request
                && inner.session.surface().len() >= compaction.config().max_surface_messages
            {
                if let Err(error) = compaction
                    .compact_for_trigger(
                        &inner.session,
                        CompactionTrigger::Pressure,
                        inner.cancellation.clone(),
                    )
                    .await
                {
                    close_step(inner, turn, step).await?;
                    return end_turn(
                        inner,
                        turn,
                        TurnEndReason::Error {
                            error: compaction_failure(error),
                        },
                    )
                    .await;
                }
            }
        }

        let tool_schemas = inner.runtime.tools.schemas();
        let (system, tools) = if inner.runtime.prompt.complete {
            (
                Some(inner.runtime.prompt.section.text.clone()),
                tool_schemas,
            )
        } else {
            let assembly = inner
                .prompt
                .assemble([&inner.runtime.prompt.section], tool_schemas)
                .map_err(|error| AgentError::Runtime(error.to_string()))?;
            (
                (!assembly.text.is_empty()).then_some(assembly.text),
                assembly.tools,
            )
        };
        let mut request = GenerateRequest {
            provider: inner.options.provider.clone(),
            model: inner.options.model.clone(),
            reasoning_effort: inner.options.reasoning_effort.clone(),
            messages: request_messages(inner),
            system,
            tools: (!tools.is_empty()).then_some(tools),
            temperature: None,
            max_tokens: inner.options.max_tokens,
            stop: None,
            session_id: Some(inner.session.id()),
            purpose: None,
        };
        let effective_header = serde_json::to_value(EpochHeader {
            config: LlmCallConfig {
                provider: request.provider.clone(),
                model: request.model.clone(),
                reasoning_effort: request.reasoning_effort.clone(),
                temperature: request.temperature,
                max_tokens: request.max_tokens,
                stop: request.stop.clone(),
            },
            adapter_defaults: None,
            system: request.system.clone(),
            tools: request.tools.clone(),
        })
        .map_err(|error| AgentError::Runtime(error.to_string()))?;
        if let Some(reason) = request_header_reason(inner, &effective_header) {
            append(
                inner,
                "request/header",
                json!({"header": effective_header, "reason": reason}),
                None,
                None,
            )
            .await?;
            record_request_header(inner, effective_header);
        }
        let mut request_context = json!({
            "provider": request.provider.clone(),
            "model": request.model.clone(),
        });
        if let Some(context_window) = inner
            .context_window
            .as_ref()
            .and_then(|resolve| resolve(&request.provider, &request.model))
        {
            request_context["contextWindow"] = Value::from(context_window);
        }
        let previous_context = inner
            .session
            .events()
            .into_iter()
            .rev()
            .find(|event| event.event_type == "request/context")
            .map(|event| event.data);
        if previous_context.as_ref() != Some(&request_context) {
            append(inner, "request/context", request_context, None, None).await?;
        }

        let mut context_overflow_recovered = false;
        let (generation, chunk_seqs) = loop {
            match consume_generation_attempt(inner, turn, step, request.clone()).await {
                Ok((generation, chunk_seqs)) => match &generation.finish_reason {
                    FinishReason::Error { failure: error } => {
                        if !context_overflow_recovered && is_context_overflow(&error.code) {
                            match compact_context_overflow(inner).await {
                                Ok(true) => {
                                    context_overflow_recovered = true;
                                    request.messages = inner.session.derive_messages();
                                    continue;
                                }
                                Ok(false) => {}
                                Err(error) => {
                                    close_step(inner, turn, step).await?;
                                    return end_turn(
                                        inner,
                                        turn,
                                        TurnEndReason::Error {
                                            error: compaction_failure(error),
                                        },
                                    )
                                    .await;
                                }
                            }
                        }
                        if schedule_retry(inner, turn, step, &request.provider, error).await? {
                            continue;
                        }
                        if inner.cancellation.is_cancelled() {
                            return close_cancelled_step(inner, turn, step).await;
                        }
                        close_step(inner, turn, step).await?;
                        return end_turn(
                            inner,
                            turn,
                            TurnEndReason::Error {
                                error: error.clone(),
                            },
                        )
                        .await;
                    }
                    FinishReason::Aborted { .. } => {
                        return close_cancelled_step(inner, turn, step).await;
                    }
                    FinishReason::Stop | FinishReason::ToolCalls | FinishReason::MaxTokens => {
                        break (generation, chunk_seqs);
                    }
                },
                Err(_error) if inner.cancellation.is_cancelled() => {
                    return close_cancelled_step(inner, turn, step).await;
                }
                Err(error) => {
                    let error = failure(error);
                    if !context_overflow_recovered && is_context_overflow(&error.code) {
                        match compact_context_overflow(inner).await {
                            Ok(true) => {
                                context_overflow_recovered = true;
                                request.messages = inner.session.derive_messages();
                                continue;
                            }
                            Ok(false) => {}
                            Err(error) => {
                                close_step(inner, turn, step).await?;
                                return end_turn(
                                    inner,
                                    turn,
                                    TurnEndReason::Error {
                                        error: compaction_failure(error),
                                    },
                                )
                                .await;
                            }
                        }
                    }
                    if schedule_retry(inner, turn, step, &request.provider, &error).await? {
                        continue;
                    }
                    if inner.cancellation.is_cancelled() {
                        return close_cancelled_step(inner, turn, step).await;
                    }
                    close_step(inner, turn, step).await?;
                    return end_turn(inner, turn, TurnEndReason::Error { error }).await;
                }
            }
        };
        if inner.cancellation.is_cancelled() {
            return close_cancelled_step(inner, turn, step).await;
        }
        append_assistant(
            inner,
            turn,
            step,
            generation.message.clone(),
            generation.usage,
            chunk_seqs,
        )
        .await?;

        match generation.finish_reason {
            FinishReason::ToolCalls => {
                let exit_plan_mode = run_tools(inner, turn, step, &generation.message).await?;
                close_step(inner, turn, step).await?;
                if exit_plan_mode {
                    append(inner, "plan/mode", json!({"active": false}), None, None).await?;
                }
                if inner.cancellation.is_cancelled() {
                    return end_turn(inner, turn, aborted(inner)).await;
                }
                if step == inner.max_steps {
                    return end_turn(inner, turn, TurnEndReason::Blocked).await;
                }
            }
            FinishReason::Stop => {
                close_step(inner, turn, step).await?;
                if inner.cancellation.is_cancelled() {
                    return end_turn(inner, turn, aborted(inner)).await;
                }
                if inner.inbox.has_next_step() {
                    if step == inner.max_steps {
                        return end_turn(inner, turn, TurnEndReason::Blocked).await;
                    }
                } else {
                    return end_turn(inner, turn, TurnEndReason::Completed).await;
                }
            }
            FinishReason::MaxTokens => {
                close_step(inner, turn, step).await?;
                return end_turn(inner, turn, TurnEndReason::MaxTokens).await;
            }
            FinishReason::Error { failure: error } => {
                close_step(inner, turn, step).await?;
                return end_turn(inner, turn, TurnEndReason::Error { error }).await;
            }
            FinishReason::Aborted { .. } => {
                close_step(inner, turn, step).await?;
                return end_turn(inner, turn, aborted(inner)).await;
            }
        }
    }
    end_turn(inner, turn, TurnEndReason::Blocked).await
}

async fn claim_next_turn(inner: &Inner) -> Result<Option<Message>, AgentError> {
    let Some(reservation) = inner.inbox.reserve_next_turn_claim() else {
        return Ok(None);
    };
    let mut messages = commit_inbox_claim(inner, reservation).await?;
    debug_assert_eq!(messages.len(), 1);
    Ok(messages.pop())
}

async fn claim_step_batch(inner: &Inner) -> Result<Vec<Message>, AgentError> {
    let Some(reservation) = inner.inbox.reserve_step_batch_claim() else {
        return Ok(Vec::new());
    };
    commit_inbox_claim(inner, reservation).await
}

async fn commit_inbox_claim(
    inner: &Inner,
    reservation: InboxClaimReservation,
) -> Result<Vec<Message>, AgentError> {
    if durable_inbox_claim(&inner.session, reservation.messages()) {
        append(
            inner,
            "agent/inbox/spliced",
            json!({
                "target": reservation.target(),
                "start": 0,
                "removedCount": reservation.messages().len(),
                "inserted": [],
            }),
            None,
            None,
        )
        .await?;
    }
    reservation
        .commit()
        .ok_or_else(|| AgentError::Runtime("inbox claim reservation was lost".into()))
}

fn durable_inbox_claim(session: &Session, messages: &[Message]) -> bool {
    !messages.is_empty()
        && messages.iter().all(|message| {
            session.events().iter().any(|event| {
                event.event_type == "agent/inbox/enqueued"
                    && event.data.pointer("/message/id").and_then(Value::as_str)
                        == Some(message.id.as_str())
            })
        })
}

async fn append_skill_context(
    inner: &Inner,
    turn: u64,
    step: u64,
    messages: &[Message],
) -> Result<(), AgentError> {
    let Some((skills, scopes)) = &inner.runtime.skills else {
        return Ok(());
    };
    let Some(cwd) = lock(scopes).get(&inner.session.id()).cloned() else {
        return Ok(());
    };
    let catalog = skills.catalog(&cwd, inner.cancellation.clone()).await?;
    let catalog_visible = inner.session.derive_messages().iter().any(|message| {
        matches!(
            &message.source,
            MessageSource::Plugin {
                plugin,
                form: Some(ContextForm::Catalog),
                ..
            } if plugin == "@deepseek-ai/dsh-tool-skill"
        )
    });
    if catalog.complete
        && catalog
            .skills
            .iter()
            .any(|entry| entry.skill.invocation.model_invocable)
        && !catalog_visible
    {
        append_message(
            inner,
            "user/message",
            turn,
            step,
            Message {
                id: MessageId::from(format!("skill-catalog-{turn}-{step}")),
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: model_catalog(&catalog),
                }],
                source: MessageSource::Plugin {
                    plugin: "@deepseek-ai/dsh-tool-skill".into(),
                    compaction_id: None,
                    form: Some(ContextForm::Catalog),
                    sections: None,
                    summary: None,
                },
            },
            None,
            None,
        )
        .await?;
    }
    let user_invocable = catalog
        .skills
        .iter()
        .filter(|entry| entry.skill.invocation.user_invocable)
        .map(|entry| entry.skill.name.as_str())
        .collect::<BTreeSet<_>>();
    for (index, name) in invoked_skill_names(messages, &user_invocable)
        .into_iter()
        .enumerate()
    {
        let skill = skills.get(&cwd, &name, inner.cancellation.clone()).await?;
        append_message(
            inner,
            "user/message",
            turn,
            step,
            Message {
                id: MessageId::from(format!("skill-invocation-{turn}-{step}-{index}")),
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: skill_result_tag(&skill),
                }],
                source: MessageSource::SkillInvocation {
                    name,
                    form: ContextForm::Instructions,
                },
            },
            None,
            None,
        )
        .await?;
    }
    Ok(())
}

async fn append_runtime_context(inner: &Inner, turn: u64, step: u64) -> Result<(), AgentError> {
    let header = inner.session.header();
    let text = runtime_context(&inner.session.events(), header.cwd.as_deref());
    let unchanged = inner
        .session
        .surface()
        .into_iter()
        .rev()
        .find(|entry| matches!(&entry.message.source, MessageSource::Plugin { plugin, .. } if plugin == "@deepseek-ai/dsh-system-prompt"))
        .is_some_and(|entry| matches!(entry.message.content.as_slice(), [ContentBlock::Text { text: previous }] if previous == &text));
    if unchanged {
        return Ok(());
    }
    append_message(
        inner,
        "user/message",
        turn,
        step,
        Message {
            id: MessageId::from(format!("runtime-context-{turn}")),
            role: MessageRole::User,
            content: vec![ContentBlock::Text { text }],
            source: MessageSource::Plugin {
                plugin: "@deepseek-ai/dsh-system-prompt".into(),
                compaction_id: None,
                form: None,
                sections: None,
                summary: None,
            },
        },
        None,
        None,
    )
    .await
}

async fn append_workspace_instructions(
    inner: &Inner,
    turn: u64,
    step: u64,
) -> Result<(), AgentError> {
    let Some(cwd) = inner.session.header().cwd else {
        return Ok(());
    };
    let Ok(instructions) = std::fs::read_to_string(std::path::Path::new(&cwd).join("AGENTS.md"))
    else {
        return Ok(());
    };
    let instructions = instructions.trim();
    if instructions.is_empty() {
        return Ok(());
    }
    let text = format!(
        "<system-reminder>\nThe following workspace instructions may be relevant to your work. Use them as guidance when applicable. More specific instructions take precedence over broader ones. They do not override system, developer, or direct user instructions.\n\nInstructions from: AGENTS.md\n\n{instructions}\n\n</system-reminder>"
    );
    let unchanged = inner.session.surface().into_iter().rev().any(|entry| {
        matches!(&entry.message.source, MessageSource::Plugin { plugin, .. } if plugin == "tessivum-workspace-instructions")
            && matches!(entry.message.content.as_slice(), [ContentBlock::Text { text: previous }] if previous == &text)
    });
    if unchanged {
        return Ok(());
    }
    append_message(
        inner,
        "user/message",
        turn,
        step,
        Message {
            id: MessageId::from(format!("workspace-instructions-{turn}")),
            role: MessageRole::User,
            content: vec![ContentBlock::Text { text }],
            source: MessageSource::Plugin {
                plugin: "tessivum-workspace-instructions".into(),
                compaction_id: None,
                form: Some(ContextForm::Instructions),
                sections: None,
                summary: Some("AGENTS.md".into()),
            },
        },
        None,
        None,
    )
    .await
}

fn request_messages(inner: &Inner) -> Vec<Message> {
    let mut messages = inner.session.derive_messages();
    if let Some(instructions) = inner.session.events().into_iter().rev().find_map(|event| {
        (event.event_type == "user/message"
            && event.data.pointer("/source/kind").and_then(Value::as_str) == Some("plugin")
            && event.data.pointer("/source/plugin").and_then(Value::as_str)
                == Some("tessivum-workspace-instructions"))
        .then(|| serde_json::from_value::<Message>(event.data).ok())
        .flatten()
    }) {
        if !messages.iter().any(|message| message.id == instructions.id) {
            messages.push(instructions);
        }
    }
    messages
}

fn invoked_skill_names(messages: &[Message], available: &BTreeSet<&str>) -> Vec<String> {
    let mut names = Vec::new();
    for message in messages
        .iter()
        .filter(|message| matches!(message.source, MessageSource::User { .. }))
    {
        for text in message.content.iter().filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        }) {
            for name in skill_gestures(text) {
                if available.contains(name) && !names.iter().any(|known| known == name) {
                    names.push(name.to_owned());
                }
            }
        }
    }
    names
}

fn skill_gestures(text: &str) -> impl Iterator<Item = &str> {
    text.match_indices('/').filter_map(move |(index, _)| {
        if text[..index]
            .chars()
            .next_back()
            .is_some_and(|character| !character.is_whitespace())
        {
            return None;
        }
        let rest = &text[index + 1..];
        let bytes = rest.as_bytes();
        let mut end = 0;
        while bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            end += 1;
        }
        if end == 0 {
            return None;
        }
        while bytes.get(end) == Some(&b'-') {
            let segment = end + 1;
            if !bytes
                .get(segment)
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            {
                break;
            }
            end = segment + 1;
            while bytes
                .get(end)
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            {
                end += 1;
            }
        }
        rest[end..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
            .then_some(&rest[..end])
    })
}

fn is_context_overflow(code: &str) -> bool {
    matches!(
        code,
        "CONTEXT_OVERFLOW" | "CONTEXT_WINDOW_EXCEEDED" | "CONTEXT_LENGTH_EXCEEDED"
    )
}

async fn compact_context_overflow(
    inner: &Inner,
) -> Result<bool, crate::compaction::CompactionError> {
    let Some(compaction) = &inner.runtime.compaction else {
        return Ok(false);
    };
    Ok(matches!(
        compaction
            .compact_for_trigger(
                &inner.session,
                CompactionTrigger::ContextOverflow,
                inner.cancellation.clone(),
            )
            .await?,
        CompactionOutcome::Compacted(_)
    ))
}

fn compaction_failure(error: crate::compaction::CompactionError) -> LlmFailure {
    LlmFailure {
        message: error.to_string(),
        code: error.code().into(),
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
}

async fn run_tools(
    inner: &Inner,
    turn: u64,
    step: u64,
    assistant: &Message,
) -> Result<bool, AgentError> {
    let calls = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Some((id.clone(), name.clone(), arguments.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut call_seqs = Vec::with_capacity(calls.len());
    for (id, name, arguments) in &calls {
        call_seqs.push(
            append(
                inner,
                "tool/call",
                json!({
                    "turn": turn,
                    "step": step,
                    "callId": id,
                    "name": name,
                    "arguments": arguments,
                }),
                None,
                None,
            )
            .await?,
        );
    }

    let session = inner.session.id();
    let cancellation = inner.cancellation.clone();
    let tools = inner.runtime.tools.clone();
    let outputs = stream::iter(calls.into_iter().enumerate().map(|(index, (call, name, raw))| {
        let tools = tools.clone();
        let session = session.clone();
        let cancellation = cancellation.clone();
        async move {
            let arguments = match serde_json::from_str::<Value>(&raw) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return (
                        index,
                        call,
                        ToolOutput::new(
                            vec![ContentBlock::Text { text: "tool arguments are not valid JSON".into() }],
                            true,
                            json!({"code": "INVALID_TOOL_ARGUMENTS", "details": error.to_string()}),
                        ),
                    )
                }
            };
            let output = tools
                .execute(
                    ToolRunContext { session, call: call.clone(), cancellation },
                    name,
                    arguments,
                )
                .await;
            (index, call, output)
        }
    }))
    .buffer_unordered(inner.max_parallel_tool_calls)
    .collect::<Vec<_>>()
    .await;
    let mut outputs = outputs;
    outputs.sort_by_key(|(index, _, _)| *index);

    let mut exit_plan_mode = false;
    for ((_, call, output), source_seq) in outputs.into_iter().zip(call_seqs) {
        if let Some(dispatches) = output.meta.get("codeDispatches").and_then(Value::as_array) {
            for dispatch in dispatches {
                let Some(event_type) = dispatch.get("type").and_then(Value::as_str) else {
                    continue;
                };
                let Some(data) = dispatch.get("data") else {
                    continue;
                };
                append(inner, event_type, data.clone(), None, None).await?;
            }
        }
        exit_plan_mode |= output
            .meta
            .get("deferredPlanExit")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut meta = output.meta.clone();
        let deferred_context = meta.as_object_mut().and_then(|object| {
            object.remove("deferredPlanExit");
            object.remove("deferredContext")
        });
        let message = Message {
            id: MessageId::random(),
            role: MessageRole::User,
            content: vec![output.into_content_block(call.clone())],
            source: MessageSource::Tool { call_id: call },
        };
        append_message(
            inner,
            "tool/result",
            turn,
            step,
            message,
            Some(vec![source_seq]),
            Some(meta),
        )
        .await?;
        if let Some(context) = deferred_context.as_ref().and_then(Value::as_object) {
            let plugin = context.get("plugin").and_then(Value::as_str);
            let summary = context.get("summary").and_then(Value::as_str);
            let text = context.get("text").and_then(Value::as_str);
            if let (Some(plugin), Some(summary), Some(text)) = (plugin, summary, text) {
                append_message(
                    inner,
                    "user/message",
                    turn,
                    step,
                    Message {
                        id: MessageId::from(format!("tool-context-{turn}-{step}-{source_seq}")),
                        role: MessageRole::User,
                        content: vec![ContentBlock::Text { text: text.into() }],
                        source: MessageSource::Plugin {
                            plugin: plugin.into(),
                            compaction_id: None,
                            form: Some(ContextForm::Notice),
                            sections: None,
                            summary: Some(summary.into()),
                        },
                    },
                    None,
                    None,
                )
                .await?;
            }
        }
    }
    Ok(exit_plan_mode)
}

async fn consume_generation_attempt(
    inner: &Inner,
    turn: u64,
    step: u64,
    request: GenerateRequest,
) -> Result<(crate::llm::LlmGeneration, Vec<u64>), crate::TessivumError> {
    let provider = request.provider.clone();
    let model = request.model.clone();
    let mut stream = inner
        .llm
        .generate(request, inner.cancellation.clone())
        .await?;
    let mut assembler = BlockAssembler::new(provider, model);
    let mut chunk_seqs = Vec::new();
    let mut generation = None;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let completed = assembler.push(chunk.clone())?;
        let seq = append(
            inner,
            "assistant/chunk",
            json!({"turn": turn, "step": step, "chunk": chunk}),
            None,
            None,
        )
        .await
        .map_err(|error| {
            crate::TessivumError::new(
                "SESSION_APPEND_FAILED",
                error.to_string(),
                "agent-loop",
                Value::Null,
            )
        })?;
        chunk_seqs.push(seq);
        if let Some(completed) = completed {
            generation = Some(completed);
        }
    }

    generation
        .ok_or_else(|| {
            crate::TessivumError::new(
                "LLM_STREAM_ENDED_EARLY",
                "the LLM stream ended before a finish chunk",
                "llm",
                Value::Null,
            )
        })
        .map(|generation| (generation, chunk_seqs))
}

async fn schedule_retry(
    inner: &Inner,
    turn: u64,
    step: u64,
    provider: &str,
    failure: &LlmFailure,
) -> Result<bool, AgentError> {
    let Some(policy) = inner.llm.provider_retry_policy(provider) else {
        return Ok(false);
    };
    if inner.cancellation.is_cancelled() || !policy.permits_failure(&failure.code) {
        return Ok(false);
    }
    let policy_key = policy.policy_key();
    let prior = inner.session.events().into_iter().rev().find(|event| {
        event.event_type == "llm/retry"
            && event.data.get("turn").and_then(Value::as_u64) == Some(turn)
            && event.data.get("step").and_then(Value::as_u64) == Some(step)
            && event.data.get("provider").and_then(Value::as_str) == Some(provider)
            && event.data.get("policyKey").and_then(Value::as_str) == Some(policy_key.as_str())
    });
    let prior_retry = prior
        .as_ref()
        .and_then(|event| event.data.get("retry").and_then(Value::as_u64))
        .unwrap_or(0);
    if policy
        .max_retries()
        .is_some_and(|max_retries| prior_retry >= max_retries)
    {
        return Ok(false);
    }
    let retry = prior_retry + 1;
    let retry_id = prior
        .as_ref()
        .and_then(|event| event.data.get("retryId").and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let random = u128::from_be_bytes(*uuid::Uuid::new_v4().as_bytes()) as f64 / u128::MAX as f64;
    let local_delay_ms = policy.local_delay_ms(retry, random);
    let delay_ms = match failure.provider_retry_after_ms {
        Some(delay) if (delay as f64) > policy.max_delay_ms() && policy.max_retries().is_some() => {
            return Ok(false)
        }
        Some(delay) if (delay as f64) > policy.max_delay_ms() => local_delay_ms,
        Some(delay) if delay > 0 => delay as f64,
        _ => local_delay_ms,
    };
    let mut retry_event = json!({
        "retryId": retry_id,
        "turn": turn,
        "step": step,
        "provider": provider,
        "mode": policy.mode(),
        "policyKey": policy_key,
        "retry": retry,
        "delayMs": delay_ms,
        "failure": failure,
    });
    if let Some(max_retries) = policy.max_retries() {
        retry_event
            .as_object_mut()
            .expect("retry event is an object")
            .insert("maxRetries".into(), Value::from(max_retries));
    }
    append(inner, "llm/retry", retry_event, None, None).await?;
    if inner.cancellation.is_cancelled() {
        return Ok(false);
    }
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs_f64(delay_ms / 1_000.0)) => {}
        _ = inner.cancellation.cancelled() => return Ok(false),
    }
    if inner.cancellation.is_cancelled() {
        return Ok(false);
    }
    append(
        inner,
        "llm/retry-started",
        json!({"retryId": retry_id, "turn": turn, "step": step, "retry": retry}),
        None,
        None,
    )
    .await?;
    Ok(!inner.cancellation.is_cancelled())
}

async fn append_assistant(
    inner: &Inner,
    turn: u64,
    step: u64,
    message: Message,
    usage: Option<crate::protocol::TokenUsage>,
    source_event_seqs: Vec<u64>,
) -> Result<(), AgentError> {
    let mut data = json!({"turn": turn, "step": step, "message": message});
    if let Some(usage) = usage {
        data["usage"] =
            serde_json::to_value(usage).map_err(|error| AgentError::Runtime(error.to_string()))?;
    }
    append(
        inner,
        "assistant/message",
        data,
        Some(source_event_seqs),
        Some(SurfaceOp::Append),
    )
    .await?;
    Ok(())
}

async fn append_message(
    inner: &Inner,
    event_type: &str,
    turn: u64,
    step: u64,
    message: Message,
    source_event_seqs: Option<Vec<u64>>,
    meta: Option<Value>,
) -> Result<(), AgentError> {
    let mut data = if event_type == "user/message" {
        serde_json::to_value(message).map_err(|error| AgentError::Runtime(error.to_string()))?
    } else {
        json!({"turn": turn, "step": step, "message": message})
    };
    if let Some(meta) = meta {
        data["meta"] = meta;
    }
    append(
        inner,
        event_type,
        data,
        source_event_seqs,
        Some(SurfaceOp::Append),
    )
    .await?;
    Ok(())
}

fn request_header_reason(inner: &Inner, header: &Value) -> Option<&'static str> {
    let state = lock(&inner.state);
    state
        .next_request_header_reason
        .or_else(|| (state.last_request_header.as_ref() != Some(header)).then_some("change"))
}

fn record_request_header(inner: &Inner, header: Value) {
    let mut state = lock(&inner.state);
    state.last_request_header = Some(header);
    state.next_request_header_reason = None;
}

async fn close_step(inner: &Inner, turn: u64, step: u64) -> Result<(), AgentError> {
    append(
        inner,
        "step/end",
        json!({"turn": turn, "step": step}),
        None,
        None,
    )
    .await?;
    Ok(())
}

async fn close_cancelled_step(inner: &Inner, turn: u64, step: u64) -> Result<(), AgentError> {
    close_step(inner, turn, step).await?;
    end_turn(inner, turn, aborted(inner)).await
}

async fn end_turn(inner: &Inner, turn: u64, reason: TurnEndReason) -> Result<(), AgentError> {
    let reason = if inner.cancellation.is_cancelled() {
        aborted(inner)
    } else {
        reason
    };
    append(
        inner,
        "turn/end",
        json!({"turn": turn, "reason": reason}),
        None,
        None,
    )
    .await?;
    Ok(())
}

async fn append(
    inner: &Inner,
    event_type: &str,
    data: Value,
    source_event_seqs: Option<Vec<u64>>,
    surface_op: Option<SurfaceOp>,
) -> Result<u64, AgentError> {
    let cancellation = if inner.cancellation.is_cancelled() {
        inner.finalization.clone()
    } else {
        inner.cancellation.clone()
    };
    Ok(inner
        .session
        .append_next(
            |seq| SessionEvent {
                event_type: event_type.into(),
                seq,
                time: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |value| value.as_millis().try_into().unwrap_or(u64::MAX)),
                data,
                ignorable: None,
                source_event_seqs,
                surface_op,
            },
            cancellation,
        )
        .await?)
}

fn next_turn(session: &Session) -> u64 {
    session
        .events()
        .iter()
        .filter(|event| event.event_type == "turn/start")
        .filter_map(|event| event.data.get("turn").and_then(Value::as_u64))
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

fn aborted(inner: &Inner) -> TurnEndReason {
    TurnEndReason::Aborted {
        reason: lock(&inner.cancellation_cause)
            .clone()
            .map(TurnEndCancelCause::from)
            .unwrap_or(TurnEndCancelCause::Legacy),
    }
}

fn failure(error: crate::TessivumError) -> LlmFailure {
    LlmFailure {
        message: error.message,
        code: error.code,
        status: None,
        provider_retry_after_ms: None,
        request_id: None,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

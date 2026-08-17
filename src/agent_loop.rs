//! Durable, wake-coalesced agent turn driver.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle};
use tokio::{sync::Notify, task::JoinHandle};

use crate::{
    agent::{AgentError, AgentFactory, AgentOptions, AgentRuntime, AgentStatus, Inbox},
    llm::LlmRuntime,
    protocol::{
        ContentBlock, EpochHeader, FinishReason, GenerateRequest, LlmCallConfig, LlmFailure,
        Message, MessageId, MessageRole, MessageSource, SessionEvent, SurfaceOp,
        TurnEndCancelCause, TurnEndReason,
    },
    session::Session,
    system_prompt::{PromptSection, SystemPrompt},
    tools::{ToolOutput, ToolRunContext, ToolRuntime},
};

/// Constructs durable agent runtimes backed by the current LLM, prompt, and tool services.
#[derive(Clone, Debug)]
pub struct AgentLoopFactory {
    llm: LlmRuntime,
    prompt: SystemPrompt,
    tools: ToolRuntime,
    max_parallel_tool_calls: usize,
    max_steps: u64,
}

impl AgentLoopFactory {
    pub const DEFAULT_MAX_PARALLEL_TOOL_CALLS: usize = 4;
    pub const DEFAULT_MAX_STEPS: u64 = 32;

    pub fn new(llm: LlmRuntime, prompt: SystemPrompt, tools: ToolRuntime) -> Self {
        Self {
            llm,
            prompt,
            tools,
            max_parallel_tool_calls: Self::DEFAULT_MAX_PARALLEL_TOOL_CALLS,
            max_steps: Self::DEFAULT_MAX_STEPS,
        }
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
        Ok(AgentLoop::spawn(
            session,
            options,
            inbox,
            cancellation,
            self.llm.clone(),
            self.prompt.clone(),
            self.tools.clone(),
            self.max_parallel_tool_calls,
            self.max_steps,
        ))
    }
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
    finalization: CancellationToken,
    llm: LlmRuntime,
    prompt: SystemPrompt,
    tools: ToolRuntime,
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
        tools: ToolRuntime,
        max_parallel_tool_calls: usize,
        max_steps: u64,
    ) -> Arc<Self> {
        let inner = Arc::new(Inner {
            session,
            options,
            inbox,
            cancellation,
            finalization: ContextHandle::root().scope().cancellation(),
            llm,
            prompt,
            tools,
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
        self.inner.cancellation.cancel();
        self.inner.wake.notify_waiters();
        self.inner.idle.notify_waiters();
        if let Some(worker) = worker {
            worker.await.map_err(|error| {
                AgentError::Runtime(format!("agent loop worker failed: {error}"))
            })?;
        }
        Ok(())
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
            let Some(message) = inner.inbox.take_next_turn() else {
                let revision = lock(&inner.state).wake_revision;
                if revision != seen_wake {
                    seen_wake = revision;
                    continue;
                }
                lock(&inner.state).settled_revision = revision;
                inner.running.store(false, Ordering::Release);
                inner.idle.notify_waiters();
                break;
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
    let mut pending_steer = None;
    for step in 1..=inner.max_steps {
        if inner.cancellation.is_cancelled() {
            return end_turn(inner, turn, aborted()).await;
        }
        append(
            inner,
            "step/start",
            json!({"turn": turn, "step": step}),
            None,
            None,
        )
        .await?;

        if let Some(message) = pending_initial.take() {
            append_message(inner, "user/message", turn, step, message, None).await?;
        }
        if let Some(message) = inner.inbox.take_pre_step() {
            append_message(inner, "user/message", turn, step, message, None).await?;
        }
        if let Some(message) = pending_steer
            .take()
            .or_else(|| inner.inbox.take_next_step())
        {
            append_message(inner, "user/message", turn, step, message, None).await?;
        }
        if inner.cancellation.is_cancelled() {
            return close_cancelled_step(inner, turn, step).await;
        }

        let assembly = inner
            .prompt
            .assemble(Vec::<PromptSection>::new(), inner.tools.schemas())
            .map_err(|error| AgentError::Runtime(error.to_string()))?;
        let request = GenerateRequest {
            provider: inner.options.provider.clone(),
            model: inner.options.model.clone(),
            reasoning_effort: None,
            messages: inner.session.derive_messages(),
            system: (!assembly.text.is_empty()).then_some(assembly.text.clone()),
            tools: (!assembly.tools.is_empty()).then_some(assembly.tools.clone()),
            temperature: None,
            max_tokens: inner.options.max_tokens,
            stop: None,
            session_id: Some(inner.session.id()),
            purpose: None,
        };
        append(
            inner,
            "request/header",
            json!({
                "turn": turn,
                "step": step,
                "header": EpochHeader {
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
                },
            }),
            None,
            None,
        )
        .await?;

        let generation = match inner
            .llm
            .complete(request, inner.cancellation.clone())
            .await
        {
            Ok(generation) => generation,
            Err(_) if inner.cancellation.is_cancelled() => {
                return close_cancelled_step(inner, turn, step).await;
            }
            Err(error) => {
                close_step(inner, turn, step).await?;
                return end_turn(
                    inner,
                    turn,
                    TurnEndReason::Error {
                        error: failure(error),
                    },
                )
                .await;
            }
        };

        let mut chunk_seqs = Vec::with_capacity(generation.chunks.len());
        for chunk in &generation.chunks {
            chunk_seqs.push(
                append(
                    inner,
                    "assistant/chunk",
                    json!({"turn": turn, "step": step, "chunk": chunk}),
                    None,
                    None,
                )
                .await?,
            );
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
                run_tools(inner, turn, step, &generation.message).await?;
                close_step(inner, turn, step).await?;
                if inner.cancellation.is_cancelled() {
                    return end_turn(inner, turn, aborted()).await;
                }
                if step == inner.max_steps {
                    return end_turn(inner, turn, TurnEndReason::Blocked).await;
                }
            }
            FinishReason::Stop => {
                close_step(inner, turn, step).await?;
                if inner.cancellation.is_cancelled() {
                    return end_turn(inner, turn, aborted()).await;
                }
                if let Some(steer) = inner.inbox.take_next_step() {
                    if step == inner.max_steps {
                        return end_turn(inner, turn, TurnEndReason::Blocked).await;
                    }
                    pending_steer = Some(steer);
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
                return end_turn(inner, turn, aborted()).await;
            }
        }
    }
    end_turn(inner, turn, TurnEndReason::Blocked).await
}

async fn run_tools(
    inner: &Inner,
    turn: u64,
    step: u64,
    assistant: &Message,
) -> Result<(), AgentError> {
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
    let tools = inner.tools.clone();
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

    for ((_, call, output), source_seq) in outputs.into_iter().zip(call_seqs) {
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
        )
        .await?;
    }
    Ok(())
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
) -> Result<(), AgentError> {
    append(
        inner,
        event_type,
        json!({"turn": turn, "step": step, "message": message}),
        source_event_seqs,
        Some(SurfaceOp::Append),
    )
    .await?;
    Ok(())
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
    end_turn(inner, turn, aborted()).await
}

async fn end_turn(inner: &Inner, turn: u64, reason: TurnEndReason) -> Result<(), AgentError> {
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
    let seq = inner.session.next_seq()?;
    let event = SessionEvent {
        event_type: event_type.into(),
        seq,
        time: 0,
        data,
        ignorable: None,
        source_event_seqs,
        surface_op,
    };
    let cancellation = if inner.cancellation.is_cancelled() {
        inner.finalization.clone()
    } else {
        inner.cancellation.clone()
    };
    inner.session.append(event, cancellation).await?;
    Ok(seq)
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

fn aborted() -> TurnEndReason {
    TurnEndReason::Aborted {
        reason: TurnEndCancelCause::Legacy,
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

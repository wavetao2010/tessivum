//! Minimal durable planning mode and whole-list todo snapshots.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{
    agent::AgentHandle,
    session::{Session, SessionError},
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext,
    },
    ContentBlock, SessionEvent, SessionId, TessivumError, TodoItem,
};

/// Stable key for the agent-owned planning service.
pub fn planning_service_key() -> ServiceKey {
    ServiceKey::new("harness.planning", "1")
}

/// The only observable planning switch. It deliberately carries no speculative plan fields.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanMode {
    #[default]
    Normal,
    Plan,
}

/// Durable mode-change payload.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanChange {
    pub mode: PlanMode,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum PlanningError {
    #[error("planning service is not owned by its live agent")]
    NotLive,
    #[error("planning operation was cancelled")]
    Cancelled,
    #[error("todo snapshot has more than 256 items")]
    TooManyTodos,
    #[error("todo content exceeds 4096 bytes")]
    TodoTooLong,
    #[error("durable planning payload is invalid")]
    InvalidReplay,
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl PlanningError {
    fn tool_error(&self) -> TessivumError {
        let code = match self {
            Self::NotLive => "PLAN_NOT_LIVE",
            Self::Cancelled => "CANCELLED",
            Self::TooManyTodos | Self::TodoTooLong => "INVALID_TODO_SNAPSHOT",
            Self::InvalidReplay => "INVALID_PLAN_REPLAY",
            Self::Session(_) => "PLAN_EVENT_APPEND_FAILED",
        };
        TessivumError::new(code, self.to_string(), "planning", Value::Null)
    }
}

#[derive(Default)]
struct PlanningState {
    mode: PlanMode,
    todos: Vec<TodoItem>,
}

struct PlanningInner {
    agent: AgentHandle,
    session: Arc<Session>,
    state: Mutex<PlanningState>,
}

/// A single agent's planning state. The service owns no todo ids, priorities, or patches.
#[derive(Clone)]
pub struct PlanningService {
    inner: Arc<PlanningInner>,
}

impl PlanningService {
    /// Restores the latest whole plan and todo snapshots from the owned session.
    pub fn new(agent: AgentHandle) -> Result<Self, PlanningError> {
        let session = agent.session();
        if agent.id() != session.id() || agent.is_disposed() {
            return Err(PlanningError::NotLive);
        }
        let mut state = PlanningState::default();
        for event in session.events() {
            match event.event_type.as_str() {
                "plan/change" => {
                    let change: PlanChange = serde_json::from_value(event.data)
                        .map_err(|_| PlanningError::InvalidReplay)?;
                    state.mode = change.mode;
                }
                "todo/write" => {
                    let todos: TodoWrite = serde_json::from_value(event.data)
                        .map_err(|_| PlanningError::InvalidReplay)?;
                    validate_todos(&todos.todos)?;
                    state.todos = todos.todos;
                }
                _ => {}
            }
        }
        Ok(Self {
            inner: Arc::new(PlanningInner {
                agent,
                session,
                state: Mutex::new(state),
            }),
        })
    }

    /// Publishes this service under `harness.planning@1` for the owning scope lifetime.
    pub fn publish(&self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(planning_service_key(), self.clone())
    }

    pub fn agent_id(&self) -> SessionId {
        self.inner.agent.id()
    }

    pub async fn mode(&self) -> PlanMode {
        self.inner.state.lock().await.mode
    }

    pub async fn todos(&self) -> Vec<TodoItem> {
        self.inner.state.lock().await.todos.clone()
    }

    /// Persists a whole mode snapshot before exposing it.
    pub async fn set_mode(
        &self,
        mode: PlanMode,
        cancellation: CancellationToken,
    ) -> Result<(), PlanningError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        let mut state = self.inner.state.lock().await;
        check_cancellation(&cancellation)?;
        append(
            &self.inner.session,
            "plan/change",
            serde_json::to_value(PlanChange { mode }).expect("plan mode is serializable"),
            cancellation,
        )
        .await?;
        state.mode = mode;
        Ok(())
    }

    /// Replaces the complete todo list; it never creates ids, priorities, or patches.
    pub async fn write_todos(
        &self,
        todos: Vec<TodoItem>,
        cancellation: CancellationToken,
    ) -> Result<(), PlanningError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        validate_todos(&todos)?;
        let mut state = self.inner.state.lock().await;
        check_cancellation(&cancellation)?;
        append(
            &self.inner.session,
            "todo/write",
            serde_json::to_value(TodoWrite {
                todos: todos.clone(),
            })
            .expect("todo write is serializable"),
            cancellation,
        )
        .await?;
        state.todos = todos;
        Ok(())
    }

    pub fn plan_tool_definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "plan",
            "Sets the minimal observable planning mode.",
            plan_schema(),
            PlanTool {
                planning: self.clone(),
            },
        )
    }

    pub fn todo_tool_definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "todo",
            "Replaces the complete todo list with content and status only.",
            todo_schema(),
            TodoTool {
                planning: self.clone(),
            },
        )
    }

    /// Owns both planning tool registrations until this value is dropped.
    pub fn register_tools(
        &self,
        runtime: &crate::tools::ToolRuntime,
    ) -> Result<PlanningTools, TessivumError> {
        Ok(PlanningTools {
            _plan: runtime.register(self.plan_tool_definition())?,
            _todo: runtime.register(self.todo_tool_definition())?,
        })
    }

    fn require_live(&self) -> Result<(), PlanningError> {
        if self.inner.agent.is_disposed() || self.inner.agent.id() != self.inner.session.id() {
            Err(PlanningError::NotLive)
        } else {
            Ok(())
        }
    }

    async fn set_mode_from_tool(
        &self,
        context: ToolRunContext,
        input: PlanToolInput,
    ) -> Result<ToolOutput, PlanningError> {
        if context.session != self.agent_id() {
            return Err(PlanningError::NotLive);
        }
        self.set_mode(input.mode, context.cancellation).await?;
        Ok(output(&PlanChange { mode: input.mode }))
    }

    async fn write_todos_from_tool(
        &self,
        context: ToolRunContext,
        input: TodoWrite,
    ) -> Result<ToolOutput, PlanningError> {
        if context.session != self.agent_id() {
            return Err(PlanningError::NotLive);
        }
        self.write_todos(input.todos.clone(), context.cancellation)
            .await?;
        Ok(output(&input))
    }
}

/// Lifetime owner for the two actual planning tools.
#[derive(Debug)]
pub struct PlanningTools {
    _plan: ToolRegistration,
    _todo: ToolRegistration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PlanToolInput {
    mode: PlanMode,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TodoWrite {
    todos: Vec<TodoItem>,
}

struct PlanTool {
    planning: PlanningService,
}

#[async_trait]
impl ToolHandler for PlanTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let input = serde_json::from_value(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_PLAN",
                "plan tool input is invalid",
                "planning",
                Value::Null,
            )
        })?;
        self.planning
            .set_mode_from_tool(context, input)
            .await
            .map_err(|error| error.tool_error())
    }
}

struct TodoTool {
    planning: PlanningService,
}

#[async_trait]
impl ToolHandler for TodoTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let input = serde_json::from_value(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_TODO_SNAPSHOT",
                "todo tool input is invalid",
                "planning",
                Value::Null,
            )
        })?;
        self.planning
            .write_todos_from_tool(context, input)
            .await
            .map_err(|error| error.tool_error())
    }
}

fn validate_todos(todos: &[TodoItem]) -> Result<(), PlanningError> {
    if todos.len() > 256 {
        return Err(PlanningError::TooManyTodos);
    }
    for todo in todos {
        todo.validate().map_err(|_| PlanningError::InvalidReplay)?;
        if todo.content.len() > 4_096 {
            return Err(PlanningError::TodoTooLong);
        }
    }
    Ok(())
}

fn output<T: Serialize>(value: &T) -> ToolOutput {
    ToolOutput::new(
        vec![ContentBlock::Text {
            text: serde_json::to_string(value).expect("planning value is serializable"),
        }],
        false,
        Value::Null,
    )
}

async fn append(
    session: &Session,
    event_type: &str,
    data: Value,
    cancellation: CancellationToken,
) -> Result<(), PlanningError> {
    session
        .append(
            SessionEvent {
                event_type: event_type.into(),
                seq: session.next_seq()?,
                time: 0,
                data,
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            cancellation,
        )
        .await?;
    Ok(())
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), PlanningError> {
    if cancellation.is_cancelled() {
        Err(PlanningError::Cancelled)
    } else {
        Ok(())
    }
}

fn plan_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false, "required": ["mode"],
        "properties": {"mode": {"type": "string", "enum": ["normal", "plan"]}}
    })
}

fn todo_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false, "required": ["todos"],
        "properties": {"todos": {"type": "array", "items": {
            "type": "object", "additionalProperties": false, "required": ["content", "status"],
            "properties": {
                "content": {"type": "string"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
            }
        }}}
    })
}

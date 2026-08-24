//! Durable plan mode and whole-list `todo_write` snapshots.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;

use crate::{
    agent::AgentHandle,
    protocol::TodoStatus,
    question::{
        AskUserQuestionIntent, AskUserQuestionItem, AskUserQuestionOption, HostQuestionRegistry,
    },
    session::{Session, SessionError},
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    ContentBlock, SessionEvent, SessionId, TessivumError, TodoItem,
};

/// Stable key for the agent-owned planning service.
pub fn planning_service_key() -> ServiceKey {
    ServiceKey::new("harness.planning", "1")
}

/// The session's durable plan-mode state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanMode {
    #[default]
    Normal,
    Plan,
}

impl PlanMode {
    pub fn active(self) -> bool {
        self == Self::Plan
    }
}

/// Durable whole-value plan-mode payload. The last `plan/mode` wins.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanModeChange {
    pub active: bool,
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
    #[error("todo content must be a unique non-empty string")]
    InvalidTodo,
    #[error("durable planning payload is invalid")]
    InvalidReplay,
    #[error("exit_plan_mode is only available in plan mode")]
    NotInPlanMode,
    #[error("exit_plan_mode requires a non-empty markdown plan starting with a # heading")]
    InvalidPlan,
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl PlanningError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::NotLive => "PLAN_NOT_LIVE",
            Self::Cancelled => "CANCELLED",
            Self::TooManyTodos | Self::TodoTooLong | Self::InvalidTodo => "INVALID_TODO_SNAPSHOT",
            Self::InvalidReplay => "INVALID_PLAN_REPLAY",
            Self::NotInPlanMode => "PLAN_MODE_INACTIVE",
            Self::InvalidPlan => "INVALID_PLAN",
            Self::Session(_) => "PLAN_EVENT_APPEND_FAILED",
        }
    }

    fn tool_error(&self) -> TessivumError {
        TessivumError::new(self.code(), self.to_string(), "planning", Value::Null)
    }
}

struct PlanningInner {
    agent: AgentHandle,
    session: Arc<Session>,
}

/// A single agent's durable plan and todo state.
#[derive(Clone)]
pub struct PlanningService {
    inner: Arc<PlanningInner>,
}

impl PlanningService {
    /// Restores and validates the session-owned planning facts before publishing a service.
    pub fn new(agent: AgentHandle) -> Result<Self, PlanningError> {
        let session = agent.session();
        if agent.id() != session.id() || agent.is_disposed() {
            return Err(PlanningError::NotLive);
        }
        fold_plan_mode(&session.events())?;
        fold_todos(&session.events())?;
        Ok(Self {
            inner: Arc::new(PlanningInner { agent, session }),
        })
    }

    pub fn publish(&self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(planning_service_key(), self.clone())
    }

    pub fn agent_id(&self) -> SessionId {
        self.inner.agent.id()
    }

    pub async fn mode(&self) -> PlanMode {
        fold_plan_mode(&self.inner.session.events()).unwrap_or(PlanMode::Normal)
    }

    /// The standing todo list retires as soon as a later turn begins.
    pub async fn todos(&self) -> Vec<TodoItem> {
        fold_todos(&self.inner.session.events())
            .unwrap_or_default()
            .unwrap_or_default()
    }

    /// Persists a whole mode value before making it observable.
    pub async fn set_mode(
        &self,
        mode: PlanMode,
        cancellation: CancellationToken,
    ) -> Result<(), PlanningError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        append(
            &self.inner.session,
            "plan/mode",
            serde_json::to_value(PlanModeChange {
                active: mode.active(),
            })
            .expect("plan mode is serializable"),
            cancellation,
        )
        .await
    }

    /// Replaces the complete todo list; a later `turn/start` retires it.
    pub async fn write_todos(
        &self,
        mut todos: Vec<TodoItem>,
        cancellation: CancellationToken,
    ) -> Result<Vec<TodoItem>, PlanningError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        normalize_todos(&mut todos)?;
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
        Ok(todos)
    }

    pub async fn exit_plan_mode(
        &self,
        plan: &str,
        cancellation: CancellationToken,
    ) -> Result<(), PlanningError> {
        self.validate_exit_plan(plan).await?;
        self.set_mode(PlanMode::Normal, cancellation).await
    }

    /// Validates a plan before presenting it to the user for review.
    pub async fn validate_exit_plan(&self, plan: &str) -> Result<(), PlanningError> {
        self.require_live()?;
        if !self.mode().await.active() {
            return Err(PlanningError::NotInPlanMode);
        }
        if !has_plan_heading(plan) {
            return Err(PlanningError::InvalidPlan);
        }
        Ok(())
    }

    fn require_live(&self) -> Result<(), PlanningError> {
        if self.inner.agent.is_disposed() || self.inner.agent.id() != self.inner.session.id() {
            Err(PlanningError::NotLive)
        } else {
            Ok(())
        }
    }
}

/// Folds the upstream `plan/mode` event format.
pub fn fold_plan_mode(events: &[SessionEvent]) -> Result<PlanMode, PlanningError> {
    let mut mode = PlanMode::Normal;
    for event in events {
        if event.event_type == "plan/mode" {
            let change: PlanModeChange = serde_json::from_value(event.data.clone())
                .map_err(|_| PlanningError::InvalidReplay)?;
            mode = if change.active {
                PlanMode::Plan
            } else {
                PlanMode::Normal
            };
        }
    }
    Ok(mode)
}

/// Folds the standing todo projection. It is absent before the first write and after turn start.
pub fn fold_todos(events: &[SessionEvent]) -> Result<Option<Vec<TodoItem>>, PlanningError> {
    let mut todos = None;
    for event in events {
        match event.event_type.as_str() {
            "todo/write" => {
                let mut write: TodoWrite = serde_json::from_value(event.data.clone())
                    .map_err(|_| PlanningError::InvalidReplay)?;
                normalize_todos(&mut write.todos)?;
                todos = Some(write.todos);
            }
            "turn/start" => todos = None,
            _ => {}
        }
    }
    Ok(todos)
}

/// Session-routed owner for the two globally registered model tools.
#[derive(Clone, Default)]
pub struct PlanningToolRouter {
    services: Arc<Mutex<BTreeMap<SessionId, PlanningService>>>,
}

impl PlanningToolRouter {
    pub fn insert(&self, service: PlanningService) {
        lock(&self.services).insert(service.agent_id(), service);
    }

    pub fn remove(&self, session_id: &SessionId) {
        lock(&self.services).remove(session_id);
    }

    pub fn register_tools(
        &self,
        runtime: &ToolRuntime,
        questions: HostQuestionRegistry,
    ) -> Result<PlanningTools, TessivumError> {
        Ok(PlanningTools {
            _exit_plan_mode: runtime.register(ToolDefinition::new(
                "exit_plan_mode",
                "Use only in plan mode. Present the complete markdown plan for user review; approval leaves plan mode.",
                exit_plan_mode_schema(),
                ExitPlanModeTool {
                    router: self.clone(),
                    questions,
                },
            ))?,
            _todo_write: runtime.register(ToolDefinition::new(
                "todo_write",
                "Record and update a structured task list for the current work. Send the ENTIRE list every call; it replaces the previous list. Use pending, in_progress, or completed status.",
                todo_schema(),
                TodoWriteTool { router: self.clone() },
            ))?,
        })
    }

    fn service(&self, session: &SessionId) -> Result<PlanningService, PlanningError> {
        lock(&self.services)
            .get(session)
            .cloned()
            .ok_or(PlanningError::NotLive)
    }
}

/// Lifetime owner for the actual planning tools.
#[derive(Debug)]
pub struct PlanningTools {
    _exit_plan_mode: ToolRegistration,
    _todo_write: ToolRegistration,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TodoWrite {
    todos: Vec<TodoItem>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExitPlanModeInput {
    plan: String,
}

struct ExitPlanModeTool {
    router: PlanningToolRouter,
    questions: HostQuestionRegistry,
}

#[async_trait]
impl ToolHandler for ExitPlanModeTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let input: ExitPlanModeInput = serde_json::from_value(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_PLAN",
                "exit_plan_mode tool input is invalid",
                "planning",
                Value::Null,
            )
        })?;
        let planning = self
            .router
            .service(&context.session)
            .map_err(|error| error.tool_error())?;
        planning
            .validate_exit_plan(&input.plan)
            .await
            .map_err(|error| error.tool_error())?;
        let answer = self
            .questions
            .ask(
                context.clone(),
                vec![AskUserQuestionItem {
                    id: "plan-review".into(),
                    header: Some("Plan review".into()),
                    question: "Approve this plan and leave plan mode?".into(),
                    detail: Some(input.plan.clone()),
                    options: Some(vec![
                        AskUserQuestionOption {
                            label: "Approve".into(),
                            description: Some(
                                "Leave plan mode; carry out the plan from your next step.".into(),
                            ),
                        },
                        AskUserQuestionOption {
                            label: "Keep planning".into(),
                            description: Some(
                                "Stay in plan mode; revise and present the plan again.".into(),
                            ),
                        },
                    ]),
                    multi_select: None,
                    intent: Some(AskUserQuestionIntent::PlanReview {
                        approve: "Approve".into(),
                    }),
                }],
            )
            .await
            .map_err(|error| {
                if error.code == "ASK_CANCELLED" {
                    TessivumError::new(
                        "PLAN_REVIEW_DISMISSED",
                        "The user dismissed the plan review to speak instead; stay in plan mode and wait for their message.",
                        "planning",
                        Value::Null,
                    )
                } else {
                    error
                }
            })?;
        let approved = answer.answers.len() == 1
            && answer.answers[0].id == "plan-review"
            && answer.answers[0].selected.len() == 1
            && answer.answers[0].selected[0] == "Approve"
            && answer.answers[0].custom.is_none();
        if !approved {
            return Err(TessivumError::new(
                "PLAN_REVIEW_DECLINED",
                "The user chose to keep planning; revise the plan and present it again.",
                "planning",
                Value::Null,
            ));
        }
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: "Plan approved — plan mode exited; carry out the plan starting with your next step.".into(),
            }],
            false,
            json!({"deferredPlanExit": true}),
        ))
    }
}

struct TodoWriteTool {
    router: PlanningToolRouter,
}

#[async_trait]
impl ToolHandler for TodoWriteTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let input: TodoWrite = serde_json::from_value(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_TODO_SNAPSHOT",
                "todo_write tool input is invalid",
                "planning",
                Value::Null,
            )
        })?;
        let todos = self
            .router
            .service(&context.session)
            .map_err(|error| error.tool_error())?
            .write_todos(input.todos, context.cancellation)
            .await
            .map_err(|error| error.tool_error())?;
        let counts = TodoCounts::from(todos.as_slice());
        Ok(output(&TodoWriteOutput { todos, counts }))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TodoWriteOutput {
    todos: Vec<TodoItem>,
    counts: TodoCounts,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TodoCounts {
    pending: usize,
    in_progress: usize,
    completed: usize,
}

impl From<&[TodoItem]> for TodoCounts {
    fn from(todos: &[TodoItem]) -> Self {
        let mut counts = Self {
            pending: 0,
            in_progress: 0,
            completed: 0,
        };
        for todo in todos {
            match todo.status {
                TodoStatus::Pending => counts.pending += 1,
                TodoStatus::InProgress => counts.in_progress += 1,
                TodoStatus::Completed => counts.completed += 1,
            }
        }
        counts
    }
}

fn normalize_todos(todos: &mut [TodoItem]) -> Result<(), PlanningError> {
    if todos.len() > 256 {
        return Err(PlanningError::TooManyTodos);
    }
    let mut seen = BTreeSet::new();
    for todo in todos {
        todo.content = todo.content.trim().into();
        if todo.content.is_empty()
            || todo.content.len() > 4_096
            || !seen.insert(todo.content.clone())
        {
            return Err(if todo.content.len() > 4_096 {
                PlanningError::TodoTooLong
            } else {
                PlanningError::InvalidTodo
            });
        }
        todo.validate().map_err(|_| PlanningError::InvalidTodo)?;
    }
    Ok(())
}

fn has_plan_heading(plan: &str) -> bool {
    let plan = plan.trim();
    plan.strip_prefix("#")
        .and_then(|line| line.strip_prefix(char::is_whitespace))
        .is_some_and(|title| !title.trim().is_empty())
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

fn exit_plan_mode_schema() -> Value {
    json!({
        "type": "object", "additionalProperties": false, "required": ["plan"],
        "properties": {"plan": {"type": "string"}}
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

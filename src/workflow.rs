//! One durable workflow engine over untyped JSON input.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::{
    agent::{AgentHandle, AgentRegistry},
    protocol::{
        ContentBlock, Message, MessageId, MessageRole, MessageSource, SessionEvent, SessionId,
        SurfaceOp, WorkflowAgentOutcome, WorkflowRunId, WorkflowStopReason,
    },
    session::{Session, SessionError, SessionStore},
    subagent::{
        SubagentActivation, SubagentError, SubagentMode, SubagentParent, SubagentRunResult,
        SubagentRunStatus, SubagentService, SubagentStartRequest,
    },
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    TessivumError,
};

/// Stable service key for the single workflow engine capability.
pub fn workflow_service_key() -> ServiceKey {
    ServiceKey::new("harness.workflow", "1")
}

/// Plain JSON accepted by the workflow engine. The runtime validates
/// `meta.name` for its canonical durable record and parses `script` as the
/// supported straight-line subset; metadata and arguments remain uninterpreted.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRequest {
    pub script: Value,
    pub meta: Value,
    pub args: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowRunStatus {
    Completed,
    Cancelled,
    Error,
}

/// A terminal workflow result. Operational engine failures are values, not
/// rejected workflow invocations.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunResult {
    pub run_id: WorkflowRunId,
    pub status: WorkflowRunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WorkflowFailure>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowFailure {
    pub code: String,
    pub message: String,
}

/// Fatal caller misuse. Once a run is admitted, its result is always a
/// [`WorkflowRunResult`].
#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("parent session is required and must be live")]
    ParentRequired,
    #[error("workflow meta.name must be a non-empty string")]
    InvalidWorkflowName,
    #[error("maximum total agents must be at least one")]
    InvalidAgentLimit,
    #[error("workflow child limit exceeded")]
    AgentLimitExceeded,
    #[error("workflow child admission is closing")]
    Closing,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Subagent(#[from] SubagentError),
    #[error(transparent)]
    Protocol(#[from] TessivumError),
}

/// The sole engine implementation used by a runtime instance.
#[async_trait]
pub trait WorkflowEngine: Send + Sync {
    async fn run(
        &self,
        context: WorkflowContext,
        request: WorkflowRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError>;
}

/// Native runner for the supported straight-line workflow subset: literal
/// `phase("name")`, `const name = await agent("prompt")`, and
/// `return { name }`. It deliberately does not execute arbitrary JavaScript.
#[derive(Clone)]
pub struct NativeWorkflowEngine {
    mode: NativeWorkflowMode,
}

#[derive(Clone)]
enum NativeWorkflowMode {
    Live,
    Recorded(Arc<Mutex<VecDeque<Value>>>),
}

impl Default for NativeWorkflowEngine {
    fn default() -> Self {
        Self {
            mode: NativeWorkflowMode::Live,
        }
    }
}

impl NativeWorkflowEngine {
    /// Uses durable workflow tool results from `recording` for replayed child
    /// replies. `None` selects live child execution.
    pub fn from_recording(recording: Option<&str>) -> Result<Self, TessivumError> {
        match recording {
            Some(recording) => Ok(Self {
                mode: NativeWorkflowMode::Recorded(Arc::new(Mutex::new(
                    recorded_workflow_results(recording)?,
                ))),
            }),
            None => Ok(Self::default()),
        }
    }

    fn recorded_result(&self, program: &WorkflowProgram) -> Result<Option<Value>, TessivumError> {
        let NativeWorkflowMode::Recorded(results) = &self.mode else {
            return Ok(None);
        };
        let mut results = lock(results);
        let result = results.front().cloned().ok_or_else(|| {
            workflow_tool_error("recorded workflow has no durable workflow tool result")
        })?;
        validate_recorded_result(&result, program)?;
        results.pop_front();
        Ok(Some(result))
    }
}

#[async_trait]
impl WorkflowEngine for NativeWorkflowEngine {
    async fn run(
        &self,
        context: WorkflowContext,
        request: WorkflowRequest,
        cancellation: CancellationToken,
    ) -> Result<Value, TessivumError> {
        let script = request
            .script
            .as_str()
            .ok_or_else(|| workflow_tool_error("workflow script must be a string"))?;
        let program = parse_workflow_script(script)?;
        let recorded_result = self.recorded_result(&program)?;
        let WorkflowProgram {
            operations,
            return_bindings,
        } = program;
        let mut phase = None;
        let mut replies = BTreeMap::new();
        let mut sequence = 0_u64;

        for operation in operations {
            match operation {
                WorkflowOperation::Phase(next) => {
                    if let Some(previous) = phase.replace(next.clone()) {
                        context.phase_end(previous, Value::Null).await;
                    }
                    context.phase_start(next, Value::Null).await;
                }
                WorkflowOperation::Agent { binding, prompt } => {
                    if cancellation.is_cancelled() {
                        return Err(workflow_tool_error("workflow was cancelled"));
                    }
                    if phase.is_none() {
                        let default_phase = "Run".to_owned();
                        context
                            .phase_start(default_phase.clone(), Value::Null)
                            .await;
                        phase = Some(default_phase);
                    }
                    sequence += 1;
                    let reply = if let Some(result) = recorded_result.as_ref() {
                        let reply = result
                            .get(&binding)
                            .and_then(Value::as_str)
                            .expect("recorded workflow result was validated");
                        context
                            .record_replay_child(
                                sequence,
                                phase.as_deref().expect("a phase was installed"),
                                &prompt,
                                reply,
                            )
                            .await?;
                        reply.to_owned()
                    } else {
                        context.run_child(sequence, &prompt).await?
                    };
                    replies.insert(binding, reply);
                }
            }
        }
        if let Some(phase) = phase {
            context.phase_end(phase, Value::Null).await;
        }
        Ok(recorded_result.unwrap_or_else(|| {
            Value::Object(
                return_bindings
                    .into_iter()
                    .map(|binding| {
                        let reply = replies
                            .remove(&binding)
                            .expect("workflow parser validated every return binding");
                        (binding, Value::String(reply))
                    })
                    .collect(),
            )
        }))
    }
}

#[derive(Clone, Debug)]
struct WorkflowProgram {
    operations: Vec<WorkflowOperation>,
    return_bindings: Vec<String>,
}

#[derive(Clone, Debug)]
enum WorkflowOperation {
    Phase(String),
    Agent { binding: String, prompt: String },
}

fn parse_workflow_script(script: &str) -> Result<WorkflowProgram, TessivumError> {
    WorkflowParser::new(script)
        .parse()
        .map_err(workflow_script_error)
}

struct WorkflowParser<'a> {
    script: &'a str,
    offset: usize,
}

impl<'a> WorkflowParser<'a> {
    fn new(script: &'a str) -> Self {
        Self { script, offset: 0 }
    }

    fn parse(mut self) -> Result<WorkflowProgram, String> {
        let mut operations = Vec::new();
        let mut bindings = BTreeMap::new();
        loop {
            self.skip_whitespace();
            if self.at_end() {
                return Err("workflow script must end with return { ... }".into());
            }
            let keyword = self
                .identifier()
                .ok_or_else(|| self.unsupported_statement())?;
            match keyword {
                "phase" => {
                    self.skip_whitespace();
                    self.expect_char('(')?;
                    self.skip_whitespace();
                    let name = self.string_literal()?;
                    self.skip_whitespace();
                    self.expect_char(')')?;
                    self.statement_end()?;
                    operations.push(WorkflowOperation::Phase(name));
                }
                "const" => {
                    self.skip_whitespace();
                    let binding = self.identifier().ok_or_else(|| {
                        "workflow agent bindings must be identifier names".to_owned()
                    })?;
                    if bindings.contains_key(binding) {
                        return Err(format!(
                            "workflow binding {binding:?} is declared more than once"
                        ));
                    }
                    self.skip_whitespace();
                    self.expect_char('=')?;
                    self.skip_whitespace();
                    self.expect_keyword("await")?;
                    self.skip_whitespace();
                    self.expect_keyword("agent")?;
                    self.skip_whitespace();
                    self.expect_char('(')?;
                    self.skip_whitespace();
                    let prompt = self.string_literal()?;
                    self.skip_whitespace();
                    self.expect_char(')')?;
                    self.statement_end()?;
                    bindings.insert(binding.to_owned(), ());
                    operations.push(WorkflowOperation::Agent {
                        binding: binding.to_owned(),
                        prompt,
                    });
                }
                "return" => {
                    self.skip_whitespace();
                    self.expect_char('{')?;
                    let mut returned = Vec::new();
                    loop {
                        self.skip_whitespace();
                        if self.consume_char('}') {
                            break;
                        }
                        let binding = self.identifier().ok_or_else(|| {
                            "workflow return objects may contain only binding names".to_owned()
                        })?;
                        if returned.iter().any(|name| name == binding) {
                            return Err(format!(
                                "workflow return binding {binding:?} appears more than once"
                            ));
                        }
                        returned.push(binding.to_owned());
                        self.skip_whitespace();
                        if self.consume_char('}') {
                            break;
                        }
                        self.expect_char(',')?;
                    }
                    self.skip_whitespace();
                    self.consume_char(';');
                    self.skip_whitespace();
                    if !self.at_end() {
                        return Err(self.unsupported_statement());
                    }
                    if bindings.is_empty() {
                        return Err("workflow script must call agent at least once".into());
                    }
                    if returned.len() != bindings.len()
                        || returned
                            .iter()
                            .any(|binding| !bindings.contains_key(binding))
                    {
                        return Err(
                            "workflow return object must contain every agent binding exactly once"
                                .into(),
                        );
                    }
                    return Ok(WorkflowProgram {
                        operations,
                        return_bindings: returned,
                    });
                }
                _ => return Err(self.unsupported_statement()),
            }
        }
    }

    fn at_end(&self) -> bool {
        self.offset == self.script.len()
    }

    fn rest(&self) -> &'a str {
        &self.script[self.offset..]
    }

    fn skip_whitespace(&mut self) -> bool {
        let before = self.offset;
        while self.rest().chars().next().is_some_and(char::is_whitespace) {
            self.take_char();
        }
        self.offset != before
    }

    fn take_char(&mut self) -> Option<char> {
        let character = self.rest().chars().next()?;
        self.offset += character.len_utf8();
        Some(character)
    }

    fn consume_char(&mut self, expected: char) -> bool {
        if self.rest().starts_with(expected) {
            self.offset += expected.len_utf8();
            true
        } else {
            false
        }
    }

    fn expect_char(&mut self, expected: char) -> Result<(), String> {
        self.consume_char(expected)
            .then_some(())
            .ok_or_else(|| format!("expected {expected:?}"))
    }

    fn identifier(&mut self) -> Option<&'a str> {
        let start = self.offset;
        let first = self.take_char()?;
        if !matches!(first, 'A'..='Z' | 'a'..='z' | '_' | '$') {
            self.offset = start;
            return None;
        }
        while self.rest().chars().next().is_some_and(
            |character| matches!(character, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '$'),
        ) {
            self.take_char();
        }
        Some(&self.script[start..self.offset])
    }

    fn expect_keyword(&mut self, expected: &str) -> Result<(), String> {
        let keyword = self
            .identifier()
            .ok_or_else(|| format!("expected {expected}"))?;
        (keyword == expected)
            .then_some(())
            .ok_or_else(|| format!("expected {expected}"))
    }

    fn string_literal(&mut self) -> Result<String, String> {
        let quote = self
            .take_char()
            .filter(|character| matches!(character, '\'' | '"'))
            .ok_or_else(|| {
                "workflow phase names and agent prompts must be string literals".to_owned()
            })?;
        let mut value = String::new();
        loop {
            let character = self
                .take_char()
                .ok_or_else(|| "unterminated string literal".to_owned())?;
            match character {
                character if character == quote => return Ok(value),
                '\\' => {
                    let escaped = self
                        .take_char()
                        .ok_or_else(|| "unterminated escape sequence".to_owned())?;
                    value.push(match escaped {
                        '\\' => '\\',
                        '\'' => '\'',
                        '"' => '"',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        _ => return Err("unsupported string escape".into()),
                    });
                }
                '\n' | '\r' => return Err("string literals may not contain raw newlines".into()),
                character => value.push(character),
            }
        }
    }

    fn statement_end(&mut self) -> Result<(), String> {
        if self.consume_char(';') {
            self.skip_whitespace();
            return Ok(());
        }
        let before = self.offset;
        self.skip_whitespace();
        (self.at_end()
            || self.script[before..self.offset]
                .chars()
                .any(|character| matches!(character, '\n' | '\r')))
        .then_some(())
        .ok_or_else(|| "workflow statements must be separated by a newline or semicolon".into())
    }

    fn unsupported_statement(&self) -> String {
        if self.rest().starts_with("//") || self.rest().starts_with("/*") {
            "workflow comments are not supported".into()
        } else {
            "workflow supports only straight-line literal phase(...), const name = await agent(...), and return { name } statements".into()
        }
    }
}

fn workflow_script_error(reason: impl AsRef<str>) -> TessivumError {
    workflow_tool_error(format!(
        "workflow scripts support only straight-line literal phase(...), const name = await agent(...), and return {{ name }} statements; arbitrary JavaScript is not executed: {}",
        reason.as_ref()
    ))
}

fn recorded_workflow_results(recording: &str) -> Result<VecDeque<Value>, TessivumError> {
    let mut pending = BTreeMap::new();
    let mut results = VecDeque::new();
    for (line_number, line) in recording.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line).map_err(|error| {
            workflow_recording_error(format!(
                "workflow recording line {} is not valid JSON: {error}",
                line_number + 1
            ))
        })?;
        let Some(event_type) = row.get("type").and_then(Value::as_str) else {
            continue;
        };
        let data = row.get("data").unwrap_or(&Value::Null);
        match event_type {
            "tool/call" if data.get("name").and_then(Value::as_str) == Some("workflow") => {
                let call_id = data
                    .get("callId")
                    .and_then(Value::as_str)
                    .filter(|call_id| !call_id.is_empty())
                    .ok_or_else(|| {
                        workflow_recording_error("recorded workflow call has no callId")
                    })?;
                if pending.insert(call_id.to_owned(), ()).is_some() {
                    return Err(workflow_recording_error(
                        "recorded workflow call IDs must be unique",
                    ));
                }
            }
            "tool/result" => {
                let Some(call_id) = recorded_result_call_id(data) else {
                    continue;
                };
                if pending.remove(call_id).is_some() {
                    results.push_back(recorded_workflow_result(data, call_id)?);
                }
            }
            _ => {}
        }
    }
    if !pending.is_empty() {
        return Err(workflow_recording_error(
            "recording has a workflow call without a durable tool result",
        ));
    }
    Ok(results)
}

fn recorded_result_call_id(data: &Value) -> Option<&str> {
    data.get("callId").and_then(Value::as_str).or_else(|| {
        data.pointer("/message/source/callId")
            .and_then(Value::as_str)
    })
}

fn recorded_workflow_result(data: &Value, call_id: &str) -> Result<Value, TessivumError> {
    let (content, is_error) = if let Some(message) = data.get("message") {
        let result = message
            .get("content")
            .and_then(Value::as_array)
            .and_then(|content| {
                content.iter().find(|block| {
                    block.get("type").and_then(Value::as_str) == Some("tool-result")
                        && block.get("toolCallId").and_then(Value::as_str) == Some(call_id)
                })
            })
            .ok_or_else(|| {
                workflow_recording_error(
                    "recorded workflow result has no matching tool-result block",
                )
            })?;
        (
            result.get("content").and_then(Value::as_array),
            result.get("isError").and_then(Value::as_bool),
        )
    } else {
        (
            data.get("content").and_then(Value::as_array),
            data.get("isError").and_then(Value::as_bool),
        )
    };
    if is_error != Some(false) {
        return Err(workflow_recording_error(
            "recorded workflow tool result must be successful",
        ));
    }
    let text = content
        .ok_or_else(|| workflow_recording_error("recorded workflow result has no content"))?
        .iter()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<String>();
    let value = text
        .rsplit_once("\nReturn value:\n")
        .or_else(|| {
            text.strip_prefix("Return value:\n")
                .map(|value| ("", value))
        })
        .map(|(_, value)| value)
        .ok_or_else(|| workflow_recording_error("recorded workflow result has no return value"))?;
    serde_json::from_str(value).map_err(|error| {
        workflow_recording_error(format!(
            "recorded workflow return value is not valid JSON: {error}"
        ))
    })
}

fn validate_recorded_result(
    result: &Value,
    program: &WorkflowProgram,
) -> Result<(), TessivumError> {
    let object = result.as_object().ok_or_else(|| {
        workflow_tool_error("recorded workflow return value must be an object of child replies")
    })?;
    if object.len() != program.return_bindings.len()
        || program
            .return_bindings
            .iter()
            .any(|binding| !matches!(object.get(binding), Some(Value::String(_))))
    {
        return Err(workflow_tool_error(
            "recorded workflow return value must contain exactly the script's string child bindings",
        ));
    }
    Ok(())
}

fn workflow_recording_error(message: impl Into<String>) -> TessivumError {
    TessivumError::new(
        "INVALID_WORKFLOW_RECORDING",
        message,
        "workflow",
        Value::Null,
    )
}

#[derive(Clone)]
pub struct WorkflowTool {
    runtime: WorkflowRuntime,
    agents: AgentRegistry,
}

pub struct WorkflowTools {
    _workflow: ToolRegistration,
}

pub fn register_workflow_tool(
    tools: &ToolRuntime,
    runtime: WorkflowRuntime,
    agents: AgentRegistry,
) -> Result<WorkflowTools, TessivumError> {
    Ok(WorkflowTools {
        _workflow: tools.register(ToolDefinition::new(
            "workflow",
            "Run a named straight-line workflow: literal phase(...), const name = await agent(...), then return { name }. Arbitrary JavaScript is not executed.",
            json!({
                "type": "object",
                "properties": {"script": {"type": "string"}, "meta": {"type": "object", "properties": {"name": {"type": "string"}, "description": {"type": "string"}}, "required": ["name"], "additionalProperties": false}, "args": {"type": "object", "properties": {}, "additionalProperties": true}},
                "required": ["script", "meta"],
                "additionalProperties": false
            }),
            WorkflowTool { runtime, agents },
        ))?,
    })
}

#[async_trait]
impl ToolHandler for WorkflowTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let request = WorkflowRequest {
            script: arguments.get("script").cloned().unwrap_or(Value::Null),
            meta: arguments.get("meta").cloned().unwrap_or(Value::Null),
            args: arguments.get("args").cloned().unwrap_or(Value::Null),
        };
        let agent = self
            .agents
            .get(&context.session)
            .ok_or_else(|| workflow_tool_error("workflow requires a live parent agent"))?;
        let result = self
            .runtime
            .attach(Arc::new(agent))
            .map_err(workflow_error)?
            .run(request, context.cancellation)
            .await
            .map_err(workflow_error)?;
        if result.status != WorkflowRunStatus::Completed {
            return Err(workflow_tool_error(
                result
                    .error
                    .as_ref()
                    .map_or("workflow did not complete", |error| error.message.as_str()),
            ));
        }
        let value = result.value.unwrap_or(Value::Null);
        let name = arguments
            .pointer("/meta/name")
            .and_then(Value::as_str)
            .unwrap_or("workflow");
        let count = value.as_object().map_or(1, |bindings| bindings.len());
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: format!(
                    "workflow {name:?} completed ({count} agent{}).\nReturn value:\n{}",
                    if count == 1 { "" } else { "s" },
                    serde_json::to_string_pretty(&value).expect("workflow value serializes")
                ),
            }],
            false,
            Value::Null,
        ))
    }
}

fn workflow_tool_error(message: impl Into<String>) -> TessivumError {
    TessivumError::new("WORKFLOW_FAILED", message, "workflow", Value::Null)
}

fn workflow_error(error: WorkflowError) -> TessivumError {
    workflow_tool_error(error.to_string())
}

#[derive(Clone)]
struct WorkflowChildResult {
    acceptance_id: u64,
    result: SubagentRunResult,
}

struct ChildAdmissions {
    closing: bool,
    pending: usize,
    children: Vec<SubagentActivation>,
    late_children: Vec<SubagentActivation>,
}

struct WorkflowAdmissionPermit {
    state: Arc<RunState>,
    released: bool,
}

impl WorkflowAdmissionPermit {
    fn admit_or_queue(&self, activation: SubagentActivation) -> bool {
        let mut admissions = self.state.children.lock();
        if admissions.closing {
            admissions.late_children.push(activation);
            false
        } else {
            admissions.children.push(activation);
            true
        }
    }

    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let wake = {
            let mut admissions = self.state.children.lock();
            admissions.pending -= 1;
            admissions.closing && admissions.pending == 0
        };
        if wake {
            self.state.admissions_quiesced.notify_waiters();
        }
    }
}

impl Drop for WorkflowAdmissionPermit {
    fn drop(&mut self) {
        self.release();
    }
}

struct RunState {
    run_id: WorkflowRunId,
    parent: Arc<Session>,
    cancellation: CancellationToken,
    subagents: SubagentParent,
    children: Mutex<ChildAdmissions>,
    admissions_quiesced: Notify,
    cleanup_started: AtomicBool,
    cleanup_done: AtomicBool,
    cleanup_finished: Notify,
    cleanup_results: Mutex<Vec<WorkflowChildResult>>,
}

impl RunState {
    fn reserve_admission(self: &Arc<Self>) -> Option<WorkflowAdmissionPermit> {
        let mut admissions = self.children.lock();
        if admissions.closing {
            return None;
        }
        admissions.pending += 1;
        Some(WorkflowAdmissionPermit {
            state: Arc::clone(self),
            released: false,
        })
    }

    fn begin_cleanup(self: &Arc<Self>) {
        self.cancellation.cancel();
        if self.cleanup_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let results = cleanup_children(Arc::clone(&state)).await;
            *state.cleanup_results.lock() = results;
            state.cleanup_done.store(true, Ordering::Release);
            state.cleanup_finished.notify_waiters();
        });
    }

    async fn close_and_dispose(self: &Arc<Self>) -> Vec<WorkflowChildResult> {
        self.begin_cleanup();
        loop {
            let notified = self.cleanup_finished.notified();
            if self.cleanup_done.load(Ordering::Acquire) {
                break;
            }
            notified.await;
        }
        self.cleanup_results.lock().clone()
    }
}

struct WorkflowInner {
    sessions: SessionStore,
    subagents: SubagentService,
    engine: Arc<dyn WorkflowEngine>,
    max_total_agents: usize,
    active: Mutex<BTreeMap<WorkflowRunId, Arc<RunState>>>,
}

/// The single workflow engine and its parent-owned child cleanup boundary.
#[derive(Clone)]
pub struct WorkflowRuntime {
    inner: Arc<WorkflowInner>,
}

impl fmt::Debug for WorkflowRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowRuntime")
            .field("active_runs", &lock(&self.inner.active).len())
            .field("max_total_agents", &self.inner.max_total_agents)
            .finish_non_exhaustive()
    }
}

/// An opaque capability attached to one live workflow-parent agent generation.
#[derive(Clone)]
pub struct WorkflowParent {
    inner: Arc<WorkflowInner>,
    parent: Arc<AgentHandle>,
}

impl fmt::Debug for WorkflowParent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowParent")
            .field("parent", &self.parent.id())
            .finish_non_exhaustive()
    }
}

impl WorkflowParent {
    /// Runs one workflow under this parent generation.
    pub async fn run(
        &self,
        request: WorkflowRequest,
        caller_cancellation: CancellationToken,
    ) -> Result<WorkflowRunResult, WorkflowError> {
        run_workflow(&self.inner, &self.parent, request, caller_cancellation).await
    }
}

/// An opaque accepted run capability. It is the only way to cancel a run;
/// its stable identifier is observational data only.
#[derive(Clone)]
pub struct WorkflowRun {
    state: Arc<RunState>,
}

impl WorkflowRun {
    pub fn run_id(&self) -> &WorkflowRunId {
        &self.state.run_id
    }

    pub async fn dispose(&self) -> Vec<SubagentRunResult> {
        self.state
            .close_and_dispose()
            .await
            .into_iter()
            .map(|child| child.result)
            .collect()
    }
}

impl WorkflowRuntime {
    pub fn new(
        sessions: SessionStore,
        subagents: SubagentService,
        engine: Arc<dyn WorkflowEngine>,
        max_total_agents: usize,
    ) -> Result<Self, WorkflowError> {
        if max_total_agents == 0 {
            return Err(WorkflowError::InvalidAgentLimit);
        }
        Ok(Self {
            inner: Arc::new(WorkflowInner {
                sessions,
                subagents,
                engine,
                max_total_agents,
                active: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(workflow_service_key(), self)
    }

    /// Derives a workflow parent capability from the live owner attachment.
    pub fn attach(&self, parent: Arc<AgentHandle>) -> Result<WorkflowParent, WorkflowError> {
        require_live_parent(&self.inner, &parent)?;
        Ok(WorkflowParent {
            inner: Arc::clone(&self.inner),
            parent,
        })
    }
}

/// Engine-facing context. It exposes only a run capability and parent-bound
/// child creation; none accepts caller-supplied authority IDs.
#[derive(Clone)]
pub struct WorkflowContext {
    observer: WorkflowObserver,
    run: WorkflowRun,
    parent_agent: Arc<AgentHandle>,
}

impl WorkflowContext {
    pub fn run_id(&self) -> &WorkflowRunId {
        self.run.run_id()
    }

    pub fn run(&self) -> WorkflowRun {
        self.run.clone()
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.observer.state.cancellation.clone()
    }

    async fn run_child(&self, seq: u64, prompt: &str) -> Result<String, TessivumError> {
        let child_session_id = SessionId::random();
        let activation = self
            .start_agent(SubagentStartRequest {
                provider: "native".into(),
                agent_id: workflow_label(prompt),
                child_session_id,
                mode: SubagentMode::OneShot,
                capabilities: Vec::new(),
                options: self.parent_agent.options(),
                created_at: workflow_now(),
                cwd: None,
                resume: false,
                initial_message: Some(Message {
                    id: MessageId::random(),
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text {
                        text: prompt.into(),
                    }],
                    source: MessageSource::User {
                        client_time_zone: None,
                    },
                }),
            })
            .await
            .map_err(workflow_error)?;
        let result = activation
            .run()
            .await
            .map_err(|error| workflow_tool_error(error.to_string()))?;
        self.end_agent(&activation, &result).await;
        let text = result
            .last_assistant_message
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            Err(workflow_tool_error(format!(
                "workflow agent {seq} returned no assistant text"
            )))
        } else {
            Ok(text)
        }
    }

    async fn record_replay_child(
        &self,
        _seq: u64,
        _phase: &str,
        prompt: &str,
        reply: &str,
    ) -> Result<(), TessivumError> {
        let child_id = SessionId::random();
        let activation = self
            .start_agent(SubagentStartRequest {
                provider: "native".into(),
                agent_id: workflow_label(prompt),
                child_session_id: child_id.clone(),
                mode: SubagentMode::OneShot,
                capabilities: Vec::new(),
                options: self.parent_agent.options(),
                created_at: workflow_now(),
                cwd: None,
                resume: false,
                initial_message: None,
            })
            .await
            .map_err(workflow_error)?;
        let session = self
            .observer
            .inner
            .sessions
            .get(&child_id)
            .ok_or_else(|| workflow_tool_error("workflow child session was not created"))?;
        append_child_event(&session, "turn/start", json!({"turn": 1}), None).await?;
        let user = Message {
            id: MessageId::random(),
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: prompt.into(),
            }],
            source: MessageSource::User {
                client_time_zone: None,
            },
        };
        append_child_event(
            &session,
            "user/message",
            serde_json::to_value(user).expect("workflow prompt serializes"),
            Some(SurfaceOp::Append),
        )
        .await?;
        append_child_event(&session, "step/start", json!({"turn": 1, "step": 1}), None).await?;
        let assistant = Message {
            id: MessageId::random(),
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text { text: reply.into() }],
            source: MessageSource::Model {
                provider: "recorded".into(),
                model: "recorded".into(),
                replay_state: None,
            },
        };
        append_child_event(
            &session,
            "assistant/message",
            json!({"turn": 1, "step": 1, "message": assistant}),
            Some(SurfaceOp::Append),
        )
        .await?;
        append_child_event(&session, "step/end", json!({"turn": 1, "step": 1}), None).await?;
        append_child_event(
            &session,
            "turn/end",
            json!({"turn": 1, "reason": {"kind": "completed"}}),
            None,
        )
        .await?;
        let result = activation
            .settle_replay(SubagentRunResult {
                status: SubagentRunStatus::Completed,
                error: None,
                last_assistant_message: Some(vec![ContentBlock::Text { text: reply.into() }]),
            })
            .await;
        self.end_agent(&activation, &result).await;
        Ok(())
    }

    pub async fn phase_start(&self, name: impl Into<String>, _data: Value) {
        *self.observer.phase.lock() = Some(name.into());
    }

    pub async fn phase_end(&self, name: impl Into<String>, _data: Value) {
        let name = name.into();
        let mut phase = self.observer.phase.lock();
        if phase.as_deref() == Some(name.as_str()) {
            *phase = None;
        }
    }

    /// Logs are not workflow-run facts and therefore have no durable lifecycle record.
    pub async fn log(&self, _name: impl Into<String>, _data: Value) {}

    /// Starts a bounded child and admits it through the closing gate. A child
    /// accepted while cleanup begins is disposed before this call returns.
    pub async fn start_agent(
        &self,
        request: SubagentStartRequest,
    ) -> Result<SubagentActivation, WorkflowError> {
        let count = self.observer.agents.fetch_add(1, Ordering::AcqRel) + 1;
        if count > self.observer.inner.max_total_agents {
            self.observer.agents.fetch_sub(1, Ordering::AcqRel);
            return Err(WorkflowError::AgentLimitExceeded);
        }
        let mut permit = self
            .observer
            .state
            .reserve_admission()
            .ok_or(WorkflowError::Closing)?;
        let label = request.agent_id.clone();
        let child_id = request.child_session_id.clone();
        let phase = self.observer.phase.lock().clone();
        let (_, activation) = match self
            .observer
            .state
            .subagents
            .start(request, self.observer.state.cancellation.clone())
            .await
        {
            Ok(started) => started,
            Err(error) => return Err(error.into()),
        };
        let admitted = permit.admit_or_queue(activation.clone());
        permit.release();
        if !admitted {
            self.observer.state.begin_cleanup();
            return Err(WorkflowError::Closing);
        }
        self.observer
            .record_agent_start(
                activation.acceptance_id(),
                count as u64,
                label,
                phase,
                child_id,
            )
            .await;
        Ok(activation)
    }

    pub async fn end_agent(&self, activation: &SubagentActivation, result: &SubagentRunResult) {
        self.observer
            .record_agent_end(activation.acceptance_id(), result)
            .await;
    }
}

struct WorkflowMember {
    seq: u64,
    ended: bool,
}

#[derive(Clone)]
struct WorkflowObserver {
    inner: Arc<WorkflowInner>,
    state: Arc<RunState>,
    phase: Arc<Mutex<Option<String>>>,
    members: Arc<Mutex<BTreeMap<u64, WorkflowMember>>>,
    recording_failed: Arc<AsyncMutex<bool>>,
    agents: Arc<AtomicUsize>,
}

impl WorkflowObserver {
    async fn record(&self, event_type: &str, data: Value) -> bool {
        let mut failed = self.recording_failed.lock().await;
        if *failed {
            return false;
        }
        if append(&self.state.parent, event_type, data).await.is_err() {
            *failed = true;
            return false;
        }
        true
    }

    async fn append_run(&self, name: &str) {
        let _ = self
            .record(
                "tool-workflow/run-start",
                json!({"runId": self.state.run_id, "name": name}),
            )
            .await;
    }

    async fn record_agent_start(
        &self,
        acceptance_id: u64,
        seq: u64,
        label: String,
        phase: Option<String>,
        child_id: crate::protocol::SessionId,
    ) {
        let mut data = json!({
            "runId": self.state.run_id,
            "seq": seq,
            "label": label,
            "childId": child_id,
        });
        if let Some(phase) = phase {
            data["phase"] = json!(phase);
        }
        if self.record("tool-workflow/agent-start", data).await {
            self.members
                .lock()
                .insert(acceptance_id, WorkflowMember { seq, ended: false });
        }
    }

    async fn record_agent_end(&self, acceptance_id: u64, result: &SubagentRunResult) {
        let seq = {
            let mut members = self.members.lock();
            let Some(member) = members.get_mut(&acceptance_id) else {
                return;
            };
            if member.ended {
                return;
            }
            member.ended = true;
            member.seq
        };
        let outcome = match result.status {
            SubagentRunStatus::Completed => WorkflowAgentOutcome::Completed,
            SubagentRunStatus::Cancelled => WorkflowAgentOutcome::Cancelled,
            SubagentRunStatus::Error => WorkflowAgentOutcome::Failed,
        };
        let _ = self
            .record(
                "tool-workflow/agent-end",
                json!({"runId": self.state.run_id, "seq": seq, "outcome": outcome}),
            )
            .await;
    }

    async fn finish_members(&self, results: &[WorkflowChildResult]) {
        for child in results {
            self.record_agent_end(child.acceptance_id, &child.result)
                .await;
        }
    }

    async fn append_run_end(&self, stop_reason: WorkflowStopReason) {
        let _ = self
            .record(
                "tool-workflow/run-end",
                json!({"runId": self.state.run_id, "stopReason": stop_reason}),
            )
            .await;
    }
}

async fn run_workflow(
    inner: &Arc<WorkflowInner>,
    parent_agent: &Arc<AgentHandle>,
    request: WorkflowRequest,
    caller_cancellation: CancellationToken,
) -> Result<WorkflowRunResult, WorkflowError> {
    let parent = require_live_parent(inner, parent_agent)?;
    let name = workflow_name(&request)?.to_owned();
    let subagents = inner.subagents.attach(Arc::clone(parent_agent))?;
    let run_id = WorkflowRunId::random();
    let state = Arc::new(RunState {
        run_id: run_id.clone(),
        parent,
        cancellation: ContextHandle::root().scope().cancellation(),
        subagents,
        children: Mutex::new(ChildAdmissions {
            closing: false,
            pending: 0,
            children: Vec::new(),
            late_children: Vec::new(),
        }),
        admissions_quiesced: Notify::new(),
        cleanup_started: AtomicBool::new(false),
        cleanup_done: AtomicBool::new(false),
        cleanup_finished: Notify::new(),
        cleanup_results: Mutex::new(Vec::new()),
    });
    let observer = WorkflowObserver {
        inner: Arc::clone(inner),
        state: Arc::clone(&state),
        phase: Arc::new(Mutex::new(None)),
        members: Arc::new(Mutex::new(BTreeMap::new())),
        recording_failed: Arc::new(AsyncMutex::new(false)),
        agents: Arc::new(AtomicUsize::new(0)),
    };
    observer.append_run(&name).await;
    lock(&inner.active).insert(run_id.clone(), Arc::clone(&state));

    let parent_cancellation = parent_agent.cancellation();
    let result = if caller_cancellation.is_cancelled() || parent_cancellation.is_cancelled() {
        state.cancellation.cancel();
        cancelled_result(&run_id)
    } else {
        let execution = inner.engine.run(
            WorkflowContext {
                observer: observer.clone(),
                run: WorkflowRun {
                    state: Arc::clone(&state),
                },
                parent_agent: Arc::clone(parent_agent),
            },
            request,
            state.cancellation.clone(),
        );
        tokio::pin!(execution);
        let outcome = tokio::select! {
            outcome = &mut execution => outcome,
            _ = caller_cancellation.cancelled() => {
                state.cancellation.cancel();
                execution.await
            }
            _ = parent_cancellation.cancelled() => {
                state.cancellation.cancel();
                execution.await
            }
        };
        if state.cancellation.is_cancelled() {
            cancelled_result(&run_id)
        } else {
            match outcome {
                Ok(value) => WorkflowRunResult {
                    run_id: run_id.clone(),
                    status: WorkflowRunStatus::Completed,
                    value: Some(value),
                    error: None,
                },
                Err(error) => error_result(&run_id, error.code, error.message),
            }
        }
    };

    let child_results = state.close_and_dispose().await;
    let mut result = result;
    if child_results
        .iter()
        .any(|child| matches!(child.result.status, SubagentRunStatus::Error))
    {
        result = error_result(
            &run_id,
            "WORKFLOW_CHILD_FAILED",
            "a workflow child failed during shutdown",
        );
    }
    observer.finish_members(&child_results).await;
    observer.append_run_end(stop_reason(result.status)).await;
    lock(&inner.active).remove(&run_id);
    Ok(result)
}

async fn cleanup_children(state: Arc<RunState>) -> Vec<WorkflowChildResult> {
    let children = {
        let mut admissions = state.children.lock();
        admissions.closing = true;
        std::mem::take(&mut admissions.children)
    };
    let mut results = Vec::with_capacity(children.len());
    for child in children {
        let acceptance_id = child.acceptance_id();
        if let Ok(result) = child.dispose().await {
            results.push(WorkflowChildResult {
                acceptance_id,
                result,
            });
        }
    }
    loop {
        let notified = state.admissions_quiesced.notified();
        if state.children.lock().pending == 0 {
            break;
        }
        notified.await;
    }
    let late_children = std::mem::take(&mut state.children.lock().late_children);
    for child in late_children {
        let acceptance_id = child.acceptance_id();
        if let Ok(result) = child.dispose().await {
            results.push(WorkflowChildResult {
                acceptance_id,
                result,
            });
        }
    }
    results
}

fn require_live_parent(
    inner: &WorkflowInner,
    parent: &AgentHandle,
) -> Result<Arc<Session>, WorkflowError> {
    if parent.is_disposed() {
        return Err(WorkflowError::ParentRequired);
    }
    let attached = parent.session();
    inner
        .sessions
        .get(&parent.id())
        .filter(|live| Arc::ptr_eq(live, &attached))
        .ok_or(WorkflowError::ParentRequired)
}
fn workflow_name(request: &WorkflowRequest) -> Result<&str, WorkflowError> {
    request
        .meta
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or(WorkflowError::InvalidWorkflowName)
}

fn workflow_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn workflow_label(prompt: &str) -> String {
    let mut label = prompt.chars().take(48).collect::<String>();
    if prompt.chars().count() > 48 {
        label.push('…');
    }
    label
}

async fn append_child_event(
    session: &Session,
    event_type: &str,
    data: Value,
    surface_op: Option<SurfaceOp>,
) -> Result<(), TessivumError> {
    session
        .append(
            SessionEvent {
                event_type: event_type.into(),
                seq: session
                    .next_seq()
                    .map_err(|error| workflow_tool_error(error.to_string()))?,
                time: workflow_now(),
                data,
                ignorable: None,
                source_event_seqs: None,
                surface_op,
            },
            ContextHandle::root().scope().cancellation(),
        )
        .await
        .map_err(|error| workflow_tool_error(error.to_string()))
}

async fn append(parent: &Session, event_type: &str, data: Value) -> Result<(), SessionError> {
    let event = SessionEvent {
        event_type: event_type.into(),
        seq: parent.next_seq()?,
        time: 0,
        data,
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
    };
    parent
        .append(event, ContextHandle::root().scope().cancellation())
        .await
}

fn cancelled_result(run_id: &WorkflowRunId) -> WorkflowRunResult {
    WorkflowRunResult {
        run_id: run_id.clone(),
        status: WorkflowRunStatus::Cancelled,
        value: None,
        error: None,
    }
}

fn error_result(
    run_id: &WorkflowRunId,
    code: impl Into<String>,
    message: impl Into<String>,
) -> WorkflowRunResult {
    WorkflowRunResult {
        run_id: run_id.clone(),
        status: WorkflowRunStatus::Error,
        value: None,
        error: Some(WorkflowFailure {
            code: code.into(),
            message: message.into(),
        }),
    }
}

fn stop_reason(status: WorkflowRunStatus) -> WorkflowStopReason {
    match status {
        WorkflowRunStatus::Completed => WorkflowStopReason::Completed,
        WorkflowRunStatus::Cancelled => WorkflowStopReason::Cancelled,
        WorkflowRunStatus::Error => WorkflowStopReason::Error,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    mutex.lock()
}

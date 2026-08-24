//! Browser-backed `ask_user_question` tool and its exact-session pending registry.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle};
use tokio::sync::oneshot;

use crate::{
    agent::{same_authority, AgentAuthority},
    approval::RpcReceipt,
    session::Session,
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    SessionEvent, SessionId, TessivumError, ToolCallId,
};

const ASK_USER_QUESTION_DESCRIPTION: &str = "Ask the user a concise question when you need confirmation, a choice, or missing information before proceeding. Send one or more questions, each with a stable id that will be echoed in the answer.";

/// One selectable answer offered to the user.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionOption {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One question displayed by the browser composer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskUserQuestionItem {
    pub id: String,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<AskUserQuestionOption>>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "multi_select")]
    pub multi_select: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<AskUserQuestionIntent>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum AskUserQuestionIntent {
    #[serde(rename = "plan-review")]
    PlanReview { approve: String },
}

/// One structured answer, retained even for a skipped question.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AskUserQuestionAnswerItem {
    pub id: String,
    pub selected: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

/// The complete answer batch for one request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AskUserQuestionAnswer {
    pub answers: Vec<AskUserQuestionAnswerItem>,
}

/// Durable request audit record. The browser's rpc id is the request identity.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionAsked {
    pub rpc_id: String,
    pub session_id: SessionId,
    pub turn: u64,
    pub call_id: ToolCallId,
    pub questions: Vec<AskUserQuestionItem>,
}

/// Final state of one browser question request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionOutcome {
    Answered,
    Cancelled,
}

/// Durable answer or cancellation record.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionResolved {
    pub rpc_id: String,
    pub turn: u64,
    pub outcome: QuestionOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<AskUserQuestionAnswer>,
}

/// Browser-visible request, replayed verbatim until it is claimed.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRequested {
    pub rpc_id: String,
    pub session_id: SessionId,
    pub questions: Vec<AskUserQuestionItem>,
}

/// Browser-visible settlement notice emitted after the durable resolution commits.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionResolvedNotice {
    pub rpc_id: String,
    pub session_id: SessionId,
    pub outcome: QuestionOutcome,
}

#[derive(Clone, Debug)]
pub enum QuestionNotification {
    Requested(QuestionRequested),
    Resolved(QuestionResolvedNotice),
}

struct QuestionSlot {
    authority: AgentAuthority,
    session: Arc<Session>,
}

struct PendingQuestion {
    requested: QuestionRequested,
    authority: AgentAuthority,
    turn: u64,
    sender: oneshot::Sender<QuestionReply>,
}

#[derive(Default)]
struct PendingState {
    entries: BTreeMap<String, PendingQuestion>,
    order: VecDeque<String>,
}

struct HostQuestionRegistryInner {
    slots: Mutex<BTreeMap<SessionId, QuestionSlot>>,
    pending: Mutex<PendingState>,
    notices: tokio::sync::broadcast::Sender<QuestionNotification>,
}

/// Host-wide owner of browser question waits. A slot is tied to one live agent
/// generation, so a stale session or a delegated agent cannot claim another
/// generation's answer channel.
#[derive(Clone)]
pub struct HostQuestionRegistry {
    inner: Arc<HostQuestionRegistryInner>,
}

impl Default for HostQuestionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HostQuestionRegistry {
    pub fn new() -> Self {
        let (notices, _) = tokio::sync::broadcast::channel(256);
        Self {
            inner: Arc::new(HostQuestionRegistryInner {
                slots: Mutex::new(BTreeMap::new()),
                pending: Mutex::new(PendingState::default()),
                notices,
            }),
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<QuestionNotification> {
        self.inner.notices.subscribe()
    }

    /// Installs browser-question authority for one exact live agent generation.
    pub fn install(
        &self,
        authority: &AgentAuthority,
        session: Arc<Session>,
    ) -> Result<HostQuestionRegistration, TessivumError> {
        if !authority.is_live() || authority.id() != session.id() {
            return Err(question_error(
                "CALLER_NOT_LIVE",
                "human interaction requires the exact live calling agent",
                Value::Null,
            ));
        }
        let session_id = session.id();
        let mut slots = lock(&self.inner.slots);
        if slots.contains_key(&session_id) {
            return Err(question_error(
                "DUPLICATE_PROVIDER",
                "a user-questions provider is already registered for this session",
                Value::Null,
            ));
        }
        slots.insert(
            session_id.clone(),
            QuestionSlot {
                authority: authority.clone(),
                session,
            },
        );
        Ok(HostQuestionRegistration {
            inner: Arc::downgrade(&self.inner),
            session_id,
            authority: authority.clone(),
            closed: AtomicBool::new(false),
        })
    }

    /// Current unclaimed requests retain their original insertion order and rpc ids.
    pub fn snapshots(&self) -> Vec<QuestionRequested> {
        let pending = lock(&self.inner.pending);
        pending
            .order
            .iter()
            .filter_map(|rpc_id| pending.entries.get(rpc_id))
            .map(|entry| entry.requested.clone())
            .collect()
    }

    /// Whether this rpc id is an unclaimed question request. This lets `/api/respond`
    /// choose the exact response schema before parsing it.
    pub fn is_pending(&self, rpc_id: &str) -> bool {
        lock(&self.inner.pending).entries.contains_key(rpc_id)
    }

    /// First valid browser answer wins. Invalid or wrong-session data never claims the request.
    pub fn respond_answer(
        &self,
        rpc_id: &str,
        session_id: &SessionId,
        answer: AskUserQuestionAnswer,
    ) -> RpcReceipt {
        let sender = {
            let mut pending = lock(&self.inner.pending);
            let Some(entry) = pending.entries.get(rpc_id) else {
                return RpcReceipt::not_pending();
            };
            if entry.requested.session_id != *session_id
                || !matches_answer(&answer, &entry.requested.questions)
            {
                return RpcReceipt::bad_response();
            }
            let entry = pending
                .entries
                .remove(rpc_id)
                .expect("pending entry exists");
            pending.order.retain(|id| id != rpc_id);
            entry.sender
        };
        if sender.send(QuestionReply::Answered(answer)).is_err() {
            return RpcReceipt::not_pending();
        }
        RpcReceipt::accepted()
    }

    /// A cancellation has no session payload in the canonical browser response;
    /// its opaque rpc id remains the capability that authorizes the cancellation.
    pub fn respond_cancelled(&self, rpc_id: &str) -> RpcReceipt {
        let sender = {
            let mut pending = lock(&self.inner.pending);
            let Some(entry) = pending.entries.remove(rpc_id) else {
                return RpcReceipt::not_pending();
            };
            pending.order.retain(|id| id != rpc_id);
            entry.sender
        };
        if sender.send(QuestionReply::Cancelled).is_err() {
            return RpcReceipt::not_pending();
        }
        RpcReceipt::accepted()
    }

    /// Cancels all unanswered waits for one session. The owning tool appends the
    /// durable cancellation before it returns.
    pub fn cancel_session(&self, session_id: &SessionId) {
        self.cancel_where(|entry| entry.requested.session_id == *session_id);
    }

    /// Cancels only waits owned by a completed turn.
    pub fn cancel_turn(&self, session_id: &SessionId, turn: u64) {
        self.cancel_where(|entry| entry.requested.session_id == *session_id && entry.turn == turn);
    }

    pub fn cancel_all(&self) {
        self.cancel_where(|_| true);
    }

    pub(crate) async fn ask(
        &self,
        context: ToolRunContext,
        questions: Vec<AskUserQuestionItem>,
    ) -> Result<AskUserQuestionAnswer, TessivumError> {
        if questions.is_empty() {
            return Err(question_error(
                "EMPTY_QUESTIONS",
                "ask_user_question requires at least one question",
                Value::Null,
            ));
        }
        if let Some(error) = validate_intents(&questions) {
            return Err(error);
        }
        if context.cancellation.is_cancelled() {
            return Err(question_error(
                "ASK_ABORTED",
                "ask_user_question was aborted before the user answered",
                Value::Null,
            ));
        }
        let (authority, session, turn) = {
            let slots = lock(&self.inner.slots);
            let Some(slot) = slots.get(&context.session) else {
                return Err(question_error(
                    "ASK_MISSING_AGENT",
                    "web user interaction requires an agent-owned session",
                    Value::Null,
                ));
            };
            if !slot.authority.is_live() {
                return Err(question_error(
                    "CALLER_NOT_LIVE",
                    "human interaction requires the exact live calling agent",
                    Value::Null,
                ));
            }
            let Some(turn) = active_turn(&slot.session) else {
                return Err(question_error(
                    "NO_ACTIVE_TURN",
                    "ask_user_question requires an active turn",
                    Value::Null,
                ));
            };
            (slot.authority.clone(), Arc::clone(&slot.session), turn)
        };
        let rpc_id = uuid::Uuid::new_v4().to_string();
        let requested = QuestionRequested {
            rpc_id: rpc_id.clone(),
            session_id: context.session.clone(),
            questions: questions.clone(),
        };
        let (sender, receiver) = oneshot::channel();
        {
            let slots = lock(&self.inner.slots);
            let Some(slot) = slots.get(&context.session) else {
                return Err(question_error(
                    "ASK_MISSING_AGENT",
                    "web user interaction requires an agent-owned session",
                    Value::Null,
                ));
            };
            if !same_authority(&slot.authority, &authority)
                || !authority.is_live()
                || active_turn(&session) != Some(turn)
            {
                return Err(question_error(
                    "CALLER_NOT_LIVE",
                    "human interaction requires the exact live calling agent",
                    Value::Null,
                ));
            }
            let mut pending = lock(&self.inner.pending);
            pending.order.push_back(rpc_id.clone());
            pending.entries.insert(
                rpc_id.clone(),
                PendingQuestion {
                    requested: requested.clone(),
                    authority,
                    turn,
                    sender,
                },
            );
        }
        let asked = QuestionAsked {
            rpc_id: rpc_id.clone(),
            session_id: context.session.clone(),
            turn,
            call_id: context.call,
            questions,
        };
        if let Err(error) = append(
            &session,
            "question/asked",
            serde_json::to_value(&asked).expect("question request is serializable"),
            context.cancellation.clone(),
        )
        .await
        {
            self.remove_pending(&rpc_id);
            return Err(question_error(
                "QUESTION_PERSISTENCE_FAILED",
                "could not persist ask_user_question",
                json!({"cause": error.to_string()}),
            ));
        }
        let _ = self
            .inner
            .notices
            .send(QuestionNotification::Requested(requested));

        let reply = tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => QuestionReply::Cancelled,
            reply = receiver => reply.unwrap_or(QuestionReply::Cancelled),
        };
        let (outcome, answer) = match reply {
            QuestionReply::Answered(answer)
                if !context.cancellation.is_cancelled()
                    && active_turn(&session) == Some(turn)
                    && self.is_live_owner(&context.session) =>
            {
                (QuestionOutcome::Answered, Some(answer))
            }
            QuestionReply::Answered(_) | QuestionReply::Cancelled => {
                (QuestionOutcome::Cancelled, None)
            }
        };
        let resolved = QuestionResolved {
            rpc_id: rpc_id.clone(),
            turn,
            outcome,
            answer: answer.clone(),
        };
        let finalization = ContextHandle::root().scope().cancellation();
        if let Err(error) = append(
            &session,
            "question/resolved",
            serde_json::to_value(&resolved).expect("question resolution is serializable"),
            finalization,
        )
        .await
        {
            self.remove_pending(&rpc_id);
            return Err(question_error(
                "QUESTION_PERSISTENCE_FAILED",
                "could not persist ask_user_question resolution",
                json!({"cause": error.to_string()}),
            ));
        }
        self.remove_pending(&rpc_id);
        let _ = self
            .inner
            .notices
            .send(QuestionNotification::Resolved(QuestionResolvedNotice {
                rpc_id,
                session_id: context.session,
                outcome,
            }));
        answer.ok_or_else(|| {
            question_error(
                "ASK_CANCELLED",
                "the user cancelled ask_user_question",
                Value::Null,
            )
        })
    }

    fn is_live_owner(&self, session_id: &SessionId) -> bool {
        lock(&self.inner.slots)
            .get(session_id)
            .is_some_and(|slot| slot.authority.is_live())
    }

    fn remove_pending(&self, rpc_id: &str) {
        let mut pending = lock(&self.inner.pending);
        pending.entries.remove(rpc_id);
        pending.order.retain(|id| id != rpc_id);
    }

    fn cancel_where(&self, matches: impl Fn(&PendingQuestion) -> bool) {
        let senders = {
            let mut pending = lock(&self.inner.pending);
            let ids = pending
                .order
                .iter()
                .filter(|rpc_id| pending.entries.get(*rpc_id).is_some_and(&matches))
                .cloned()
                .collect::<Vec<_>>();
            let mut senders = Vec::with_capacity(ids.len());
            for rpc_id in ids {
                if let Some(entry) = pending.entries.remove(&rpc_id) {
                    senders.push(entry.sender);
                }
            }
            let keep = pending
                .entries
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            pending.order.retain(|rpc_id| keep.contains(rpc_id));
            senders
        };
        for sender in senders {
            let _ = sender.send(QuestionReply::Cancelled);
        }
    }
}

/// Lifetime owner of one exact session slot.
pub struct HostQuestionRegistration {
    inner: Weak<HostQuestionRegistryInner>,
    session_id: SessionId,
    authority: AgentAuthority,
    closed: AtomicBool,
}

impl HostQuestionRegistration {
    pub fn close(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let removed = {
            let mut slots = lock(&inner.slots);
            if slots
                .get(&self.session_id)
                .is_some_and(|slot| same_authority(&slot.authority, &self.authority))
            {
                slots.remove(&self.session_id);
                true
            } else {
                false
            }
        };
        if removed {
            HostQuestionRegistry { inner }
                .cancel_where(|entry| same_authority(&entry.authority, &self.authority));
        }
        removed
    }
}

impl Drop for HostQuestionRegistration {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

enum QuestionReply {
    Answered(AskUserQuestionAnswer),
    Cancelled,
}

struct QuestionTool {
    questions: HostQuestionRegistry,
}

#[async_trait]
impl ToolHandler for QuestionTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let arguments: AskUserQuestionToolArguments =
            serde_json::from_value(arguments).map_err(|error| {
                question_error(
                    "INVALID_TOOL_ARGUMENTS",
                    "ask_user_question arguments are invalid",
                    json!({"cause": error.to_string()}),
                )
            })?;
        let answer = self.questions.ask(context, arguments.questions).await?;
        let text = serde_json::to_string(&answer).map_err(|error| {
            question_error(
                "QUESTION_SERIALIZATION_FAILED",
                "could not serialize ask_user_question answer",
                json!({"cause": error.to_string()}),
            )
        })?;
        Ok(ToolOutput::new(
            vec![crate::ContentBlock::Text { text }],
            false,
            Value::Null,
        ))
    }
}

#[derive(Deserialize)]
struct AskUserQuestionToolArguments {
    questions: Vec<AskUserQuestionItem>,
}

/// Registers the model-facing tool for the host's browser question provider.
pub fn register_ask_user_question_tool(
    tools: &ToolRuntime,
    questions: HostQuestionRegistry,
) -> Result<ToolRegistration, TessivumError> {
    tools.register(ToolDefinition::new(
        "ask_user_question",
        ASK_USER_QUESTION_DESCRIPTION,
        ask_user_question_schema(),
        QuestionTool { questions },
    ))
}

fn ask_user_question_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": true,
                    "properties": {
                        "id": {"type": "string"},
                        "question": {"type": "string"},
                        "detail": {"type": "string"},
                        "intent": {
                            "type": "object",
                            "properties": {
                                "kind": {"type": "string", "enum": ["plan-review"]},
                                "approve": {"type": "string"}
                            },
                            "required": ["kind", "approve"]
                        },
                        "header": {"type": "string"},
                        "options": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": true,
                                "properties": {
                                    "label": {"type": "string"},
                                    "description": {"type": "string"}
                                },
                                "required": ["label"]
                            }
                        },
                        "multi_select": {"type": "boolean"}
                    },
                    "required": ["id", "question"]
                }
            }
        },
        "required": ["questions"]
    })
}

fn validate_intents(questions: &[AskUserQuestionItem]) -> Option<TessivumError> {
    for question in questions {
        let Some(AskUserQuestionIntent::PlanReview { approve }) = &question.intent else {
            continue;
        };
        if !question
            .options
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|option| option.label == *approve)
            || question.detail.is_none()
        {
            return Some(question_error(
                "BAD_INTENT",
                "question intent must name one of its options and include detail",
                json!({"questionId": question.id}),
            ));
        }
    }
    None
}

fn matches_answer(answer: &AskUserQuestionAnswer, questions: &[AskUserQuestionItem]) -> bool {
    answer.answers.len() == questions.len()
        && answer
            .answers
            .iter()
            .zip(questions)
            .all(|(answer, question)| {
                answer.id == question.id
                    && answer
                        .selected
                        .iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        == answer.selected.len()
                    && answer
                        .custom
                        .as_deref()
                        .is_none_or(|custom| !custom.trim().is_empty())
                    && (question.multi_select == Some(true)
                        || (answer.selected.len() <= 1
                            && (answer.custom.is_none() || answer.selected.is_empty())))
                    && answer.selected.iter().all(|selected| {
                        question
                            .options
                            .as_deref()
                            .unwrap_or_default()
                            .iter()
                            .any(|option| option.label == *selected)
                    })
            })
}

fn active_turn(session: &Session) -> Option<u64> {
    let mut active = None;
    for event in session.events() {
        match event.event_type.as_str() {
            "turn/start" => active = event.data.get("turn").and_then(Value::as_u64),
            "turn/end" => active = None,
            _ => {}
        }
    }
    active
}

async fn append(
    session: &Session,
    event_type: &str,
    data: Value,
    cancellation: CancellationToken,
) -> Result<(), crate::session::SessionError> {
    session
        .append(
            SessionEvent {
                event_type: event_type.into(),
                seq: session.next_seq()?,
                time: 0,
                data,
                ignorable: Some(true),
                source_event_seqs: None,
                surface_op: None,
            },
            cancellation,
        )
        .await
}

fn question_error(code: &str, message: &str, details: Value) -> TessivumError {
    TessivumError::new(code, message, "question", details)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

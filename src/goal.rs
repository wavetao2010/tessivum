//! Durable compare-and-swap goals owned by one live agent.

use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::{
    agent::AgentHandle,
    session::{Session, SessionError},
    tools::{ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRunContext},
    ContentBlock, SessionEvent, SessionId, TessivumError,
};

/// Stable key for the agent-owned goals service.
pub fn goals_service_key() -> ServiceKey {
    ServiceKey::new("harness.goals", "1")
}

/// The optimistic-concurrency identity of one versioned goal.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalRef {
    pub id: String,
    pub revision: u64,
}

impl GoalRef {
    pub fn validate(&self) -> Result<(), GoalError> {
        if self.id.trim().is_empty() || self.id.len() > 256 {
            return Err(GoalError::Invalid(
                "goal id must be 1 through 256 non-whitespace bytes",
            ));
        }
        if self.revision == 0 {
            return Err(GoalError::Invalid("goal revision must be positive"));
        }
        Ok(())
    }
}

/// The portable lifecycle of a goal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalPhase {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl GoalPhase {
    fn permits(self, next: Self) -> bool {
        !matches!(self, Self::Complete) || next == Self::Complete
    }
}

/// A whole, revisioned goal snapshot. Partial updates are deliberately absent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalSnapshot {
    pub reference: GoalRef,
    pub phase: GoalPhase,
    pub title: String,
    #[serde(default)]
    pub tombstone: bool,
}

impl GoalSnapshot {
    pub fn validate(&self) -> Result<(), GoalError> {
        self.reference.validate()?;
        if self.title.trim().is_empty() || self.title.len() > 4_096 {
            return Err(GoalError::Invalid(
                "goal title must be 1 through 4096 non-whitespace bytes",
            ));
        }
        Ok(())
    }
}

/// The durable, audit-visible whole change event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalChange {
    pub snapshot: GoalSnapshot,
    pub agent_id: SessionId,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum GoalError {
    #[error("invalid goal: {0}")]
    Invalid(&'static str),
    #[error("goal {0:?} was not found")]
    NotFound(String),
    #[error("goal compare-and-swap reference is stale")]
    Stale,
    #[error("a tombstoned goal cannot be changed")]
    Tombstoned,
    #[error("a complete goal cannot leave the complete phase")]
    InvalidTransition,
    #[error("goal activation is invalid or has been disarmed")]
    Inactive,
    #[error("goal user rounds must be consecutive attributed user messages")]
    InvalidRound,
    #[error("goal user-round cap was reached")]
    RoundCap,
    #[error("goal service is not owned by its live agent")]
    NotLive,
    #[error("goal operation was cancelled")]
    Cancelled,
    #[error(transparent)]
    Session(#[from] SessionError),
}

impl GoalError {
    fn tool_error(&self) -> TessivumError {
        let code = match self {
            Self::Invalid(_) => "INVALID_GOAL",
            Self::NotFound(_) => "GOAL_NOT_FOUND",
            Self::Stale => "STALE_GOAL_REF",
            Self::Tombstoned => "GOAL_TOMBSTONED",
            Self::InvalidTransition => "INVALID_GOAL_TRANSITION",
            Self::Inactive => "INACTIVE_GOAL",
            Self::InvalidRound => "INVALID_GOAL_ROUND",
            Self::RoundCap => "GOAL_ROUND_CAP",
            Self::NotLive => "GOAL_NOT_LIVE",
            Self::Cancelled => "CANCELLED",
            Self::Session(_) => "GOAL_EVENT_APPEND_FAILED",
        };
        TessivumError::new(code, self.to_string(), "goals", Value::Null)
    }
}

#[derive(Default)]
struct GoalState {
    goals: BTreeMap<String, GoalSnapshot>,
    next_activation: u64,
    activation: Option<Activation>,
}

#[derive(Clone)]
struct Activation {
    id: u64,
    reference: GoalRef,
    max_rounds: u64,
    rounds: u64,
    first_event_seq: u64,
    last_user_event_seq: Option<u64>,
}

struct GoalInner {
    agent: AgentHandle,
    session: Arc<Session>,
    state: Mutex<GoalState>,
}

/// A session-owned goal ledger. Its tool can only be used by its exact agent session.
#[derive(Clone)]
pub struct GoalService {
    inner: Arc<GoalInner>,
}

impl GoalService {
    /// Restores the latest valid whole snapshots from this agent's durable history.
    pub fn new(agent: AgentHandle) -> Result<Self, GoalError> {
        let session = agent.session();
        if agent.id() != session.id() || agent.is_disposed() {
            return Err(GoalError::NotLive);
        }
        let mut state = GoalState::default();
        for event in session.events() {
            if event.event_type != "goal/change" {
                continue;
            }
            let change: GoalChange = serde_json::from_value(event.data)
                .map_err(|_| GoalError::Invalid("durable goal/change payload is invalid"))?;
            if change.agent_id != session.id() {
                return Err(GoalError::NotLive);
            }
            apply_snapshot(&mut state.goals, None, change.snapshot, true)?;
        }
        Ok(Self {
            inner: Arc::new(GoalInner {
                agent,
                session,
                state: Mutex::new(state),
            }),
        })
    }

    /// Publishes this service under `harness.goals@1` for the owning scope lifetime.
    pub fn publish(&self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(goals_service_key(), self.clone())
    }

    pub fn agent_id(&self) -> SessionId {
        self.inner.agent.id()
    }

    pub async fn snapshot(&self, id: &str) -> Option<GoalSnapshot> {
        self.inner.state.lock().await.goals.get(id).cloned()
    }

    pub async fn snapshots(&self) -> Vec<GoalSnapshot> {
        self.inner
            .state
            .lock()
            .await
            .goals
            .values()
            .cloned()
            .collect()
    }

    /// Applies a full snapshot only when `expected` is the current exact reference.
    pub async fn write(
        &self,
        expected: Option<GoalRef>,
        snapshot: GoalSnapshot,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        snapshot.validate()?;
        let mut state = self.inner.state.lock().await;
        check_cancellation(&cancellation)?;
        let mut candidate = state.goals.clone();
        apply_snapshot(&mut candidate, expected.as_ref(), snapshot.clone(), false)?;
        let change = GoalChange {
            snapshot: snapshot.clone(),
            agent_id: self.agent_id(),
        };
        append(
            &self.inner.session,
            "goal/change",
            serde_json::to_value(change).expect("goal change is serializable"),
            cancellation,
        )
        .await?;
        state.goals = candidate;
        Ok(snapshot)
    }

    /// Arms a separate bounded user-round tracker; it does not alter the goal phase.
    pub async fn activate(
        &self,
        reference: GoalRef,
        max_rounds: u64,
        cancellation: CancellationToken,
    ) -> Result<GoalActivation, GoalError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        if !(1..=64).contains(&max_rounds) {
            return Err(GoalError::Invalid(
                "goal round cap must be between 1 and 64",
            ));
        }
        let mut state = self.inner.state.lock().await;
        let Some(snapshot) = state.goals.get(&reference.id) else {
            return Err(GoalError::NotFound(reference.id));
        };
        if snapshot.reference != reference {
            return Err(GoalError::Stale);
        }
        if snapshot.tombstone || snapshot.phase != GoalPhase::Active {
            return Err(GoalError::Inactive);
        }
        state.next_activation = state.next_activation.checked_add(1).unwrap_or(1);
        let id = state.next_activation;
        let first_event_seq = self.inner.session.next_seq()?;
        state.activation = Some(Activation {
            id,
            reference,
            max_rounds,
            rounds: 0,
            first_event_seq,
            last_user_event_seq: None,
        });
        Ok(GoalActivation {
            inner: Arc::downgrade(&self.inner),
            id,
        })
    }

    pub fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "goal",
            "Writes one complete goal snapshot using its exact previous goal reference.",
            goal_schema(),
            GoalTool {
                goals: self.clone(),
            },
        )
    }

    fn require_live(&self) -> Result<(), GoalError> {
        if self.inner.agent.is_disposed() || self.inner.agent.id() != self.inner.session.id() {
            Err(GoalError::NotLive)
        } else {
            Ok(())
        }
    }

    async fn write_from_tool(
        &self,
        context: ToolRunContext,
        input: GoalToolInput,
    ) -> Result<ToolOutput, GoalError> {
        if context.session != self.agent_id() {
            return Err(GoalError::NotLive);
        }
        let snapshot = self
            .write(input.expected, input.snapshot, context.cancellation)
            .await?;
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: serde_json::to_string(&snapshot).expect("goal snapshot is serializable"),
            }],
            false,
            Value::Null,
        ))
    }
}

/// A separately armed goal runner. Dropping it does not silently disarm the goal.
pub struct GoalActivation {
    inner: Weak<GoalInner>,
    id: u64,
}

impl GoalActivation {
    /// Admits exactly the next direct user message and never replays an earlier round.
    pub async fn admit_user_round(
        &self,
        user_event_seq: u64,
        cancellation: CancellationToken,
    ) -> Result<u64, GoalError> {
        check_cancellation(&cancellation)?;
        let Some(inner) = self.inner.upgrade() else {
            return Err(GoalError::Inactive);
        };
        if inner.agent.is_disposed() || inner.agent.id() != inner.session.id() {
            return Err(GoalError::NotLive);
        }
        let mut state = inner.state.lock().await;
        let activation = state.activation.clone().ok_or(GoalError::Inactive)?;
        if activation.id != self.id {
            return Err(GoalError::Inactive);
        }
        if activation.rounds >= activation.max_rounds {
            return Err(GoalError::RoundCap);
        }
        let current = state
            .goals
            .get(&activation.reference.id)
            .ok_or_else(|| GoalError::NotFound(activation.reference.id.clone()))?;
        if current.reference != activation.reference
            || current.phase != GoalPhase::Active
            || current.tombstone
        {
            return Err(GoalError::Inactive);
        }
        let after = activation
            .last_user_event_seq
            .unwrap_or_else(|| activation.first_event_seq.saturating_sub(1));
        let expected = inner
            .session
            .events()
            .into_iter()
            .find(|event| {
                event.seq >= activation.first_event_seq
                    && event.seq > after
                    && attributed_user(event)
            })
            .map(|event| event.seq)
            .ok_or(GoalError::InvalidRound)?;
        if expected != user_event_seq {
            return Err(GoalError::InvalidRound);
        }
        let activation = state.activation.as_mut().ok_or(GoalError::Inactive)?;
        if activation.id != self.id {
            return Err(GoalError::Inactive);
        }
        activation.last_user_event_seq = Some(user_event_seq);
        activation.rounds += 1;
        Ok(activation.rounds)
    }

    /// Explicitly disarms this activation without mutating the durable goal.
    pub async fn disarm(&self) -> Result<(), GoalError> {
        let Some(inner) = self.inner.upgrade() else {
            return Ok(());
        };
        let mut state = inner.state.lock().await;
        if state
            .activation
            .as_ref()
            .is_some_and(|activation| activation.id == self.id)
        {
            state.activation = None;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalToolInput {
    expected: Option<GoalRef>,
    snapshot: GoalSnapshot,
}

struct GoalTool {
    goals: GoalService,
}

#[async_trait]
impl ToolHandler for GoalTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let input = serde_json::from_value(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_GOAL",
                "goal tool input is invalid",
                "goals",
                Value::Null,
            )
        })?;
        self.goals
            .write_from_tool(context, input)
            .await
            .map_err(|error| error.tool_error())
    }
}

fn apply_snapshot(
    goals: &mut BTreeMap<String, GoalSnapshot>,
    expected: Option<&GoalRef>,
    snapshot: GoalSnapshot,
    replay: bool,
) -> Result<(), GoalError> {
    snapshot.validate()?;
    let id = snapshot.reference.id.clone();
    match goals.get(&id) {
        None => {
            if expected.is_some() || snapshot.reference.revision != 1 || snapshot.tombstone {
                return Err(GoalError::Stale);
            }
        }
        Some(current) => {
            if current.tombstone {
                return Err(GoalError::Tombstoned);
            }
            let Some(expected) = expected.or_else(|| replay.then_some(&current.reference)) else {
                return Err(GoalError::Stale);
            };
            if expected != &current.reference
                || snapshot.reference.revision != current.reference.revision.saturating_add(1)
            {
                return Err(GoalError::Stale);
            }
            if !current.phase.permits(snapshot.phase) {
                return Err(GoalError::InvalidTransition);
            }
        }
    }
    goals.insert(id, snapshot);
    Ok(())
}

fn attributed_user(event: &SessionEvent) -> bool {
    event.event_type == "user/message"
        && event.data.pointer("/source/kind").and_then(Value::as_str) == Some("user")
}

async fn append(
    session: &Session,
    event_type: &str,
    data: Value,
    cancellation: CancellationToken,
) -> Result<(), GoalError> {
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

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), GoalError> {
    if cancellation.is_cancelled() {
        Err(GoalError::Cancelled)
    } else {
        Ok(())
    }
}

fn goal_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["expected", "snapshot"],
        "properties": {
            "expected": {"oneOf": [
                {"type": "null"},
                {"type": "object", "additionalProperties": false, "required": ["id", "revision"], "properties": {
                    "id": {"type": "string"}, "revision": {"type": "integer"}
                }}
            ]},
            "snapshot": {"type": "object", "additionalProperties": false, "required": ["reference", "phase", "title", "tombstone"], "properties": {
                "reference": {"type": "object", "additionalProperties": false, "required": ["id", "revision"], "properties": {
                    "id": {"type": "string"}, "revision": {"type": "integer"}
                }},
                "phase": {"type": "string", "enum": ["active", "paused", "blocked", "complete"]},
                "title": {"type": "string"},
                "tombstone": {"type": "boolean"}
            }}
        }
    })
}

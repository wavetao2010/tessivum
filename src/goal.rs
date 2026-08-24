//! Durable compare-and-swap goals owned by one live agent.

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    agent::{AgentHandle, AgentStatus},
    protocol::{Message, MessageId, MessageRole, MessageSource},
    session::{Session, SessionError},
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRegistration,
        ToolRunContext, ToolRuntime,
    },
    ContentBlock, SessionEvent, SessionId, TessivumError, MAX_SAFE_INTEGER,
};

/// Stable key for the agent-owned goals service.
pub fn goals_service_key() -> ServiceKey {
    ServiceKey::new("harness.goals", "1")
}

pub const DEFAULT_MAX_GOAL_ROUNDS: u64 = 256;

fn default_max_goal_rounds() -> u64 {
    DEFAULT_MAX_GOAL_ROUNDS
}

/// The optimistic-concurrency identity of one versioned goal.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
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

/// A stable policy reason retained whenever a goal becomes blocked.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoalBlockReason {
    pub code: String,
    pub message: String,
}

impl GoalBlockReason {
    fn validate(&self) -> Result<(), GoalError> {
        let valid_code = !self.code.is_empty()
            && self.code.split('-').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            });
        if !valid_code || self.message.trim().is_empty() {
            return Err(GoalError::Invalid(
                "goal block reason requires a lower-kebab-case code and non-empty message",
            ));
        }
        Ok(())
    }

    fn normalized(mut self) -> Result<Self, GoalError> {
        self.message = self.message.trim().into();
        self.validate()?;
        Ok(self)
    }
}

/// A whole, revisioned goal snapshot. Partial updates are deliberately absent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct GoalSnapshot {
    pub reference: GoalRef,
    pub phase: GoalPhase,
    pub title: String,
    #[serde(default = "default_max_goal_rounds")]
    pub max_goal_rounds: u64,
    #[serde(default)]
    pub tombstone: bool,
}

impl GoalSnapshot {
    pub fn validate(&self) -> Result<(), GoalError> {
        self.reference.validate()?;
        if self.title.trim().is_empty() {
            return Err(GoalError::Invalid("goal title must be non-whitespace"));
        }
        if !(1..=MAX_SAFE_INTEGER).contains(&self.max_goal_rounds) {
            return Err(GoalError::Invalid(
                "goal round cap must be a positive safe integer",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoalSnapshotWire {
    id: String,
    revision: u64,
    objective: String,
    phase: GoalPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocked_reason: Option<GoalBlockReason>,
    max_goal_rounds: u64,
}

impl GoalSnapshotWire {
    fn from_snapshot(snapshot: GoalSnapshot, blocked_reason: Option<GoalBlockReason>) -> Self {
        Self {
            id: snapshot.reference.id,
            revision: snapshot.reference.revision,
            objective: snapshot.title,
            phase: snapshot.phase,
            blocked_reason,
            max_goal_rounds: snapshot.max_goal_rounds,
        }
    }

    fn snapshot(self) -> Result<(GoalSnapshot, Option<GoalBlockReason>), GoalError> {
        let blocked_reason = self.blocked_reason;
        match (self.phase, blocked_reason.as_ref()) {
            (GoalPhase::Blocked, Some(reason)) => reason.validate()?,
            (GoalPhase::Blocked, None) => {
                return Err(GoalError::Invalid("blocked goal lacks blockedReason"))
            }
            (_, Some(_)) => return Err(GoalError::Invalid("non-blocked goal has blockedReason")),
            (_, None) => {}
        }
        let snapshot = GoalSnapshot {
            reference: GoalRef {
                id: self.id,
                revision: self.revision,
            },
            phase: self.phase,
            title: self.objective,
            max_goal_rounds: self.max_goal_rounds,
            tombstone: false,
        };
        snapshot.validate()?;
        Ok((snapshot, blocked_reason))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum GoalOperation {
    Create,
    Edit,
    Pause,
    Resume,
    Complete,
    Block,
    Clear,
}

/// The durable, audit-visible whole change event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
struct GoalChange {
    kind: String,
    version: u8,
    operation: GoalOperation,
    #[serde(skip_serializing_if = "Option::is_none")]
    goal: Option<GoalSnapshotWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rounds_started: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleared: Option<GoalRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleared_at: Option<u64>,
}

impl GoalChange {
    fn snapshot(
        operation: GoalOperation,
        snapshot: GoalSnapshot,
        times: GoalTimes,
        blocked_reason: Option<GoalBlockReason>,
    ) -> Self {
        Self {
            kind: "goal/change".into(),
            version: 1,
            operation,
            goal: Some(GoalSnapshotWire::from_snapshot(snapshot, blocked_reason)),
            rounds_started: Some(times.rounds_started),
            created_at: Some(times.created_at),
            updated_at: Some(times.updated_at),
            cleared: None,
            cleared_at: None,
        }
    }

    fn clear(cleared: GoalRef, cleared_at: u64) -> Self {
        Self {
            kind: "goal/change".into(),
            version: 1,
            operation: GoalOperation::Clear,
            goal: None,
            rounds_started: None,
            created_at: None,
            updated_at: None,
            cleared: Some(cleared),
            cleared_at: Some(cleared_at),
        }
    }

    fn validate_shape(&self) -> Result<(), GoalError> {
        if self.kind != "goal/change" || self.version != 1 {
            return Err(GoalError::Invalid("durable goal/change payload is invalid"));
        }
        if self.operation == GoalOperation::Clear {
            let Some(cleared) = &self.cleared else {
                return Err(GoalError::Invalid("durable goal clear lacks ref"));
            };
            if self.goal.is_some()
                || self.rounds_started.is_some()
                || self.created_at.is_some()
                || self.updated_at.is_some()
                || self.cleared_at.is_none()
                || self.cleared_at.is_some_and(|time| time > MAX_SAFE_INTEGER)
            {
                return Err(GoalError::Invalid("durable goal clear payload is invalid"));
            }
            return cleared.validate();
        }
        let (Some(rounds_started), Some(created_at), Some(updated_at)) =
            (self.rounds_started, self.created_at, self.updated_at)
        else {
            return Err(GoalError::Invalid(
                "durable goal change lacks counters or timestamps",
            ));
        };
        if self.goal.is_none()
            || self.cleared.is_some()
            || self.cleared_at.is_some()
            || rounds_started > MAX_SAFE_INTEGER
            || created_at > MAX_SAFE_INTEGER
            || updated_at > MAX_SAFE_INTEGER
            || updated_at < created_at
        {
            return Err(GoalError::Invalid("durable goal change payload is invalid"));
        }
        if let Some(goal) = &self.goal {
            match (goal.phase, goal.blocked_reason.as_ref()) {
                (GoalPhase::Blocked, Some(reason)) => reason.validate()?,
                (GoalPhase::Blocked, None) => {
                    return Err(GoalError::Invalid("blocked goal lacks blockedReason"))
                }
                (_, Some(_)) => {
                    return Err(GoalError::Invalid("non-blocked goal has blockedReason"))
                }
                (_, None) => {}
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum GoalError {
    #[error("invalid goal: {0}")]
    Invalid(&'static str),
    #[error("an active goal already exists")]
    AlreadyExists,
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
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "INVALID_GOAL",
            Self::AlreadyExists => "GOAL_ALREADY_EXISTS",
            Self::NotFound(_) => "GOAL_NOT_FOUND",
            Self::Stale => "GOAL_STALE_REVISION",
            Self::Tombstoned => "GOAL_NOT_FOUND",
            Self::InvalidTransition => "GOAL_INVALID_TRANSITION",
            Self::Inactive => "INACTIVE_GOAL",
            Self::InvalidRound => "INVALID_GOAL_ROUND",
            Self::RoundCap => "GOAL_ROUND_CAP",
            Self::NotLive => "GOAL_AGENT_NOT_LIVE",
            Self::Cancelled => "CANCELLED",
            Self::Session(_) => "GOAL_EVENT_APPEND_FAILED",
        }
    }

    fn tool_error(&self) -> TessivumError {
        TessivumError::new(self.code(), self.to_string(), "goals", Value::Null)
    }
}

#[derive(Clone, Copy)]
struct GoalTimes {
    created_at: u64,
    updated_at: u64,
    rounds_started: u64,
}

#[derive(Clone, Default)]
struct GoalState {
    goals: BTreeMap<String, GoalSnapshot>,
    blocked_reasons: BTreeMap<String, GoalBlockReason>,
    times: BTreeMap<String, GoalTimes>,
    next_activation: u64,
    activation: Option<Activation>,
    observed_events: usize,
}

#[derive(Clone)]
struct Activation {
    id: u64,
    reference: GoalRef,
    max_rounds: u64,
    rounds: u64,
    first_event_seq: u64,
    last_user_event_seq: Option<u64>,
    queued_round: Option<u64>,
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
    pub fn new(agent: AgentHandle) -> Result<Self, GoalError> {
        let session = agent.session();
        if agent.id() != session.id() || agent.is_disposed() {
            return Err(GoalError::NotLive);
        }
        let mut state = GoalState::default();
        let events = session.events();
        for event in &events {
            if event.event_type == "goal/change" {
                let change: GoalChange = serde_json::from_value(event.data.clone())
                    .map_err(|_| GoalError::Invalid("durable goal/change payload is invalid"))?;
                change.validate_shape()?;
                match change.operation {
                    GoalOperation::Clear => {
                        let reference = change
                            .cleared
                            .ok_or(GoalError::Invalid("durable goal clear lacks ref"))?;
                        let current = state
                            .goals
                            .get(&reference.id)
                            .cloned()
                            .ok_or_else(|| GoalError::NotFound(reference.id.clone()))?;
                        let snapshot = GoalSnapshot {
                            reference,
                            phase: current.phase,
                            title: current.title,
                            max_goal_rounds: current.max_goal_rounds,
                            tombstone: true,
                        };
                        validate_goal_operation(&state.goals, GoalOperation::Clear, &snapshot)?;
                        apply_snapshot(&mut state.goals, None, snapshot.clone(), true)?;
                        state.times.remove(&snapshot.reference.id);
                        state.blocked_reasons.remove(&snapshot.reference.id);
                    }
                    _ => {
                        let (snapshot, blocked_reason) = change
                            .goal
                            .ok_or(GoalError::Invalid("durable goal change lacks goal"))?
                            .snapshot()?;
                        let id = snapshot.reference.id.clone();
                        let times = GoalTimes {
                            created_at: change
                                .created_at
                                .ok_or(GoalError::Invalid("goal change lacks createdAt"))?,
                            updated_at: change
                                .updated_at
                                .ok_or(GoalError::Invalid("goal change lacks updatedAt"))?,
                            rounds_started: change
                                .rounds_started
                                .ok_or(GoalError::Invalid("goal change lacks roundsStarted"))?,
                        };
                        validate_goal_operation(&state.goals, change.operation, &snapshot)?;
                        validate_goal_times(&state.times, change.operation, &snapshot, times)?;
                        apply_snapshot(&mut state.goals, None, snapshot.clone(), true)?;
                        if let Some(reason) = blocked_reason {
                            state.blocked_reasons.insert(id.clone(), reason);
                        } else {
                            state.blocked_reasons.remove(&id);
                        }
                        state.times.insert(id, times);
                    }
                }
            }
            apply_goal_round(&mut state, event)?;
        }
        state.observed_events = events.len();
        if let Some(goal) = state
            .goals
            .values()
            .find(|goal| !goal.tombstone && goal.phase == GoalPhase::Active)
            .cloned()
        {
            let rounds = state
                .times
                .get(&goal.reference.id)
                .map_or(0, |times| times.rounds_started);
            state.next_activation = 1;
            state.activation = Some(Activation {
                id: 1,
                reference: goal.reference,
                max_rounds: goal.max_goal_rounds,
                rounds,
                first_event_seq: 1,
                last_user_event_seq: None,
                queued_round: None,
            });
        }
        Ok(Self {
            inner: Arc::new(GoalInner {
                agent,
                session,
                state: Mutex::new(state),
            }),
        })
    }

    pub fn publish(&self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(goals_service_key(), self.clone())
    }

    pub fn agent_id(&self) -> SessionId {
        self.inner.agent.id()
    }

    pub async fn snapshot(&self, id: &str) -> Option<GoalSnapshot> {
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state).ok()?;
        state.goals.get(id).cloned()
    }

    pub async fn snapshots(&self) -> Vec<GoalSnapshot> {
        let mut state = self.inner.state.lock().await;
        if self.sync_locked(&mut state).is_err() {
            return Vec::new();
        }
        state.goals.values().cloned().collect()
    }

    /// Returns the current durable goal pointer, including a completed goal until cleared.
    pub async fn current(&self) -> Option<GoalSnapshot> {
        let events = self.inner.session.events();
        for event in events.into_iter().rev() {
            if event.event_type != "goal/change" {
                continue;
            }
            let change: GoalChange = serde_json::from_value(event.data).ok()?;
            return match change.operation {
                GoalOperation::Clear => None,
                _ => change.goal?.snapshot().ok().map(|(snapshot, _)| snapshot),
            };
        }
        None
    }

    /// Returns the upstream-shaped current goal projection folded through admitted rounds.
    pub async fn projection(&self) -> Option<Value> {
        let current = self.current().await?;
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state).ok()?;
        let times = state.times.get(&current.reference.id)?;
        let reason = state.blocked_reasons.get(&current.reference.id).cloned();
        let goal = GoalSnapshotWire::from_snapshot(current, reason);
        Some(json!({
            "goal": goal,
            "roundsStarted": times.rounds_started,
            "createdAt": times.created_at,
            "updatedAt": times.updated_at,
        }))
    }

    /// Renders the model-facing current goal and transient activation observation.
    pub async fn model_value(&self) -> Result<Value, GoalError> {
        let Some(current) = self.current().await else {
            return Ok(json!({"goal": null}));
        };
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state)?;
        let times = state
            .times
            .get(&current.reference.id)
            .ok_or(GoalError::Invalid("goal lacks round counter"))?;
        let blocked_reason = state.blocked_reasons.get(&current.reference.id);
        let activation = state.activation.as_ref().is_some_and(|activation| {
            activation.reference == current.reference
                && current.phase == GoalPhase::Active
                && !current.tombstone
        });
        let mut value = json!({
            "goal": {
                "id": current.reference.id,
                "revision": current.reference.revision,
                "objective": current.title,
                "phase": current.phase,
                "roundsStarted": times.rounds_started,
                "maxGoalRounds": current.max_goal_rounds,
            },
            "activation": if activation { "armed" } else { "disarmed" },
        });
        if let Some(blocked_reason) = blocked_reason {
            value["goal"]["blockedReason"] =
                serde_json::to_value(blocked_reason).expect("goal block reason is serializable");
        }
        Ok(value)
    }
    pub async fn write(
        &self,
        expected: Option<GoalRef>,
        snapshot: GoalSnapshot,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state)?;
        let operation = if snapshot.tombstone {
            GoalOperation::Clear
        } else if snapshot.reference.revision == 1 {
            GoalOperation::Create
        } else if let Some(current) = state.goals.get(&snapshot.reference.id) {
            match (current.phase, snapshot.phase) {
                (GoalPhase::Active, GoalPhase::Paused) => GoalOperation::Pause,
                (
                    GoalPhase::Active | GoalPhase::Paused | GoalPhase::Blocked,
                    GoalPhase::Complete,
                ) => GoalOperation::Complete,
                (GoalPhase::Active, GoalPhase::Blocked) => GoalOperation::Block,
                (GoalPhase::Paused | GoalPhase::Blocked, GoalPhase::Active) => {
                    GoalOperation::Resume
                }
                _ => GoalOperation::Edit,
            }
        } else {
            GoalOperation::Edit
        };
        self.write_locked(&mut state, expected, snapshot, operation, cancellation)
            .await
    }

    pub async fn create(
        &self,
        objective: String,
        max_goal_rounds: Option<u64>,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        let snapshot = GoalSnapshot {
            reference: GoalRef {
                id: format!("goal-{}", Uuid::new_v4()),
                revision: 1,
            },
            phase: GoalPhase::Active,
            title: normalize_objective(objective)?,
            max_goal_rounds: max_goal_rounds.unwrap_or(DEFAULT_MAX_GOAL_ROUNDS),
            tombstone: false,
        };
        snapshot.validate()?;
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state)?;
        if state
            .goals
            .values()
            .any(|goal| !goal.tombstone && goal.phase != GoalPhase::Complete)
        {
            return Err(GoalError::AlreadyExists);
        }
        let snapshot = self
            .write_locked(
                &mut state,
                None,
                snapshot,
                GoalOperation::Create,
                cancellation.clone(),
            )
            .await?;
        check_cancellation(&cancellation)?;
        self.arm_locked(
            &mut state,
            snapshot.reference.clone(),
            snapshot.max_goal_rounds,
        )?;
        Ok(snapshot)
    }

    /// Revises an exact goal while preserving its phase and activation state.
    pub async fn edit(
        &self,
        reference: GoalRef,
        objective: Option<String>,
        max_goal_rounds: Option<u64>,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        if objective.is_none() && max_goal_rounds.is_none() {
            return Err(GoalError::Invalid(
                "goal edit requires objective or maxGoalRounds",
            ));
        }
        self.require_live()?;
        check_cancellation(&cancellation)?;
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state)?;
        let current = self.current_locked(&state, &reference)?.clone();
        let snapshot = GoalSnapshot {
            reference: next_ref(&current.reference)?,
            phase: current.phase,
            title: objective
                .map(normalize_objective)
                .transpose()?
                .unwrap_or(current.title),
            max_goal_rounds: max_goal_rounds.unwrap_or(current.max_goal_rounds),
            tombstone: false,
        };
        let snapshot = self
            .write_locked(
                &mut state,
                Some(reference.clone()),
                snapshot,
                GoalOperation::Edit,
                cancellation,
            )
            .await?;
        if let Some(activation) = state.activation.as_mut() {
            if activation.reference == reference {
                activation.reference = snapshot.reference.clone();
                activation.max_rounds = snapshot.max_goal_rounds;
            }
        }
        Ok(snapshot)
    }

    /// Pauses an active goal and disarms its automatic continuation.
    pub async fn pause(
        &self,
        reference: GoalRef,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        self.transition(
            reference,
            &[GoalPhase::Active],
            GoalPhase::Paused,
            false,
            cancellation,
        )
        .await
    }

    /// Durably blocks an active goal with a policy-owned reason and disarms continuation.
    pub async fn block(
        &self,
        reference: GoalRef,
        reason: GoalBlockReason,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state)?;
        let current = self.current_locked(&state, &reference)?.clone();
        if current.phase != GoalPhase::Active {
            return Err(GoalError::InvalidTransition);
        }
        let snapshot = GoalSnapshot {
            reference: next_ref(&current.reference)?,
            phase: GoalPhase::Blocked,
            title: current.title,
            max_goal_rounds: current.max_goal_rounds,
            tombstone: false,
        };
        let snapshot = self
            .write_locked_with_reason(
                &mut state,
                Some(reference),
                snapshot,
                GoalOperation::Block,
                Some(reason),
                cancellation,
            )
            .await?;
        state.activation = None;
        Ok(snapshot)
    }

    /// Resumes an exact inactive goal, or rearms an active but disarmed goal.
    pub async fn resume(
        &self,
        reference: GoalRef,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state)?;
        let current = self.current_locked(&state, &reference)?.clone();
        if !matches!(
            current.phase,
            GoalPhase::Active | GoalPhase::Paused | GoalPhase::Blocked
        ) || (current.phase == GoalPhase::Active && state.activation.is_some())
        {
            return Err(GoalError::InvalidTransition);
        }
        if state
            .times
            .get(&current.reference.id)
            .is_some_and(|times| times.rounds_started >= current.max_goal_rounds)
        {
            return Err(GoalError::InvalidTransition);
        }
        let snapshot = GoalSnapshot {
            reference: next_ref(&current.reference)?,
            phase: GoalPhase::Active,
            title: current.title,
            max_goal_rounds: current.max_goal_rounds,
            tombstone: false,
        };
        let snapshot = self
            .write_locked(
                &mut state,
                Some(reference),
                snapshot,
                GoalOperation::Resume,
                cancellation,
            )
            .await?;
        self.arm_locked(
            &mut state,
            snapshot.reference.clone(),
            snapshot.max_goal_rounds,
        )?;
        Ok(snapshot)
    }

    /// Completes an exact non-complete goal and disarms its continuation.
    pub async fn complete(
        &self,
        reference: GoalRef,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        self.transition(
            reference,
            &[GoalPhase::Active, GoalPhase::Paused, GoalPhase::Blocked],
            GoalPhase::Complete,
            false,
            cancellation,
        )
        .await
    }

    /// Durably tombstones an exact goal while retaining its history.
    pub async fn clear(
        &self,
        reference: GoalRef,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state)?;
        let current = self.current_locked(&state, &reference)?.clone();
        let snapshot = GoalSnapshot {
            reference: next_ref(&current.reference)?,
            phase: current.phase,
            title: current.title,
            max_goal_rounds: current.max_goal_rounds,
            tombstone: true,
        };
        let snapshot = self
            .write_locked(
                &mut state,
                Some(reference),
                snapshot,
                GoalOperation::Clear,
                cancellation,
            )
            .await?;
        state.activation = None;
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
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state)?;
        self.arm_locked(&mut state, reference, max_rounds)
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
    /// Schedules the next automatic round when this exact live agent is idle.
    /// A queued round is reserved in process-local activation state until its durable user message
    /// is observed; all queue failures become a durable blocker rather than a silent retry loop.
    pub async fn drive(&self) -> Result<(), GoalError> {
        self.require_live()?;
        if self.inner.agent.status() != AgentStatus::Idle {
            return Ok(());
        }
        enum Next {
            Queue {
                reference: GoalRef,
                round: u64,
                objective: String,
                max_rounds: u64,
            },
            Block {
                reference: GoalRef,
                max_rounds: u64,
            },
        }
        let next = {
            let mut state = self.inner.state.lock().await;
            self.sync_locked(&mut state)?;
            let Some(activation) = state.activation.as_ref() else {
                return Ok(());
            };
            if activation.queued_round.is_some() {
                return Ok(());
            }
            let reference = activation.reference.clone();
            let current = state
                .goals
                .get(&reference.id)
                .ok_or_else(|| GoalError::NotFound(reference.id.clone()))?;
            if current.reference != reference
                || current.tombstone
                || current.phase != GoalPhase::Active
            {
                return Ok(());
            }
            let rounds = state
                .times
                .get(&current.reference.id)
                .ok_or(GoalError::Invalid("goal lacks round counter"))?
                .rounds_started;
            if rounds >= current.max_goal_rounds {
                Next::Block {
                    reference: current.reference.clone(),
                    max_rounds: current.max_goal_rounds,
                }
            } else {
                let round = rounds + 1;
                let reference = current.reference.clone();
                let objective = current.title.clone();
                let max_rounds = current.max_goal_rounds;
                state
                    .activation
                    .as_mut()
                    .expect("activation exists")
                    .queued_round = Some(round);
                Next::Queue {
                    reference,
                    round,
                    objective,
                    max_rounds,
                }
            }
        };
        let (reference, round, message) = match next {
            Next::Block {
                reference,
                max_rounds,
            } => {
                self.block(
                    reference,
                    GoalBlockReason {
                        code: "round-limit".into(),
                        message: format!(
                            "Goal reached its configured limit of {max_rounds} rounds."
                        ),
                    },
                    self.cancellation(),
                )
                .await?;
                return Ok(());
            }
            Next::Queue {
                reference,
                round,
                objective,
                max_rounds,
            } => {
                let message = Message {
                    id: MessageId::random(),
                    role: MessageRole::User,
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "<goal_round>\nObjective: {}\nRound: {round}/{max_rounds}\n\nContinue working toward the objective in this same session. Treat the current workspace, tool results, and durable session state as authoritative; inspect them instead of assuming earlier narration is still current. Make concrete progress and verify the result. Before claiming completion, gather evidence that the whole objective is achieved, read the current goal, and mark it complete. If work remains, leave the goal active for the next round. Follow the configured goal-tool policy before reporting a blocker.\n</goal_round>",
                            serde_json::to_string(&objective).expect("goal objective is serializable"),
                        ),
                    }],
                    source: MessageSource::Goal {
                        goal_id: reference.id.clone(),
                        revision: reference.revision,
                        round,
                    },
                };
                (reference, round, message)
            }
        };
        let queued = SessionEvent {
            event_type: "agent/inbox/enqueued".into(),
            seq: self.inner.session.next_seq()?,
            time: now(),
            data: json!({"target": "next-turn", "message": message}),
            ignorable: Some(true),
            source_event_seqs: None,
            surface_op: None,
        };
        if self
            .inner
            .session
            .append(queued, self.cancellation())
            .await
            .is_err()
        {
            self.clear_queued_round(&reference, round).await;
            self.block(
                reference,
                GoalBlockReason {
                    code: "queue-failed".into(),
                    message: format!(
                        "Could not queue goal round {round}: durable queue append failed."
                    ),
                },
                self.cancellation(),
            )
            .await?;
            return Err(GoalError::Invalid("goal continuation queue append failed"));
        }
        if self.inner.agent.followup(message).await.is_err() {
            self.clear_queued_round(&reference, round).await;
            self.block(
                reference,
                GoalBlockReason {
                    code: "queue-failed".into(),
                    message: format!("Could not queue goal round {round}: agent delivery failed."),
                },
                self.cancellation(),
            )
            .await?;
            return Err(GoalError::Invalid(
                "goal continuation queue delivery failed",
            ));
        }
        let service = self.clone();
        tokio::spawn(async move {
            let _ = service.inner.agent.when_idle().await;
            let _ = service.drive_boxed().await;
        });
        Ok(())
    }

    fn drive_boxed(&self) -> Pin<Box<dyn Future<Output = Result<(), GoalError>> + Send + '_>> {
        Box::pin(self.drive())
    }

    async fn clear_queued_round(&self, reference: &GoalRef, round: u64) {
        let mut state = self.inner.state.lock().await;
        if state.activation.as_ref().is_some_and(|activation| {
            activation.reference == *reference && activation.queued_round == Some(round)
        }) {
            state
                .activation
                .as_mut()
                .expect("activation was checked")
                .queued_round = None;
        }
    }

    fn sync_locked(&self, state: &mut GoalState) -> Result<(), GoalError> {
        let events = self.inner.session.events();
        for event in events.iter().skip(state.observed_events) {
            apply_goal_round(state, event)?;
        }
        state.observed_events = events.len();
        Ok(())
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.inner.agent.cancellation()
    }

    fn current_locked<'a>(
        &self,
        state: &'a GoalState,
        reference: &GoalRef,
    ) -> Result<&'a GoalSnapshot, GoalError> {
        let current = state
            .goals
            .get(&reference.id)
            .ok_or_else(|| GoalError::NotFound(reference.id.clone()))?;
        if current.tombstone {
            return Err(GoalError::Tombstoned);
        }
        if current.reference != *reference {
            return Err(GoalError::Stale);
        }
        Ok(current)
    }

    async fn write_locked(
        &self,
        state: &mut GoalState,
        expected: Option<GoalRef>,
        snapshot: GoalSnapshot,
        operation: GoalOperation,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        self.write_locked_with_reason(state, expected, snapshot, operation, None, cancellation)
            .await
    }

    async fn write_locked_with_reason(
        &self,
        state: &mut GoalState,
        expected: Option<GoalRef>,
        snapshot: GoalSnapshot,
        operation: GoalOperation,
        reason: Option<GoalBlockReason>,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        check_cancellation(&cancellation)?;
        snapshot.validate()?;
        let blocked_reason = match snapshot.phase {
            GoalPhase::Blocked => reason
                .or_else(|| state.blocked_reasons.get(&snapshot.reference.id).cloned())
                .ok_or(GoalError::Invalid("blocked goal lacks blockedReason"))?
                .normalized()
                .map(Some)?,
            _ => None,
        };
        loop {
            validate_goal_operation(&state.goals, operation, &snapshot)?;
            let mut candidate = state.goals.clone();
            apply_snapshot(&mut candidate, expected.as_ref(), snapshot.clone(), false)?;
            let old_times = state.times.get(&snapshot.reference.id).copied();
            let updated_at = old_times.map_or_else(now, |times| now().max(times.updated_at));
            let times = GoalTimes {
                created_at: old_times.map_or(updated_at, |times| times.created_at),
                updated_at,
                rounds_started: old_times.map_or(0, |times| times.rounds_started),
            };
            let change = if operation == GoalOperation::Clear {
                GoalChange::clear(snapshot.reference.clone(), updated_at)
            } else {
                GoalChange::snapshot(operation, snapshot.clone(), times, blocked_reason.clone())
            };
            match append(
                &self.inner.session,
                "goal/change",
                serde_json::to_value(change).expect("goal change is serializable"),
                cancellation.clone(),
            )
            .await
            {
                Ok(()) => {
                    state.goals = candidate;
                    if operation == GoalOperation::Clear {
                        state.times.remove(&snapshot.reference.id);
                        state.blocked_reasons.remove(&snapshot.reference.id);
                    } else {
                        state.times.insert(snapshot.reference.id.clone(), times);
                        if let Some(reason) = blocked_reason {
                            state
                                .blocked_reasons
                                .insert(snapshot.reference.id.clone(), reason);
                        } else {
                            state.blocked_reasons.remove(&snapshot.reference.id);
                        }
                    }
                    state.observed_events = state.observed_events.saturating_add(1);
                    return Ok(snapshot);
                }
                Err(GoalError::Session(error @ SessionError::SequenceGap { expected, actual })) => {
                    if actual >= expected || self.inner.session.next_seq()? != expected {
                        return Err(GoalError::Session(error));
                    }
                    // Another session writer won after append sampled next_seq; fold it before rebuilding the event.
                    check_cancellation(&cancellation)?;
                    self.sync_locked(state)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn arm_locked(
        &self,
        state: &mut GoalState,
        reference: GoalRef,
        max_rounds: u64,
    ) -> Result<GoalActivation, GoalError> {
        if !(1..=MAX_SAFE_INTEGER).contains(&max_rounds) {
            return Err(GoalError::Invalid(
                "goal round cap must be a positive safe integer",
            ));
        }
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
            queued_round: None,
        });
        Ok(GoalActivation {
            inner: Arc::downgrade(&self.inner),
            id,
        })
    }

    async fn transition(
        &self,
        reference: GoalRef,
        allowed: &[GoalPhase],
        phase: GoalPhase,
        arm: bool,
        cancellation: CancellationToken,
    ) -> Result<GoalSnapshot, GoalError> {
        self.require_live()?;
        check_cancellation(&cancellation)?;
        let mut state = self.inner.state.lock().await;
        self.sync_locked(&mut state)?;
        let current = self.current_locked(&state, &reference)?.clone();
        if !allowed.contains(&current.phase) {
            return Err(GoalError::InvalidTransition);
        }
        let snapshot = GoalSnapshot {
            reference: next_ref(&current.reference)?,
            phase,
            title: current.title,
            max_goal_rounds: current.max_goal_rounds,
            tombstone: false,
        };
        let operation = match phase {
            GoalPhase::Paused => GoalOperation::Pause,
            GoalPhase::Active => GoalOperation::Resume,
            GoalPhase::Complete => GoalOperation::Complete,
            GoalPhase::Blocked => GoalOperation::Block,
        };
        let snapshot = self
            .write_locked(
                &mut state,
                Some(reference),
                snapshot,
                operation,
                cancellation,
            )
            .await?;
        if arm {
            self.arm_locked(
                &mut state,
                snapshot.reference.clone(),
                snapshot.max_goal_rounds,
            )?;
        } else {
            state.activation = None;
        }
        Ok(snapshot)
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

/// Session-routed model-facing goal tools over the native goal service.
#[derive(Clone, Default)]
pub struct GoalToolRouter {
    services: Arc<std::sync::Mutex<BTreeMap<SessionId, GoalService>>>,
}

impl GoalToolRouter {
    pub fn insert(&self, service: GoalService) {
        self.services
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(service.agent_id(), service);
    }

    pub fn remove(&self, session_id: &SessionId) {
        self.services
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
    }

    pub fn register_tools(&self, runtime: &ToolRuntime) -> Result<GoalTools, TessivumError> {
        Ok(GoalTools {
            _create_goal: runtime.register(ToolDefinition::new(
                "create_goal",
                "Create one persisted same-session completion goal for the current request.",
                json!({
                    "type": "object",
                    "properties": {
                        "objective": {"type": "string"},
                        "max_goal_rounds": {"type": "integer"}
                    },
                    "required": ["objective"],
                    "additionalProperties": false
                }),
                CreateGoalTool { router: self.clone() },
            ))?,
            _get_goal: runtime.register(ToolDefinition::new(
                "get_goal",
                "Read the current same-session goal before updating it.",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
                GetGoalTool {
                    router: self.clone(),
                },
            ))?,
            _update_goal: runtime.register(ToolDefinition::new(
                "update_goal",
                "Update the exact current goal revision with edit, pause, resume, complete, or blocked.",
                model_goal_update_schema(),
                UpdateGoalTool {
                    router: self.clone(),
                },
            ))?,
        })
    }

    fn service(&self, session: &SessionId) -> Result<GoalService, TessivumError> {
        self.services
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(session)
            .cloned()
            .ok_or_else(|| {
                TessivumError::new(
                    "GOAL_AGENT_NOT_LIVE",
                    "goal tools require a live owning agent",
                    "goals",
                    Value::Null,
                )
            })
    }
}

/// Lifetime owner for model-facing goal registrations.
#[derive(Debug)]
pub struct GoalTools {
    _create_goal: ToolRegistration,
    _get_goal: ToolRegistration,
    _update_goal: ToolRegistration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateGoalInput {
    objective: String,
    #[serde(default)]
    max_goal_rounds: Option<u64>,
}

struct CreateGoalTool {
    router: GoalToolRouter,
}

#[async_trait]
impl ToolHandler for CreateGoalTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let input: CreateGoalInput = serde_json::from_value(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_GOAL",
                "create_goal input is invalid",
                "goals",
                Value::Null,
            )
        })?;
        if input.objective.trim().is_empty() || input.max_goal_rounds == Some(0) {
            return Err(model_goal_update_error(
                "objective must be non-empty and max_goal_rounds must be positive",
            ));
        }
        let goals = self.router.service(&context.session)?;
        goals
            .create(input.objective, input.max_goal_rounds, context.cancellation)
            .await
            .map_err(|error| error.tool_error())?;
        Ok(model_goal_output(
            goals
                .model_value()
                .await
                .map_err(|error| error.tool_error())?,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyGoalInput {}

struct GetGoalTool {
    router: GoalToolRouter,
}

#[async_trait]
impl ToolHandler for GetGoalTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        serde_json::from_value::<EmptyGoalInput>(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_GOAL",
                "get_goal input is invalid",
                "goals",
                Value::Null,
            )
        })?;
        let value = self
            .router
            .service(&context.session)?
            .model_value()
            .await
            .map_err(|error| error.tool_error())?;
        Ok(model_goal_output(value))
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ModelGoalAction {
    Edit,
    Pause,
    Resume,
    Complete,
    Blocked,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelGoalUpdate {
    goal_id: String,
    revision: u64,
    action: ModelGoalAction,
    #[serde(default)]
    objective: Option<String>,
    #[serde(default)]
    max_goal_rounds: Option<u64>,
    #[serde(default)]
    blocked_reason: Option<String>,
}

struct UpdateGoalTool {
    router: GoalToolRouter,
}

#[async_trait]
impl ToolHandler for UpdateGoalTool {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let input: ModelGoalUpdate = serde_json::from_value(arguments).map_err(|_| {
            TessivumError::new(
                "INVALID_GOAL",
                "update_goal input is invalid",
                "goals",
                Value::Null,
            )
        })?;
        if input.goal_id.is_empty() || input.goal_id != input.goal_id.trim() || input.revision == 0
        {
            return Err(TessivumError::new(
                "INVALID_GOAL",
                "goal_id must be non-empty and revision must be positive",
                "goals",
                Value::Null,
            ));
        }
        let reference = GoalRef {
            id: input.goal_id,
            revision: input.revision,
        };
        let objective = input.objective.filter(|value| !value.is_empty());
        let max_goal_rounds = input.max_goal_rounds.filter(|value| *value != 0);
        let blocked_reason = input
            .blocked_reason
            .filter(|value| !value.trim().is_empty());
        let goals = self.router.service(&context.session)?;
        let terminal_action = matches!(
            input.action,
            ModelGoalAction::Complete | ModelGoalAction::Blocked
        );
        match input.action {
            ModelGoalAction::Edit => {
                if blocked_reason.is_some() {
                    return Err(model_goal_update_error(
                        "blocked_reason is valid only with action blocked",
                    ));
                }
                goals
                    .edit(reference, objective, max_goal_rounds, context.cancellation)
                    .await
                    .map_err(|error| error.tool_error())?;
            }
            ModelGoalAction::Pause | ModelGoalAction::Resume => {
                if objective.is_some() || max_goal_rounds.is_some() || blocked_reason.is_some() {
                    return Err(model_goal_update_error(
                        "objective and max_goal_rounds are valid only with action edit; blocked_reason is valid only with action blocked",
                    ));
                }
                let result = if matches!(input.action, ModelGoalAction::Pause) {
                    goals.pause(reference, context.cancellation).await
                } else {
                    goals.resume(reference, context.cancellation).await
                };
                result.map_err(|error| error.tool_error())?;
            }
            ModelGoalAction::Complete => {
                if objective.is_some() || max_goal_rounds.is_some() || blocked_reason.is_some() {
                    return Err(model_goal_update_error(
                        "objective and max_goal_rounds are valid only with action edit; blocked_reason is valid only with action blocked",
                    ));
                }
                goals
                    .complete(reference, context.cancellation)
                    .await
                    .map_err(|error| error.tool_error())?;
            }
            ModelGoalAction::Blocked => {
                if objective.is_some() || max_goal_rounds.is_some() {
                    return Err(model_goal_update_error(
                        "objective and max_goal_rounds are valid only with action edit",
                    ));
                }
                let Some(message) = blocked_reason else {
                    return Err(model_goal_update_error(
                        "blocked_reason is required with action blocked",
                    ));
                };
                goals
                    .block(
                        reference,
                        GoalBlockReason {
                            code: "model-reported".into(),
                            message,
                        },
                        context.cancellation,
                    )
                    .await
                    .map_err(|error| error.tool_error())?;
            }
        }
        let value = goals
            .model_value()
            .await
            .map_err(|error| error.tool_error())?;
        let mut output = model_goal_output(value.clone());
        if terminal_action {
            let objective = value
                .pointer("/goal/objective")
                .and_then(Value::as_str)
                .unwrap_or("the current goal");
            let action = if matches!(input.action, ModelGoalAction::Complete) {
                "complete"
            } else {
                "blocked"
            };
            let text = if action == "complete" {
                format!(
                    "<goal_complete>\nGoal: {objective}\nThe goal is marked complete and this autonomous run is ending. Write the closing message to the user now: state the outcome, summarize what was done and how it was verified, and point to concrete results. Do not call any more tools in this run.\n</goal_complete>"
                )
            } else {
                format!(
                    "<goal_blocked>\nGoal: {objective}\nThe goal is marked blocked and this autonomous run is ending. Write the closing message to the user now, including the concrete blocker and what is needed to continue. Do not call any more tools in this run.\n</goal_blocked>"
                )
            };
            output.meta = json!({"deferredContext": {
                "plugin": "tool-goal",
                "summary": format!("{action}: {objective}"),
                "text": text,
            }});
        }
        Ok(output)
    }
}

fn model_goal_output(value: Value) -> ToolOutput {
    ToolOutput::new(
        vec![ContentBlock::Text {
            text: serde_json::to_string(&value).expect("goal tool value is serializable"),
        }],
        false,
        Value::Null,
    )
}

fn model_goal_update_error(message: &'static str) -> TessivumError {
    TessivumError::new("INVALID_GOAL", message, "goals", Value::Null)
}

fn model_goal_update_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["goal_id", "revision", "action"],
        "properties": {
            "goal_id": {"type": "string"},
            "revision": {"type": "integer"},
            "action": {"type": "string", "enum": ["edit", "pause", "resume", "complete", "blocked"]},
            "objective": {"type": "string"},
            "max_goal_rounds": {"type": "integer"},
            "blocked_reason": {"type": "string"},
        },
    })
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

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(MAX_SAFE_INTEGER as u128) as u64
}

fn next_ref(reference: &GoalRef) -> Result<GoalRef, GoalError> {
    Ok(GoalRef {
        id: reference.id.clone(),
        revision: reference
            .revision
            .checked_add(1)
            .ok_or(GoalError::Invalid("goal revision overflow"))?,
    })
}
fn validate_goal_operation(
    goals: &BTreeMap<String, GoalSnapshot>,
    operation: GoalOperation,
    next: &GoalSnapshot,
) -> Result<(), GoalError> {
    if operation == GoalOperation::Create {
        return if !next.tombstone && next.reference.revision == 1 && next.phase == GoalPhase::Active
        {
            Ok(())
        } else {
            Err(GoalError::InvalidTransition)
        };
    }
    let current = goals
        .get(&next.reference.id)
        .ok_or_else(|| GoalError::NotFound(next.reference.id.clone()))?;
    if current.tombstone {
        return Err(GoalError::Tombstoned);
    }
    if next.reference != next_ref(&current.reference)? {
        return Err(GoalError::Stale);
    }
    let same_definition =
        current.title == next.title && current.max_goal_rounds == next.max_goal_rounds;
    let valid = match operation {
        GoalOperation::Create => unreachable!(),
        GoalOperation::Edit => !next.tombstone && next.phase == current.phase,
        GoalOperation::Pause => {
            !next.tombstone
                && same_definition
                && current.phase == GoalPhase::Active
                && next.phase == GoalPhase::Paused
        }
        GoalOperation::Resume => {
            !next.tombstone
                && same_definition
                && matches!(
                    current.phase,
                    GoalPhase::Active | GoalPhase::Paused | GoalPhase::Blocked
                )
                && next.phase == GoalPhase::Active
        }
        GoalOperation::Complete => {
            !next.tombstone
                && same_definition
                && current.phase != GoalPhase::Complete
                && next.phase == GoalPhase::Complete
        }
        GoalOperation::Block => {
            !next.tombstone
                && same_definition
                && current.phase == GoalPhase::Active
                && next.phase == GoalPhase::Blocked
        }
        GoalOperation::Clear => next.tombstone && same_definition && next.phase == current.phase,
    };
    if valid {
        Ok(())
    } else {
        Err(GoalError::InvalidTransition)
    }
}

fn validate_goal_times(
    times: &BTreeMap<String, GoalTimes>,
    operation: GoalOperation,
    snapshot: &GoalSnapshot,
    next: GoalTimes,
) -> Result<(), GoalError> {
    if operation == GoalOperation::Create {
        return if next.rounds_started == 0 {
            Ok(())
        } else {
            Err(GoalError::Invalid("goal create roundsStarted must be zero"))
        };
    }
    let current = times.get(&snapshot.reference.id).ok_or(GoalError::Invalid(
        "durable goal change lacks prior timestamps",
    ))?;
    if next.created_at == current.created_at
        && next.updated_at >= current.updated_at
        && next.rounds_started == current.rounds_started
    {
        Ok(())
    } else {
        Err(GoalError::Invalid(
            "goal change does not preserve counters or timestamps",
        ))
    }
}

fn normalize_objective(objective: String) -> Result<String, GoalError> {
    let objective = objective.trim();
    if objective.is_empty() {
        return Err(GoalError::Invalid("goal objective must be non-empty"));
    }
    Ok(objective.into())
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
fn apply_goal_round(state: &mut GoalState, event: &SessionEvent) -> Result<(), GoalError> {
    if event.event_type != "user/message" {
        return Ok(());
    }
    let message: Message = serde_json::from_value(event.data.clone())
        .map_err(|_| GoalError::Invalid("durable goal round message is invalid"))?;
    let MessageSource::Goal {
        goal_id,
        revision,
        round,
    } = message.source
    else {
        return Ok(());
    };
    if message.role != MessageRole::User {
        return Err(GoalError::Invalid("goal round must be a user message"));
    }
    let goal = state
        .goals
        .get(&goal_id)
        .filter(|goal| !goal.tombstone && goal.phase == GoalPhase::Active)
        .cloned()
        .ok_or(GoalError::Inactive)?;
    if goal.reference.revision != revision {
        return Err(GoalError::Stale);
    }
    let times = state
        .times
        .get_mut(&goal_id)
        .ok_or(GoalError::Invalid("goal lacks round counter"))?;
    if round != times.rounds_started.saturating_add(1) || round > goal.max_goal_rounds {
        return Err(GoalError::RoundCap);
    }
    times.rounds_started = round;
    if state
        .activation
        .as_ref()
        .is_none_or(|activation| activation.reference.id != goal.reference.id)
    {
        state.next_activation = state.next_activation.checked_add(1).unwrap_or(1);
        state.activation = Some(Activation {
            id: state.next_activation,
            reference: goal.reference.clone(),
            max_rounds: goal.max_goal_rounds,
            rounds: round,
            first_event_seq: event.seq,
            last_user_event_seq: Some(event.seq),
            queued_round: None,
        });
    } else if let Some(activation) = state.activation.as_mut() {
        activation.reference = goal.reference;
        activation.max_rounds = goal.max_goal_rounds;
        activation.rounds = round;
        activation.last_user_event_seq = Some(event.seq);
        activation.queued_round = None;
    }
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
                time: now(),
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
            "snapshot": {"type": "object", "additionalProperties": false, "required": ["reference", "phase", "title", "maxGoalRounds", "tombstone"], "properties": {
                "reference": {"type": "object", "additionalProperties": false, "required": ["id", "revision"], "properties": {
                    "id": {"type": "string"}, "revision": {"type": "integer"}
                }},
                "phase": {"type": "string", "enum": ["active", "paused", "blocked", "complete"]},
                "title": {"type": "string"},
                "maxGoalRounds": {"type": "integer"},
                "tombstone": {"type": "boolean"}
            }}
        }
    })
}

use std::{env, fmt, path::PathBuf};

use serde::ser::SerializeStruct;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{agent_mode::AgentModeId, error::TessivumError};

/// The only session-log format accepted by this crate.
pub const SESSION_FORMAT_VERSION: u64 = 0;
/// Largest integer that JSON JavaScript consumers can represent exactly.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

macro_rules! opaque_string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Wraps an opaque wire value without interpreting it.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Generates an opaque UUID v4 string for a newly-owned identity.
            pub fn random() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            /// Borrows the unmodified wire value.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consumes this wrapper and returns its unmodified wire value.
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.into_inner()
            }
        }
    };
}

opaque_string_id!(
    /// Opaque session identity preserved verbatim on the wire.
    SessionId
);
opaque_string_id!(
    /// Opaque message identity preserved verbatim on the wire.
    MessageId
);
opaque_string_id!(
    /// Opaque model tool-call correlation identity preserved verbatim on the wire.
    ToolCallId
);
opaque_string_id!(
    /// Opaque provider-issued request identity retained for diagnostics.
    ProviderRequestId
);
opaque_string_id!(
    /// Opaque workflow-run identity preserved verbatim on the wire.
    WorkflowRunId
);

/// Durable metadata kept outside a session's append-only event log.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub version: u64,
    pub id: SessionId,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<SessionOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delegation_depth: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<AgentModeId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionHeaderWire {
    version: u64,
    id: SessionId,
    created_at: u64,
    cwd: Option<String>,
    parent_session: Option<SessionId>,
    seed_length: Option<u64>,
    origin: Option<SessionOrigin>,
    delegation_depth: Option<u64>,
    agent_mode: Option<AgentModeId>,
    agent_preset: Option<String>,
}

impl<'de> Deserialize<'de> for SessionHeader {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_wire(SessionHeaderWire::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

impl SessionHeader {
    pub(crate) fn from_json_value(value: Value) -> Result<Self, TessivumError> {
        let wire = serde_json::from_value(value).map_err(|error| {
            invalid(
                "INVALID_SESSION_HEADER",
                "session header is invalid",
                json!({"reason": error.to_string()}),
            )
        })?;
        Self::from_wire(wire)
    }

    fn from_wire(wire: SessionHeaderWire) -> Result<Self, TessivumError> {
        if wire.agent_mode.is_some() && wire.agent_preset.is_some() {
            return Err(invalid(
                "SESSION_HEADER_MODE_CONFLICT",
                "session header cannot contain both agentMode and agentPreset",
                json!({"fields": ["agentMode", "agentPreset"]}),
            ));
        }
        Ok(Self {
            version: wire.version,
            id: wire.id,
            created_at: wire.created_at,
            cwd: wire.cwd,
            parent_session: wire.parent_session,
            seed_length: wire.seed_length,
            origin: wire.origin,
            delegation_depth: wire.delegation_depth,
            agent_mode: wire
                .agent_mode
                .map(Ok)
                .or_else(|| {
                    wire.agent_preset
                        .as_deref()
                        .map(migrate_legacy_agent_preset)
                })
                .transpose()?,
        })
    }
}

/// Converts one stored Rust-domain preset name only at a persistence migration boundary.
pub(crate) fn migrate_legacy_agent_preset(
    legacy_preset: &str,
) -> Result<AgentModeId, TessivumError> {
    match legacy_preset {
        "standard" => Ok(AgentModeId::standard()),
        "code" => Ok(AgentModeId::ptc()),
        "minimal" => Ok(AgentModeId::minimal()),
        "cordis" => Ok(AgentModeId::composition()),
        _ => {
            let expected_mode_path = legacy_mode_path(legacy_preset);
            Err(TessivumError::new(
                "MODE_MIGRATION_REQUIRED",
                format!(
                    "legacy agent preset {legacy_preset:?} must migrate to {}",
                    expected_mode_path.display()
                ),
                "persistence",
                json!({
                    "legacyPreset": legacy_preset,
                    "expectedModePath": expected_mode_path.display().to_string(),
                }),
            ))
        }
    }
}

fn legacy_mode_path(legacy_preset: &str) -> PathBuf {
    let root = env::var_os("TESSIVUM_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".tessivum")))
        .unwrap_or_else(|| PathBuf::from(".tessivum"));
    PathBuf::from(format!(
        "{}/modes/{legacy_preset}/mode.toml",
        root.display()
    ))
}
/// Provider/model authority captured for a session or the durable host default.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModelSelection {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl SessionModelSelection {
    pub fn validate(&self) -> Result<(), TessivumError> {
        if self.provider.trim().is_empty() || self.model.trim().is_empty() {
            return Err(invalid(
                "INVALID_MODEL_SELECTION",
                "provider and model must not be blank",
                Value::Null,
            ));
        }
        if self
            .reasoning_effort
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(invalid(
                "INVALID_MODEL_SELECTION",
                "reasoningEffort must not be blank when present",
                Value::Null,
            ));
        }
        Ok(())
    }
}

/// Durable classification for sessions created by delegation.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionOrigin {
    Subagent,
}

impl SessionHeader {
    pub fn validate(&self) -> Result<(), TessivumError> {
        if self.version != SESSION_FORMAT_VERSION {
            return Err(invalid(
                "UNSUPPORTED_SESSION_FORMAT",
                "session header version is not supported",
                json!({"version": self.version, "supportedVersion": SESSION_FORMAT_VERSION}),
            ));
        }
        check_safe_integer("createdAt", self.created_at)?;
        if let Some(seed_length) = self.seed_length {
            check_safe_integer("seedLength", seed_length)?;
        }
        if let Some(delegation_depth) = self.delegation_depth {
            check_safe_integer("delegationDepth", delegation_depth)?;
        }
        Ok(())
    }
}

/// Serializable provider or transport failure facts.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LlmFailure {
    pub message: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_retry_after_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<ProviderRequestId>,
}

impl LlmFailure {
    pub fn validate(&self) -> Result<(), TessivumError> {
        if self.message.trim().is_empty() || self.code.trim().is_empty() {
            return Err(invalid(
                "INVALID_LLM_FAILURE",
                "LLM failure message and code must be non-empty",
                Value::Null,
            ));
        }
        if let Some(status) = self.status {
            if !(100..=599).contains(&status) {
                return Err(invalid(
                    "INVALID_LLM_FAILURE_STATUS",
                    "LLM failure status must be an HTTP status code",
                    json!({"status": status}),
                ));
            }
        }
        validate_positive_optional("providerRetryAfterMs", self.provider_retry_after_ms)?;
        if let Some(request_id) = &self.request_id {
            validate_nonempty("INVALID_LLM_REQUEST_ID", "requestId", request_id.as_str())?;
        }
        Ok(())
    }
}

/// A model-visible block. Tool arguments remain the provider's raw JSON string.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Image {
        attachment: Value,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        arguments: String,
    },
    ToolResult {
        tool_call_id: ToolCallId,
        content: Vec<ContentBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

impl ContentBlock {
    pub fn validate(&self) -> Result<(), TessivumError> {
        match self {
            Self::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                validate_nonempty(
                    "INVALID_TOOL_CALL_ID",
                    "tool-result toolCallId",
                    tool_call_id.as_str(),
                )?;
                for block in content {
                    block.validate()?;
                }
            }
            Self::Text { .. }
            | Self::Reasoning { .. }
            | Self::Image { .. }
            | Self::ToolCall { .. } => {}
        }
        Ok(())
    }

    pub(crate) fn type_tag(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Reasoning { .. } => "reasoning",
            Self::Image { .. } => "image",
            Self::ToolCall { .. } => "tool-call",
            Self::ToolResult { .. } => "tool-result",
        }
    }
}

/// One named contribution to a snapshot-form plugin message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSnapshotSection {
    pub name: String,
    pub text: String,
}

/// Semantic form declared by a plugin source.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextForm {
    Instructions,
    Catalog,
    Snapshot,
    Notice,
    Relay,
    Recall,
}

/// The producer of one model-visible message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum MessageSource {
    Goal {
        goal_id: String,
        revision: u64,
        round: u64,
    },
    User {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_time_zone: Option<String>,
    },
    Plugin {
        plugin: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        compaction_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        form: Option<ContextForm>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sections: Option<Vec<ContextSnapshotSection>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    SkillInvocation {
        name: String,
        form: ContextForm,
    },
    Model {
        provider: String,
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        replay_state: Option<Value>,
    },
    Tool {
        call_id: ToolCallId,
    },
}

impl MessageSource {
    pub fn validate(&self) -> Result<(), TessivumError> {
        match self {
            Self::Plugin {
                plugin,
                compaction_id,
                form,
                sections,
                summary,
            } => {
                validate_nonempty("INVALID_MESSAGE_SOURCE", "plugin", plugin)?;
                if let Some(compaction_id) = compaction_id {
                    validate_nonempty("INVALID_MESSAGE_SOURCE", "compactionId", compaction_id)?;
                }
                match form {
                    Some(ContextForm::Snapshot) if sections.is_none() => {
                        return Err(invalid(
                            "INVALID_MESSAGE_SOURCE",
                            "snapshot plugin sources require sections",
                            Value::Null,
                        ));
                    }
                    Some(ContextForm::Notice) if summary.is_none() => {
                        return Err(invalid(
                            "INVALID_MESSAGE_SOURCE",
                            "notice plugin sources require a summary",
                            Value::Null,
                        ));
                    }
                    Some(ContextForm::Snapshot | ContextForm::Notice)
                    | Some(
                        ContextForm::Instructions
                        | ContextForm::Catalog
                        | ContextForm::Relay
                        | ContextForm::Recall,
                    )
                    | None => {}
                }
            }
            Self::SkillInvocation { name, form } => {
                validate_nonempty("INVALID_MESSAGE_SOURCE", "name", name)?;
                if *form != ContextForm::Instructions {
                    return Err(invalid(
                        "INVALID_MESSAGE_SOURCE",
                        "skill invocation sources require instructions form",
                        Value::Null,
                    ));
                }
            }
            Self::Model {
                provider, model, ..
            } => validate_route(provider, model)?,
            Self::Tool { call_id } => {
                validate_nonempty("INVALID_TOOL_CALL_ID", "callId", call_id.as_str())?;
            }
            Self::User { client_time_zone } => {
                if let Some(value) = client_time_zone {
                    validate_client_time_zone(value)?;
                }
            }
            Self::Goal {
                goal_id,
                revision,
                round,
            } => {
                validate_nonempty("INVALID_GOAL_SOURCE", "goalId", goal_id)?;
                if *revision == 0 || *round == 0 {
                    return Err(TessivumError::new(
                        "INVALID_GOAL_SOURCE",
                        "goal message source revision and round must be positive",
                        "protocol",
                        Value::Null,
                    ));
                }
            }
        }
        Ok(())
    }
}

fn validate_client_time_zone(value: &str) -> Result<(), TessivumError> {
    if value == "UTC" {
        return Ok(());
    }
    (!value.is_empty()
        && value.len() <= 64
        && value.trim() == value
        && value.contains('/')
        && !value.contains("..")
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'+' | b'.' | b'-' | b'/')
        }))
    .then_some(())
    .ok_or_else(|| {
        TessivumError::new(
            "INVALID_CLIENT_TIME_ZONE",
            "clientTimeZone must be UTC or a valid IANA Area/Location name",
            "protocol",
            Value::Null,
        )
    })
}

/// Provider-neutral conversation role.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

/// One immutable message shared by delivery, durable history, and model requests.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub id: MessageId,
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    pub source: MessageSource,
}

impl Message {
    pub fn validate(&self) -> Result<(), TessivumError> {
        self.source.validate()?;
        for block in &self.content {
            block.validate()?;
        }
        Ok(())
    }
}

/// Token accounting for one model call. Cached input is reported separately.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn validate(&self) -> Result<(), TessivumError> {
        for (name, value) in [
            ("inputTokens", self.input_tokens),
            ("outputTokens", self.output_tokens),
        ] {
            check_safe_integer(name, value)?;
        }
        for (name, value) in [
            ("cacheReadTokens", self.cache_read_tokens),
            ("cacheWriteTokens", self.cache_write_tokens),
            ("reasoningTokens", self.reasoning_tokens),
        ] {
            if let Some(value) = value {
                check_safe_integer(name, value)?;
            }
        }
        Ok(())
    }
}

/// Why a model response stopped.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    MaxTokens,
    Aborted { failure: LlmFailure },
    Error { failure: LlmFailure },
}

impl FinishReason {
    pub fn validate(&self) -> Result<(), TessivumError> {
        match self {
            Self::Aborted { failure } | Self::Error { failure } => failure.validate(),
            Self::Stop | Self::ToolCalls | Self::MaxTokens => Ok(()),
        }
    }
}

/// Raw streaming protocol emitted by a model adapter.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum StreamChunk {
    BlockStart {
        index: u64,
        block_type: String,
    },
    TextDelta {
        index: u64,
        text: String,
    },
    ReasoningDelta {
        index: u64,
        text: String,
    },
    ToolCallDelta {
        index: u64,
        id: ToolCallId,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments_delta: String,
    },
    BlockEnd {
        index: u64,
        block: ContentBlock,
    },
    Usage {
        usage: TokenUsage,
    },
    Finish {
        reason: FinishReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        replay_state: Option<Value>,
    },
}

impl StreamChunk {
    pub fn validate(&self) -> Result<(), TessivumError> {
        match self {
            Self::BlockStart { index, block_type } => {
                check_safe_integer("index", *index)?;
                validate_block_type(block_type)?;
            }
            Self::TextDelta { index, .. } | Self::ReasoningDelta { index, .. } => {
                check_safe_integer("index", *index)?;
            }
            Self::ToolCallDelta { index, .. } => {
                check_safe_integer("index", *index)?;
            }
            Self::BlockEnd { index, block } => {
                check_safe_integer("index", *index)?;
                block.validate()?;
            }
            Self::Usage { usage } => usage.validate()?,
            Self::Finish { reason, .. } => reason.validate()?,
        }
        Ok(())
    }
}

/// JSON-schema description of one model-callable tool.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSchema {
    pub fn validate(&self) -> Result<(), TessivumError> {
        validate_nonempty("INVALID_TOOL_SCHEMA", "tool schema name", &self.name)?;
        if !self.parameters.is_object() {
            return Err(invalid(
                "INVALID_TOOL_SCHEMA",
                "tool schema parameters must be a JSON object",
                json!({"field": "parameters"}),
            ));
        }
        Ok(())
    }
}

/// Provider, model, and sampling values materialized into a request header.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LlmCallConfig {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

impl LlmCallConfig {
    pub fn validate(&self) -> Result<(), TessivumError> {
        validate_route(&self.provider, &self.model)?;
        if let Some(reasoning_effort) = &self.reasoning_effort {
            validate_nonempty(
                "INVALID_REASONING_EFFORT",
                "reasoningEffort",
                reasoning_effort,
            )?;
        }
        validate_temperature(self.temperature)?;
        validate_positive_optional("maxTokens", self.max_tokens)?;
        validate_stop(self.stop.as_deref())
    }
}

/// Adapter values that were materialized into a request header.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LlmCallConfigAdapterDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<bool>,
}

impl LlmCallConfigAdapterDefaults {
    pub fn validate(&self) -> Result<(), TessivumError> {
        if self.reasoning_effort == Some(false) || self.max_tokens == Some(false) {
            return Err(invalid(
                "INVALID_ADAPTER_DEFAULTS",
                "adapter default markers must be true when present",
                Value::Null,
            ));
        }
        if self.reasoning_effort.is_none() && self.max_tokens.is_none() {
            return Err(invalid(
                "INVALID_ADAPTER_DEFAULTS",
                "empty adapter default markers must be omitted",
                Value::Null,
            ));
        }
        Ok(())
    }
}

/// Fully assembled model request. Cancellation is deliberately out of band.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenerateRequest {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<GeneratePurpose>,
}

/// Provider-neutral classification for an auxiliary model call.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GeneratePurpose {
    Compaction,
    SessionTitle,
}

impl GenerateRequest {
    pub fn validate(&self) -> Result<(), TessivumError> {
        validate_route(&self.provider, &self.model)?;
        if let Some(reasoning_effort) = &self.reasoning_effort {
            validate_nonempty(
                "INVALID_REASONING_EFFORT",
                "reasoningEffort",
                reasoning_effort,
            )?;
        }
        validate_temperature(self.temperature)?;
        validate_positive_optional("maxTokens", self.max_tokens)?;
        validate_stop(self.stop.as_deref())?;
        for message in &self.messages {
            message.validate()?;
        }
        if let Some(tools) = &self.tools {
            validate_tools(tools)?;
        }
        Ok(())
    }
}

/// Why an active agent driver was cancelled.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum AgentCancelCause {
    User,
    Parent,
    Hook { reason: String },
    Disposed,
}

/// Durable cancellation cause, including imported logs with no original cause.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum TurnEndCancelCause {
    User,
    Parent,
    Hook { reason: String },
    Disposed,
    Legacy,
}

impl From<AgentCancelCause> for TurnEndCancelCause {
    fn from(cause: AgentCancelCause) -> Self {
        match cause {
            AgentCancelCause::User => Self::User,
            AgentCancelCause::Parent => Self::Parent,
            AgentCancelCause::Hook { reason } => Self::Hook { reason },
            AgentCancelCause::Disposed => Self::Disposed,
        }
    }
}

/// Why a turn ended.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum TurnEndReason {
    Completed,
    Aborted { reason: TurnEndCancelCause },
    Blocked,
    Error { error: LlmFailure },
    MaxTokens,
    Interrupted,
}

impl TurnEndReason {
    pub fn validate(&self) -> Result<(), TessivumError> {
        match self {
            Self::Error { error } => error.validate(),
            Self::Completed
            | Self::Aborted { .. }
            | Self::Blocked
            | Self::MaxTokens
            | Self::Interrupted => Ok(()),
        }
    }
}

/// An operation that places a message-producing event on the derived surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceOp {
    Append,
    Replace { start: u64, end: u64 },
}

impl SurfaceOp {
    pub fn validate(&self) -> Result<(), TessivumError> {
        if let Self::Replace { start, end } = self {
            check_safe_integer("surfaceOp.start", *start)?;
            check_safe_integer("surfaceOp.end", *end)?;
            if start > end {
                return Err(invalid(
                    "INVALID_SURFACE_RANGE",
                    "surface replacement start must not exceed end",
                    json!({"start": start, "end": end}),
                ));
            }
        }
        Ok(())
    }
}

impl Serialize for SurfaceOp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Append => serializer.serialize_str("append"),
            Self::Replace { start, end } => {
                let mut state = serializer.serialize_struct("SurfaceOp", 3)?;
                state.serialize_field("op", "replace")?;
                state.serialize_field("start", start)?;
                state.serialize_field("end", end)?;
                state.end()
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SurfaceOpWire {
    Append(String),
    Replace { op: String, start: u64, end: u64 },
}

impl<'de> Deserialize<'de> for SurfaceOp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match SurfaceOpWire::deserialize(deserializer)? {
            SurfaceOpWire::Append(op) if op == "append" => Ok(Self::Append),
            SurfaceOpWire::Replace { op, start, end } if op == "replace" => {
                Ok(Self::Replace { start, end })
            }
            _ => Err(de::Error::custom(
                "surfaceOp must be append or a replace range",
            )),
        }
    }
}
/// Canonical terminal outcome for one durable workflow member.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowAgentOutcome {
    Completed,
    Failed,
    Cancelled,
}

/// Canonical terminal reason for one durable workflow run.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowStopReason {
    Completed,
    Cancelled,
    Error,
}

/// Opens one durable top-level workflow run record.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolWorkflowRunStartData {
    pub run_id: WorkflowRunId,
    pub name: String,
}

/// Records one workflow member after its child session is published.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolWorkflowAgentStartData {
    pub run_id: WorkflowRunId,
    pub seq: u64,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub child_id: SessionId,
}

/// Settles one previously started workflow member.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolWorkflowAgentEndData {
    pub run_id: WorkflowRunId,
    pub seq: u64,
    pub outcome: WorkflowAgentOutcome,
}

/// Settles one workflow run after its live resources reach quiescence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolWorkflowRunEndData {
    pub run_id: WorkflowRunId,
    pub stop_reason: WorkflowStopReason,
}

/// A JSON-lossless append-only session event envelope.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub seq: u64,
    pub time: u64,
    /// Event-specific extensible data, retained without a lossy intermediate DTO.
    pub data: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignorable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_event_seqs: Option<Vec<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface_op: Option<SurfaceOp>,
}

impl SessionEvent {
    /// Validates envelope, forward-compatibility, numeric, and surface invariants.
    pub fn validate(&self) -> Result<(), TessivumError> {
        check_safe_integer("seq", self.seq)?;
        check_safe_integer("time", self.time)?;

        if self.ignorable == Some(false) {
            return Err(invalid(
                "INVALID_IGNORABLE_EVENT",
                "ignorable must be true when present",
                Value::Null,
            ));
        }
        if !is_known_event_type(&self.event_type) && self.ignorable != Some(true) {
            return Err(invalid(
                "UNKNOWN_REQUIRED_EVENT",
                "unknown session events must be marked ignorable",
                json!({"type": self.event_type}),
            ));
        }
        validate_tool_workflow_event(&self.event_type, &self.data)?;

        if is_surface_event_type(&self.event_type) {
            let surface_op = self.surface_op.as_ref().ok_or_else(|| {
                invalid(
                    "MISSING_SURFACE_OPERATION",
                    "surface events must declare surfaceOp",
                    json!({"type": self.event_type}),
                )
            })?;
            surface_op.validate()?;
            if let Some(source_event_seqs) = &self.source_event_seqs {
                if self.event_type != "assistant/message" && source_event_seqs.is_empty() {
                    return Err(invalid(
                        "INVALID_SOURCE_EVENT_SEQS",
                        "only assistant/message may cite an empty sourceEventSeqs list",
                        json!({"type": self.event_type}),
                    ));
                }
                for source_seq in source_event_seqs {
                    check_safe_integer("sourceEventSeqs", *source_seq)?;
                    if *source_seq >= self.seq {
                        return Err(invalid(
                            "INVALID_SOURCE_EVENT_SEQS",
                            "sourceEventSeqs must cite earlier events",
                            json!({"seq": self.seq, "sourceEventSeq": source_seq}),
                        ));
                    }
                }
            }
            if matches!(surface_op, SurfaceOp::Replace { .. })
                && self.source_event_seqs.as_ref().is_none_or(Vec::is_empty)
            {
                return Err(invalid(
                    "INVALID_SURFACE_REPLACEMENT",
                    "surface replacements must cite shadowed source events",
                    Value::Null,
                ));
            }
        } else if self.surface_op.is_some() || self.source_event_seqs.is_some() {
            return Err(invalid(
                "INVALID_SURFACE_METADATA",
                "surface metadata is only valid on user/message, assistant/message, and tool/result",
                json!({"type": self.event_type}),
            ));
        }
        Ok(())
    }
}

/// Converts a stored preset-selection event only while restoring durable history.
pub(crate) fn migrate_legacy_agent_preset_selection(
    event_type: &mut String,
    data: &mut Value,
) -> Result<(), TessivumError> {
    if event_type != "agent-preset/selected" {
        return Ok(());
    }
    let legacy_preset = data
        .as_object()
        .and_then(|object| object.get("agentPreset"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            invalid(
                "INVALID_LEGACY_AGENT_PRESET_EVENT",
                "legacy agent preset selection must contain agentPreset",
                Value::Null,
            )
        })?;
    let agent_mode = migrate_legacy_agent_preset(legacy_preset)?;
    *event_type = "agent-mode/selected".into();
    *data = json!({"agentMode": agent_mode});
    Ok(())
}

/// One entry in a whole-list todo snapshot.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// Portable todo lifecycle state.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoItem {
    pub fn validate(&self) -> Result<(), TessivumError> {
        if self.content.is_empty() {
            return Err(invalid(
                "INVALID_TODO_ITEM",
                "todo content must be non-empty",
                Value::Null,
            ));
        }
        Ok(())
    }
}

/// Full state required to reconstruct the next model request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EpochHeader {
    pub config: LlmCallConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter_defaults: Option<LlmCallConfigAdapterDefaults>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSchema>>,
}

impl EpochHeader {
    pub fn validate(&self) -> Result<(), TessivumError> {
        self.config.validate()?;
        if let Some(adapter_defaults) = &self.adapter_defaults {
            adapter_defaults.validate()?;
        }
        if let Some(system) = &self.system {
            validate_nonempty("INVALID_EPOCH_HEADER", "system", system)?;
        }
        if let Some(tools) = &self.tools {
            if tools.is_empty() {
                return Err(invalid(
                    "INVALID_EPOCH_HEADER",
                    "empty tool snapshots must be omitted",
                    Value::Null,
                ));
            }
            validate_tools(tools)?;
        }
        Ok(())
    }
}

/// Registration-bound model route metadata for the next request.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestContext {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

impl RequestContext {
    pub fn validate(&self) -> Result<(), TessivumError> {
        validate_route(&self.provider, &self.model)?;
        validate_positive_optional("contextWindow", self.context_window)
    }
}

/// SDK runtime initialization parameters.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub cwd: String,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
}

impl InitializeParams {
    pub fn validate(&self) -> Result<(), TessivumError> {
        validate_positive_optional("maxTokens", self.max_tokens)
    }
}

/// Wire-stable SDK server identity.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SdkServerInfo {
    pub name: String,
    pub version: String,
}

/// SDK runtime initialization result.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub server_info: SdkServerInfo,
}

/// One user turn sent through the SDK runtime protocol.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptParams {
    pub session_id: SessionId,
    pub content_blocks: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_time_zone: Option<String>,
}

impl SessionPromptParams {
    pub fn validate(&self) -> Result<(), TessivumError> {
        for block in &self.content_blocks {
            block.validate()?;
        }
        if let Some(value) = &self.client_time_zone {
            validate_client_time_zone(value)?;
        }
        Ok(())
    }
}

/// Durable queue receipt for an SDK user turn.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptResult {
    pub message_id: MessageId,
}

/// Deployment-mapped SDK run outcome.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SdkRunStatus {
    Ok,
    Error,
}

/// One persisted event forwarded through the SDK notification channel.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEventNotification {
    pub session_id: SessionId,
    pub event: SessionEvent,
}

impl SessionEventNotification {
    pub fn validate(&self) -> Result<(), TessivumError> {
        self.event.validate()
    }
}

/// Whole-agent state reported by the SDK runtime.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionStatus {
    Idle,
    Running,
}

/// One SDK whole-agent lifecycle state notification.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusNotification {
    pub session_id: SessionId,
    pub status: SessionStatus,
}

/// SDK notification that an in-process child session started.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentStartedNotification {
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
}

/// SDK notification that an in-process child session finished.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentFinishedNotification {
    pub provider: String,
    pub agent_id: String,
    pub parent_session_id: SessionId,
    pub child_session_id: SessionId,
    pub status: SdkRunStatus,
    pub stop_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<Vec<ContentBlock>>,
}

impl SubagentFinishedNotification {
    pub fn validate(&self) -> Result<(), TessivumError> {
        if let Some(blocks) = &self.last_assistant_message {
            for block in blocks {
                block.validate()?;
            }
        }
        Ok(())
    }
}

/// JSON-RPC SDK shutdown parameters.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ShutdownParams {}

/// JSON-RPC SDK shutdown result.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct ShutdownResult {}

fn is_known_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "turn/start"
            | "turn/end"
            | "step/start"
            | "step/end"
            | "user/message"
            | "assistant/chunk"
            | "assistant/message"
            | "agent/inbox/enqueued"
            | "agent/inbox/spliced"
            | "tool/call"
            | "tool/result"
            | "tool/code-dispatch-start"
            | "tool/code-dispatch"
            | "todo/write"
            | "cordis/dynamic"
            | "schedule/change"
            | "goal/change"
            | "plan/change"
            | "plan/mode"
            | "command/run"
            | "command/done"
            | "feedback/record"
            | "permission/preset"
            | "sandbox/mode"
            | "approval/policy"
            | "agent-mode/selected"
            | "approval/asked"
            | "approval/decided"
            | "question/asked"
            | "question/resolved"
            | "job/done"
            | "subagent/contained-start"
            | "subagent/contained-end"
            | "tool-workflow/run-start"
            | "tool-workflow/agent-start"
            | "tool-workflow/agent-end"
            | "tool-workflow/run-end"
            | "request/header"
            | "request/context"
            | "llm/retry"
            | "llm/retry-started"
            | "session/model-selected"
            | "session/title"
            | "web/deepseek-search-llm-request"
            | "session/end-seed"
    )
}
fn validate_tool_workflow_event(event_type: &str, data: &Value) -> Result<(), TessivumError> {
    match event_type {
        "tool-workflow/run-start" => {
            let data: ToolWorkflowRunStartData = workflow_event_data(event_type, data)?;
            workflow_nonempty("runId", data.run_id.as_str())?;
            workflow_nonempty("name", &data.name)
        }
        "tool-workflow/agent-start" => {
            if data.get("phase").is_some_and(Value::is_null) {
                return Err(invalid(
                    "INVALID_TOOL_WORKFLOW_EVENT",
                    "tool-workflow agent phase must be omitted or a string",
                    json!({"type": event_type}),
                ));
            }
            let data: ToolWorkflowAgentStartData = workflow_event_data(event_type, data)?;
            workflow_nonempty("runId", data.run_id.as_str())?;
            check_safe_integer("tool-workflow.agent-start.seq", data.seq)?;
            if data.seq == 0 {
                return Err(invalid(
                    "INVALID_TOOL_WORKFLOW_EVENT",
                    "tool-workflow agent sequence must be positive",
                    json!({"type": event_type, "seq": data.seq}),
                ));
            }
            workflow_nonempty("childId", data.child_id.as_str())
        }
        "tool-workflow/agent-end" => {
            let data: ToolWorkflowAgentEndData = workflow_event_data(event_type, data)?;
            workflow_nonempty("runId", data.run_id.as_str())?;
            check_safe_integer("tool-workflow.agent-end.seq", data.seq)?;
            if data.seq == 0 {
                return Err(invalid(
                    "INVALID_TOOL_WORKFLOW_EVENT",
                    "tool-workflow agent sequence must be positive",
                    json!({"type": event_type, "seq": data.seq}),
                ));
            }
            Ok(())
        }
        "tool-workflow/run-end" => {
            let data: ToolWorkflowRunEndData = workflow_event_data(event_type, data)?;
            workflow_nonempty("runId", data.run_id.as_str())
        }
        _ => Ok(()),
    }
}

fn workflow_event_data<'a, T>(event_type: &str, data: &'a Value) -> Result<T, TessivumError>
where
    T: Deserialize<'a>,
{
    T::deserialize(data).map_err(|error| {
        invalid(
            "INVALID_TOOL_WORKFLOW_EVENT",
            "tool-workflow event data does not match its canonical schema",
            json!({"type": event_type, "error": error.to_string()}),
        )
    })
}

fn workflow_nonempty(field: &str, value: &str) -> Result<(), TessivumError> {
    if value.is_empty() {
        return Err(invalid(
            "INVALID_TOOL_WORKFLOW_EVENT",
            "tool-workflow identity fields must not be empty",
            json!({"field": field}),
        ));
    }
    Ok(())
}

fn is_surface_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "user/message" | "assistant/message" | "tool/result"
    )
}

fn invalid(code: &str, message: &str, details: Value) -> TessivumError {
    TessivumError::protocol(code, message, details)
}

fn check_safe_integer(name: &str, value: u64) -> Result<(), TessivumError> {
    if value > MAX_SAFE_INTEGER {
        return Err(invalid(
            "NUMERIC_BOUND_EXCEEDED",
            "numeric protocol fields must fit a JavaScript safe integer",
            json!({"field": name, "value": value, "maximum": MAX_SAFE_INTEGER}),
        ));
    }
    Ok(())
}

fn validate_positive_optional(name: &str, value: Option<u64>) -> Result<(), TessivumError> {
    if let Some(value) = value {
        check_safe_integer(name, value)?;
        if value == 0 {
            return Err(invalid(
                "INVALID_POSITIVE_VALUE",
                "optional numeric protocol fields must be positive when present",
                json!({"field": name, "value": value}),
            ));
        }
    }
    Ok(())
}

fn validate_temperature(temperature: Option<f64>) -> Result<(), TessivumError> {
    if let Some(temperature) = temperature {
        if !temperature.is_finite() {
            return Err(invalid(
                "INVALID_TEMPERATURE",
                "temperature must be finite",
                Value::Null,
            ));
        }
    }
    Ok(())
}

fn validate_route(provider: &str, model: &str) -> Result<(), TessivumError> {
    if provider.trim().is_empty() || model.trim().is_empty() {
        return Err(invalid(
            "INVALID_LLM_ROUTE",
            "provider and model must be non-empty",
            Value::Null,
        ));
    }
    Ok(())
}

fn validate_nonempty(code: &str, field: &str, value: &str) -> Result<(), TessivumError> {
    if value.trim().is_empty() {
        return Err(invalid(
            code,
            "required protocol fields must be non-empty",
            json!({"field": field}),
        ));
    }
    Ok(())
}

fn validate_block_type(block_type: &str) -> Result<(), TessivumError> {
    if matches!(
        block_type,
        "text" | "reasoning" | "image" | "tool-call" | "tool-result"
    ) {
        Ok(())
    } else {
        Err(invalid(
            "INVALID_STREAM_BLOCK_TYPE",
            "blockType must name a known content block type",
            json!({"blockType": block_type}),
        ))
    }
}

fn validate_stop(stop: Option<&[String]>) -> Result<(), TessivumError> {
    if let Some(stop) = stop {
        if stop.is_empty() {
            return Err(invalid(
                "INVALID_STOP_SEQUENCES",
                "empty stop sequences must be omitted",
                Value::Null,
            ));
        }
        for sequence in stop {
            validate_nonempty("INVALID_STOP_SEQUENCES", "stop", sequence)?;
        }
    }
    Ok(())
}

fn validate_tools(tools: &[ToolSchema]) -> Result<(), TessivumError> {
    for (index, tool) in tools.iter().enumerate() {
        tool.validate()?;
        if tools[..index].iter().any(|prior| prior.name == tool.name) {
            return Err(invalid(
                "DUPLICATE_TOOL_SCHEMA",
                "tool schema names must be unique within a request snapshot",
                json!({"name": tool.name}),
            ));
        }
    }
    Ok(())
}

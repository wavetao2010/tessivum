use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use async_trait::async_trait;
use futures_util::{stream, Stream, StreamExt};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};

use crate::{
    ContentBlock, FinishReason, GenerateRequest, LlmFailure, Message, MessageId, MessageRole,
    MessageSource, SessionEvent, SessionId, StreamChunk, TessivumError, TokenUsage, ToolCallId,
};

/// The stable capability key for provider-neutral streamed LLM generation.
pub fn llm_service_key() -> ServiceKey {
    ServiceKey::new("harness.llm", "1")
}

/// One fallible, ordered stream of raw provider chunks.
pub type LlmStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, TessivumError>> + Send>>;

/// Provider-owned model-request retry policy resolved at registration time.
#[derive(Clone, Debug, PartialEq)]
pub enum LlmRetryPolicy {
    Normal {
        max_retries: u64,
        retryable_codes: Vec<String>,
        initial_delay_ms: f64,
        max_delay_ms: f64,
        jitter_ratio: f64,
    },
    Always {
        initial_delay_ms: f64,
        max_delay_ms: f64,
        jitter_ratio: f64,
    },
}

/// Wire configuration for one provider-owned retry policy.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum LlmRetryPolicyConfig {
    Normal {
        #[serde(default)]
        max_retries: Option<u64>,
        #[serde(default)]
        retryable_codes: Option<Vec<String>>,
        #[serde(default)]
        backoff: LlmRetryBackoffConfig,
    },
    Always {
        #[serde(default)]
        backoff: LlmRetryBackoffConfig,
    },
}

/// Optional local-backoff controls for one provider policy.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LlmRetryBackoffConfig {
    #[serde(default)]
    initial_delay_ms: Option<f64>,
    #[serde(default)]
    max_delay_ms: Option<f64>,
    #[serde(default)]
    jitter_ratio: Option<f64>,
}

impl LlmRetryPolicy {
    pub fn resolve(config: Option<&LlmRetryPolicyConfig>) -> Result<Self, TessivumError> {
        const MAX_TIMER_DELAY_MS: f64 = 2_147_483_647.0;
        const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
        const DEFAULT_CODES: [&str; 5] = [
            "EMPTY_RESPONSE",
            "RATE_LIMIT",
            "SERVER",
            "TIMEOUT",
            "TRANSPORT",
        ];
        let resolve_backoff = |backoff: &LlmRetryBackoffConfig| {
            let initial_delay_ms = backoff.initial_delay_ms.unwrap_or(500.0);
            let max_delay_ms = backoff.max_delay_ms.unwrap_or(10_000.0);
            let jitter_ratio = backoff.jitter_ratio.unwrap_or(0.1);
            if !initial_delay_ms.is_finite()
                || initial_delay_ms <= 0.0
                || initial_delay_ms > MAX_TIMER_DELAY_MS
                || !max_delay_ms.is_finite()
                || max_delay_ms <= 0.0
                || max_delay_ms > MAX_TIMER_DELAY_MS
                || initial_delay_ms > max_delay_ms
                || !jitter_ratio.is_finite()
                || !(0.0..=1.0).contains(&jitter_ratio)
            {
                return Err(TessivumError::new(
                    "INVALID_RETRY_POLICY",
                    "retryPolicy backoff values are invalid",
                    "llm",
                    Value::Null,
                ));
            }
            Ok((initial_delay_ms, max_delay_ms, jitter_ratio))
        };
        match config {
            None => Ok(Self::Normal {
                max_retries: 2,
                retryable_codes: DEFAULT_CODES.into_iter().map(str::to_owned).collect(),
                initial_delay_ms: 500.0,
                max_delay_ms: 10_000.0,
                jitter_ratio: 0.1,
            }),
            Some(LlmRetryPolicyConfig::Normal {
                max_retries,
                retryable_codes,
                backoff,
            }) => {
                let max_retries = max_retries.unwrap_or(2);
                let retryable_codes = retryable_codes
                    .clone()
                    .unwrap_or_else(|| DEFAULT_CODES.into_iter().map(str::to_owned).collect());
                if max_retries > MAX_SAFE_INTEGER
                    || retryable_codes.is_empty()
                    || retryable_codes.iter().any(|code| code.is_empty())
                    || retryable_codes.iter().collect::<BTreeSet<_>>().len()
                        != retryable_codes.len()
                {
                    return Err(TessivumError::new(
                        "INVALID_RETRY_POLICY",
                        "retryPolicy must have a safe retry count and unique non-empty codes",
                        "llm",
                        Value::Null,
                    ));
                }
                let (initial_delay_ms, max_delay_ms, jitter_ratio) = resolve_backoff(backoff)?;
                Ok(Self::Normal {
                    max_retries,
                    retryable_codes,
                    initial_delay_ms,
                    max_delay_ms,
                    jitter_ratio,
                })
            }
            Some(LlmRetryPolicyConfig::Always { backoff }) => {
                let (initial_delay_ms, max_delay_ms, jitter_ratio) = resolve_backoff(backoff)?;
                Ok(Self::Always {
                    initial_delay_ms,
                    max_delay_ms,
                    jitter_ratio,
                })
            }
        }
    }

    pub fn policy_key(&self) -> String {
        match self {
            Self::Normal {
                max_retries,
                retryable_codes,
                initial_delay_ms,
                max_delay_ms,
                jitter_ratio,
            } => {
                let mut retryable_codes = retryable_codes.clone();
                retryable_codes.sort();
                serde_json::to_string(&json!([
                    "normal",
                    max_retries,
                    retryable_codes,
                    initial_delay_ms,
                    max_delay_ms,
                    jitter_ratio,
                ]))
                .expect("retry policy key serializes")
            }
            Self::Always {
                initial_delay_ms,
                max_delay_ms,
                jitter_ratio,
            } => serde_json::to_string(&json!([
                "always",
                initial_delay_ms,
                max_delay_ms,
                jitter_ratio,
            ]))
            .expect("retry policy key serializes"),
        }
    }

    pub fn local_delay_ms(&self, retry: u64, random: f64) -> f64 {
        let (initial_delay_ms, max_delay_ms, jitter_ratio) = match self {
            Self::Normal {
                initial_delay_ms,
                max_delay_ms,
                jitter_ratio,
                ..
            }
            | Self::Always {
                initial_delay_ms,
                max_delay_ms,
                jitter_ratio,
            } => (*initial_delay_ms, *max_delay_ms, *jitter_ratio),
        };
        let exponent = retry.saturating_sub(1).min(1024) as i32;
        let exponential = (initial_delay_ms * 2_f64.powi(exponent)).min(max_delay_ms);
        (exponential * (1.0 - jitter_ratio + 2.0 * jitter_ratio * random)).min(max_delay_ms)
    }

    pub fn max_delay_ms(&self) -> f64 {
        match self {
            Self::Normal { max_delay_ms, .. } | Self::Always { max_delay_ms, .. } => *max_delay_ms,
        }
    }

    pub fn mode(&self) -> &'static str {
        match self {
            Self::Normal { .. } => "normal",
            Self::Always { .. } => "always",
        }
    }

    pub fn permits_failure(&self, code: &str) -> bool {
        match self {
            Self::Normal {
                retryable_codes, ..
            } => retryable_codes.iter().any(|candidate| candidate == code),
            Self::Always { .. } => true,
        }
    }

    pub fn max_retries(&self) -> Option<u64> {
        match self {
            Self::Normal { max_retries, .. } => Some(*max_retries),
            Self::Always { .. } => None,
        }
    }
}
/// A model provider that performs exactly one generation attempt per call.
#[async_trait]
pub trait LlmAdapter: Send + Sync {
    async fn generate(
        &self,
        request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError>;

    /// Discovers provider model rows for one immutable configuration snapshot.
    ///
    /// The default retains source compatibility for generation-only adapters.
    async fn models(&self, _config: Value) -> Result<Value, TessivumError> {
        Err(llm_error(
            "LLM_MODELS_UNSUPPORTED",
            "this LLM adapter does not support model discovery",
            Value::Null,
        ))
    }
}

struct ProviderSlot {
    id: u64,
    adapter: Arc<dyn LlmAdapter>,
    retry_policy: Option<LlmRetryPolicy>,
}

#[derive(Default)]
struct ProviderRegistry {
    next_id: u64,
    providers: BTreeMap<String, ProviderSlot>,
}

/// Routes requests to explicitly registered native providers.
#[derive(Clone, Default)]
pub struct LlmRuntime {
    providers: Arc<Mutex<ProviderRegistry>>,
}

impl fmt::Debug for LlmRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let providers = lock(&self.providers);
        formatter
            .debug_struct("LlmRuntime")
            .field("providers", &providers.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Owns one provider registration. Dropping it makes that route unavailable.
pub struct LlmProviderRegistration {
    providers: Weak<Mutex<ProviderRegistry>>,
    provider: String,
    id: u64,
}

impl fmt::Debug for LlmProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmProviderRegistration")
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl LlmProviderRegistration {
    /// Removes this registration early. It is safe to call more than once.
    pub fn unregister(&mut self) -> bool {
        let Some(providers) = self.providers.upgrade() else {
            return false;
        };
        let mut providers = lock(&providers);
        let is_current = providers
            .providers
            .get(&self.provider)
            .is_some_and(|slot| slot.id == self.id);
        if is_current {
            providers.providers.remove(&self.provider);
        }
        is_current
    }

    /// Returns whether this handle still owns the active route.
    pub fn is_active(&self) -> bool {
        self.providers.upgrade().is_some_and(|providers| {
            lock(&providers)
                .providers
                .get(&self.provider)
                .is_some_and(|slot| slot.id == self.id)
        })
    }
}

impl Drop for LlmProviderRegistration {
    fn drop(&mut self) {
        self.unregister();
    }
}

/// The immutable result formed from one terminal provider stream.
#[derive(Clone, Debug, PartialEq)]
pub struct LlmGeneration {
    pub message: Message,
    pub usage: Option<TokenUsage>,
    pub finish_reason: FinishReason,
    /// The exact chunks accepted from the adapter, in arrival order.
    pub chunks: Vec<StreamChunk>,
}

impl LlmRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes this runtime as a scope-owned `harness.llm@1` service.
    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(llm_service_key(), self)
    }

    /// Registers the sole adapter for `provider` until its returned handle drops.
    pub fn register(
        &self,
        provider: impl Into<String>,
        adapter: Arc<dyn LlmAdapter>,
    ) -> Result<LlmProviderRegistration, TessivumError> {
        self.register_with_retry_policy(
            provider,
            adapter,
            Some(LlmRetryPolicy::resolve(None).expect("default retry policy is valid")),
        )
    }

    /// Registers one provider and captures its policy with the adapter route.
    pub fn register_with_retry_policy(
        &self,
        provider: impl Into<String>,
        adapter: Arc<dyn LlmAdapter>,
        retry_policy: Option<LlmRetryPolicy>,
    ) -> Result<LlmProviderRegistration, TessivumError> {
        let provider = provider.into();
        if provider.trim().is_empty() {
            return Err(llm_error(
                "INVALID_LLM_PROVIDER",
                "LLM provider names must not be empty",
                Value::Null,
            ));
        }

        let mut providers = lock(&self.providers);
        if providers.providers.contains_key(&provider) {
            return Err(llm_error(
                "DUPLICATE_LLM_PROVIDER",
                "an LLM provider is already registered for this route",
                json!({"provider": provider}),
            ));
        }
        providers.next_id = providers.next_id.wrapping_add(1);
        let id = providers.next_id;
        providers.providers.insert(
            provider.clone(),
            ProviderSlot {
                id,
                adapter,
                retry_policy,
            },
        );
        Ok(LlmProviderRegistration {
            providers: Arc::downgrade(&self.providers),
            provider,
            id,
        })
    }

    /// Returns the immutable policy captured for this provider route.
    pub fn provider_retry_policy(&self, provider: &str) -> Option<LlmRetryPolicy> {
        lock(&self.providers)
            .providers
            .get(provider)
            .and_then(|slot| slot.retry_policy.clone())
    }

    /// Starts one raw generation stream. This method never retries an adapter call.
    pub async fn generate(
        &self,
        request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        validate_request(&request)?;
        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }

        let adapter = match lock(&self.providers)
            .providers
            .get(&request.provider)
            .map(|slot| Arc::clone(&slot.adapter))
        {
            Some(adapter) => adapter,
            None => {
                return Ok(terminal_failure_stream(
                    llm_error(
                        "LLM_PROVIDER_NOT_FOUND",
                        "no LLM adapter is registered for the requested provider",
                        json!({"provider": request.provider}),
                    ),
                    cancellation,
                ));
            }
        };
        match adapter.generate(request, cancellation.clone()).await {
            Ok(stream) => Ok(cancellable_stream(stream, cancellation)),
            Err(error) => Ok(terminal_failure_stream(error, cancellation)),
        }
    }

    /// Discovers models from the active adapter registered for exactly `provider`.
    ///
    /// An adapter receives a fresh deep clone of the caller's object per attempt.
    /// Discovery has one bounded retry for transient provider failures only.
    pub async fn models(&self, provider: String, config: Value) -> Result<Value, TessivumError> {
        if provider.trim().is_empty() {
            return Err(model_discovery_error(
                "LLM_PROVIDER_NOT_FOUND",
                "no LLM adapter is registered for the requested provider",
                "llm",
                json!({"provider": provider, "attempts": 0, "retries": 0, "retryable": false}),
            ));
        }
        let config = config.as_object().cloned().ok_or_else(|| {
            model_discovery_error(
                "INVALID_MODEL_DISCOVERY_CONFIG",
                "model discovery config must be an object",
                "llm",
                json!({"provider": provider, "attempts": 0, "retries": 0, "retryable": false}),
            )
        })?;

        const ATTEMPTS: u64 = 2;
        const TIMEOUT: Duration = Duration::from_millis(5_000);
        const RETRY_DELAY: Duration = Duration::from_millis(250);
        for attempt in 1..=ATTEMPTS {
            let adapter = lock(&self.providers)
                .providers
                .get(&provider)
                .map(|slot| Arc::clone(&slot.adapter))
                .ok_or_else(|| {
                    model_discovery_error(
                        "LLM_PROVIDER_NOT_FOUND",
                        "no LLM adapter is registered for the requested provider",
                        "llm",
                        json!({
                            "provider": provider,
                            "attempts": attempt - 1,
                            "retries": attempt - 1,
                            "retryable": false,
                        }),
                    )
                })?;
            let result =
                tokio::time::timeout(TIMEOUT, adapter.models(Value::Object(config.clone()))).await;
            match result {
                Ok(Ok(models)) => return Ok(models),
                Ok(Err(error))
                    if attempt < ATTEMPTS && is_transient_model_discovery_error(&error) =>
                {
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                Ok(Err(error)) => {
                    let retryable = is_transient_model_discovery_error(&error);
                    return Err(with_model_discovery_metadata(
                        error, &provider, attempt, retryable,
                    ));
                }
                Err(_) if attempt < ATTEMPTS => tokio::time::sleep(RETRY_DELAY).await,
                Err(_) => {
                    return Err(model_discovery_error(
                        "MODEL_DISCOVERY_TIMEOUT",
                        format!("provider '{provider}' model discovery timed out after 5000ms"),
                        "llm",
                        json!({
                            "provider": provider,
                            "attempts": attempt,
                            "retries": attempt - 1,
                            "retryable": true,
                        }),
                    ));
                }
            }
        }
        unreachable!("model discovery attempts always return")
    }

    /// Consumes one raw generation stream into an immutable assistant message.
    pub async fn complete(
        &self,
        request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmGeneration, TessivumError> {
        let provider = request.provider.clone();
        let model = request.model.clone();
        let mut stream = self.generate(request, cancellation.clone()).await?;
        let mut assembler = BlockAssembler::new(provider, model);

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if let Some(generation) = assembler.push(chunk)? {
                return Ok(generation);
            }
        }

        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        Err(llm_error(
            "LLM_STREAM_ENDED_EARLY",
            "the LLM stream ended before a finish chunk",
            Value::Null,
        ))
    }
}

/// Validates and assembles a single provider stream without interpreting raw tool JSON.
pub struct BlockAssembler {
    provider: String,
    model: String,
    message_id: MessageId,
    blocks: BTreeMap<u64, BlockRecord>,
    usage: Option<TokenUsage>,
    chunks: Vec<StreamChunk>,
    finished: bool,
    finish_reason: Option<FinishReason>,
    replay_state: Option<Value>,
}

struct BlockRecord {
    status: BlockStatus,
}

enum BlockStatus {
    Open(OpenBlock),
    Closed(ContentBlock),
    Omitted,
}

enum OpenBlock {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: Option<ToolCallId>,
        name: Option<String>,
    },
    Direct(String),
}

impl BlockAssembler {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self::with_message_id(provider, model, MessageId::random())
    }

    pub fn with_message_id(
        provider: impl Into<String>,
        model: impl Into<String>,
        message_id: MessageId,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            message_id,
            blocks: BTreeMap::new(),
            usage: None,
            chunks: Vec::new(),
            finished: false,
            finish_reason: None,
            replay_state: None,
        }
    }

    /// Accepts one chunk and returns the completed generation at its terminal finish.
    pub fn push(&mut self, chunk: StreamChunk) -> Result<Option<LlmGeneration>, TessivumError> {
        if self.finished {
            return Err(llm_error(
                "LLM_STREAM_AFTER_FINISH",
                "LLM streams must not contain chunks after finish",
                Value::Null,
            ));
        }
        chunk.validate()?;

        match &chunk {
            StreamChunk::BlockStart { index, block_type } => {
                if self.blocks.contains_key(index) {
                    return Err(stream_error("block indexes must be unique", *index));
                }
                self.blocks.insert(
                    *index,
                    BlockRecord {
                        status: match block_type.as_str() {
                            "text" => BlockStatus::Open(OpenBlock::Text(String::new())),
                            "reasoning" => BlockStatus::Open(OpenBlock::Reasoning(String::new())),
                            "tool-call" => BlockStatus::Open(OpenBlock::ToolCall {
                                id: None,
                                name: None,
                            }),
                            _ => BlockStatus::Open(OpenBlock::Direct(block_type.clone())),
                        },
                    },
                );
            }
            StreamChunk::TextDelta { index, text } => {
                self.with_open(*index, "text delta", |block| match block {
                    OpenBlock::Text(assembled) => {
                        assembled.push_str(text);
                        Ok(())
                    }
                    _ => Err(stream_error(
                        "text delta does not match its block type",
                        *index,
                    )),
                })?;
            }
            StreamChunk::ReasoningDelta { index, text } => {
                self.with_open(*index, "reasoning delta", |block| match block {
                    OpenBlock::Reasoning(assembled) => {
                        assembled.push_str(text);
                        Ok(())
                    }
                    _ => Err(stream_error(
                        "reasoning delta does not match its block type",
                        *index,
                    )),
                })?;
            }
            StreamChunk::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta: _,
            } => {
                self.with_open(*index, "tool-call delta", |block| match block {
                    OpenBlock::ToolCall {
                        id: seen_id,
                        name: seen_name,
                    } => {
                        if seen_id.as_ref().is_some_and(|seen| seen != id) {
                            return Err(stream_error(
                                "tool-call deltas must retain one call id",
                                *index,
                            ));
                        }
                        if let (Some(seen), Some(incoming)) = (seen_name.as_ref(), name.as_ref()) {
                            if seen != incoming {
                                return Err(stream_error(
                                    "tool-call deltas must retain one tool name",
                                    *index,
                                ));
                            }
                        }
                        *seen_id = Some(id.clone());
                        if name.is_some() {
                            *seen_name = name.clone();
                        }
                        Ok(())
                    }
                    _ => Err(stream_error(
                        "tool-call delta does not match its block type",
                        *index,
                    )),
                })?;
            }
            StreamChunk::BlockEnd { index, block } => {
                let record = self.blocks.get_mut(index).ok_or_else(|| {
                    stream_error("block end requires a prior block start", *index)
                })?;
                let BlockStatus::Open(open) = &record.status else {
                    return Err(stream_error("blocks may only close once", *index));
                };
                record.status = BlockStatus::Closed(close_block(open, block, *index)?);
            }
            StreamChunk::Usage { usage } => {
                if self.usage.is_some() {
                    return Err(llm_error(
                        "DUPLICATE_LLM_USAGE",
                        "LLM streams may contain at most one usage chunk",
                        Value::Null,
                    ));
                }
                self.usage = Some(usage.clone());
            }
            StreamChunk::Finish {
                reason,
                replay_state,
            } => self.finish(reason.clone(), replay_state.clone())?,
        }

        self.chunks.push(chunk);
        if !self.finished {
            return Ok(None);
        }

        Ok(Some(LlmGeneration {
            message: Message {
                id: self.message_id.clone(),
                role: MessageRole::Assistant,
                content: self
                    .blocks
                    .values()
                    .filter_map(|record| match &record.status {
                        BlockStatus::Closed(block) => Some(block.clone()),
                        BlockStatus::Open(_) | BlockStatus::Omitted => None,
                    })
                    .collect(),
                source: MessageSource::Model {
                    provider: self.provider.clone(),
                    model: self.model.clone(),
                    replay_state: self.replay_state.clone(),
                },
            },
            usage: self.usage.clone(),
            finish_reason: self
                .finish_reason
                .as_ref()
                .expect("finished streams retain their finish reason")
                .clone(),
            chunks: self.chunks.clone(),
        }))
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn with_open(
        &mut self,
        index: u64,
        action: &str,
        operation: impl FnOnce(&mut OpenBlock) -> Result<(), TessivumError>,
    ) -> Result<(), TessivumError> {
        let record = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| stream_error(&format!("{action} requires an open block"), index))?;
        match &mut record.status {
            BlockStatus::Open(block) => operation(block),
            // Deltas can race a provider's terminal block end and are deliberately inert.
            BlockStatus::Closed(_) | BlockStatus::Omitted => Ok(()),
        }
    }

    fn finish(
        &mut self,
        reason: FinishReason,
        replay_state: Option<Value>,
    ) -> Result<(), TessivumError> {
        if matches!(reason, FinishReason::MaxTokens) {
            for record in self.blocks.values_mut() {
                let BlockStatus::Open(open) = &record.status else {
                    continue;
                };
                record.status = match open {
                    OpenBlock::Text(text) => {
                        BlockStatus::Closed(ContentBlock::Text { text: text.clone() })
                    }
                    OpenBlock::Reasoning(text) => {
                        BlockStatus::Closed(ContentBlock::Reasoning { text: text.clone() })
                    }
                    // An incomplete JSON argument stream is never made callable.
                    OpenBlock::ToolCall { .. } | OpenBlock::Direct(_) => BlockStatus::Omitted,
                };
            }
        } else if matches!(
            reason,
            FinishReason::Error { .. } | FinishReason::Aborted { .. }
        ) {
            for record in self.blocks.values_mut() {
                if matches!(record.status, BlockStatus::Open(_)) {
                    record.status = BlockStatus::Omitted;
                }
            }
        } else if self
            .blocks
            .values()
            .any(|record| matches!(record.status, BlockStatus::Open(_)))
        {
            return Err(llm_error(
                "LLM_FINISH_WITH_OPEN_BLOCK",
                "only max-tokens, error, or aborted may finish with an open LLM block",
                Value::Null,
            ));
        }

        self.finished = true;
        self.finish_reason = Some(reason);
        self.replay_state = replay_state;
        Ok(())
    }
}

fn close_block(
    open: &OpenBlock,
    terminal: &ContentBlock,
    index: u64,
) -> Result<ContentBlock, TessivumError> {
    match (open, terminal) {
        (OpenBlock::Text(_), ContentBlock::Text { .. })
        | (OpenBlock::Reasoning(_), ContentBlock::Reasoning { .. }) => Ok(terminal.clone()),
        (
            OpenBlock::ToolCall { id, name },
            ContentBlock::ToolCall {
                id: terminal_id,
                name: terminal_name,
                ..
            },
        ) => {
            if id.as_ref().is_some_and(|seen| seen != terminal_id) {
                return Err(stream_error(
                    "tool-call block end must retain the delta call id",
                    index,
                ));
            }
            if name.as_ref().is_some_and(|seen| seen != terminal_name) {
                return Err(stream_error(
                    "tool-call block end must retain the delta tool name",
                    index,
                ));
            }
            Ok(terminal.clone())
        }
        (OpenBlock::Direct(block_type), block) if block_type == block.type_tag() => {
            Ok(block.clone())
        }
        _ => Err(stream_error(
            "block end does not match its started block type",
            index,
        )),
    }
}

/// A route in the legacy raw-provider JSONL format.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReplayRoute {
    session_id: Option<SessionId>,
    provider: String,
    model: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayLine {
    #[serde(default)]
    session_id: Option<SessionId>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    chunk: Option<StreamChunk>,
    #[serde(default)]
    chunks: Vec<StreamChunk>,
}

/// Header facts that order recorded parent and child session scripts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayHeader {
    pub id: SessionId,
    pub created_at: u64,
    pub seed_length: u64,
}

/// One terminal provider attempt scripted by durable JSONL or an override.
///
/// `Hang` emits the canonical partial text prefix then waits for cancellation.
/// Its optional ready file is written only after that prefix is consumed.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplayEntry {
    Chunks {
        chunks: Vec<StreamChunk>,
    },
    Throw {
        chunks: Vec<StreamChunk>,
        message: String,
        code: String,
    },
    Hang {
        chunks: Vec<StreamChunk>,
        ready_file: Option<String>,
    },
}

/// One ordered recorded session script.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayScript {
    pub header: ReplayHeader,
    pub entries: Vec<ReplayEntry>,
}

/// One positional override of a derived primary-session entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayOverridePatch {
    pub at: usize,
    pub entry: ReplayEntry,
}

/// A whole script replacement or positional patches over a derived script.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplayOverride {
    Replace(Vec<ReplayEntry>),
    Patches(Vec<ReplayOverridePatch>),
}

#[derive(Clone, Debug)]
enum ReplayAttempt {
    Chunks(Arc<[StreamChunk]>),
    Throw {
        chunks: Arc<[StreamChunk]>,
        error: TessivumError,
    },
    Hang {
        chunks: Arc<[StreamChunk]>,
        ready_file: Option<String>,
    },
}

impl ReplayAttempt {
    fn has_request_placeholder(&self) -> bool {
        match self {
            Self::Chunks(chunks) => chunks_have_placeholder(chunks),
            Self::Throw { chunks, error } => {
                chunks_have_placeholder(chunks)
                    || error.message.contains(FROM_REQUEST_OPEN)
                    || error.code.contains(FROM_REQUEST_OPEN)
            }
            Self::Hang { chunks, .. } => chunks_have_placeholder(chunks),
        }
    }
}

#[derive(Clone, Debug)]
struct BoundScript {
    script: usize,
    cursor: usize,
}

#[derive(Debug, Default)]
struct ReplayCursors {
    routes: BTreeMap<ReplayRoute, usize>,
    scripts: BTreeMap<Option<SessionId>, BoundScript>,
    next_script: usize,
}

#[derive(Debug)]
enum ReplayPlan {
    Routes(BTreeMap<ReplayRoute, Vec<ReplayAttempt>>),
    Scripts(Vec<ReplayScriptInternal>),
}

#[derive(Debug)]
struct ReplayScriptInternal {
    header: ReplayHeader,
    entries: Vec<ReplayAttempt>,
}

const FROM_REQUEST_OPEN: &str = "{{fromRequest:";
const FROM_REQUEST_CLOSE: &str = "}}";

/// A provider-neutral recorded adapter. It supports both the original
/// raw-provider JSONL fixture and durable session JSONL scripts.
#[derive(Debug)]
pub struct RecordedLlmAdapter {
    plan: Arc<ReplayPlan>,
    cursors: Arc<Mutex<ReplayCursors>>,
    pace: Duration,
}

impl Default for RecordedLlmAdapter {
    fn default() -> Self {
        Self {
            plan: Arc::new(ReplayPlan::Routes(BTreeMap::new())),
            cursors: Arc::new(Mutex::new(ReplayCursors::default())),
            pace: Duration::ZERO,
        }
    }
}

impl Clone for RecordedLlmAdapter {
    fn clone(&self) -> Self {
        Self {
            plan: Arc::clone(&self.plan),
            cursors: Arc::new(Mutex::new(ReplayCursors::default())),
            pace: self.pace,
        }
    }
}

impl RecordedLlmAdapter {
    /// Parses one raw-provider JSON object per line. Each object must name its
    /// provider and model unless supplied through `from_jsonl_with_route`.
    pub fn from_jsonl(recording: &str) -> Result<Self, TessivumError> {
        Self::parse_jsonl(recording, None)
    }

    /// Parses raw-provider recordings, supplying route values omitted by a
    /// protocol-level fixture.
    pub fn from_jsonl_with_route(
        recording: &str,
        session_id: Option<SessionId>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, TessivumError> {
        Self::parse_jsonl(
            recording,
            Some(ReplayRoute {
                session_id,
                provider: provider.into(),
                model: model.into(),
            }),
        )
    }

    /// Parses a durable session JSONL into one first-call-bound replay script.
    pub fn from_session_jsonl(recording: &str) -> Result<Self, TessivumError> {
        Self::from_session_jsonls_with_override(recording, &[], None)
    }

    /// Parses a durable primary log plus child logs. Child scripts bind by their
    /// first live request after the primary script, with independent cursors.
    pub fn from_session_jsonls(primary: &str, children: &[&str]) -> Result<Self, TessivumError> {
        Self::from_session_jsonls_with_override(primary, children, None)
    }

    /// Parses a primary durable script with a JSON override document.
    pub fn from_session_jsonl_with_override(
        recording: &str,
        override_json: &str,
    ) -> Result<Self, TessivumError> {
        Self::from_session_jsonls_with_override(recording, &[], Some(override_json))
    }

    /// Parses primary and child durable scripts, applying the override only to
    /// the primary script as the frozen replay contract specifies.
    pub fn from_session_jsonls_with_override(
        primary: &str,
        children: &[&str],
        override_json: Option<&str>,
    ) -> Result<Self, TessivumError> {
        let mut primary = parse_replay_script(primary)?;
        if let Some(override_json) = override_json {
            primary.entries =
                apply_replay_override(primary.entries, parse_replay_override(override_json)?)?;
        }
        let mut scripts = Vec::with_capacity(children.len() + 1);
        scripts.push(primary);
        let mut children = children
            .iter()
            .enumerate()
            .map(|(index, recording)| {
                let header = parse_replay_header(recording)?;
                let seed_length = usize::try_from(header.seed_length).map_err(|_| {
                    replay_error(
                        "a child replay seedLength does not fit this platform",
                        json!({"child": index + 1, "seedLength": header.seed_length}),
                    )
                })?;
                let events = parse_replay_session_log(recording)?;
                if seed_length > events.len() {
                    return Err(replay_error(
                        "a child replay seedLength exceeds its recorded events",
                        json!({"child": index + 1, "seedLength": seed_length, "events": events.len()}),
                    ));
                }
                Ok(ReplayScript {
                    header,
                    entries: derive_replay_script(&events[seed_length..])?,
                })
            })
            .collect::<Result<Vec<_>, TessivumError>>()?;
        children.sort_by(|left, right| {
            left.header
                .created_at
                .cmp(&right.header.created_at)
                .then_with(|| left.header.id.cmp(&right.header.id))
        });
        scripts.extend(children);
        Self::from_session_scripts(scripts)
    }

    /// Installs already parsed durable scripts. Scripts are claimed once by a
    /// live session at its first request, preserving per-session ordering.
    pub fn from_session_scripts(scripts: Vec<ReplayScript>) -> Result<Self, TessivumError> {
        let scripts = scripts
            .into_iter()
            .enumerate()
            .map(|(script_index, script)| {
                let entries = script
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(entry_index, entry)| {
                        compile_replay_entry(
                            entry,
                            json!({"script": script_index + 1, "entry": entry_index + 1}),
                        )
                    })
                    .collect::<Result<Vec<_>, TessivumError>>()?;
                Ok(ReplayScriptInternal {
                    header: script.header,
                    entries,
                })
            })
            .collect::<Result<Vec<_>, TessivumError>>()?;
        Ok(Self {
            plan: Arc::new(ReplayPlan::Scripts(scripts)),
            cursors: Arc::new(Mutex::new(ReplayCursors::default())),
            pace: Duration::ZERO,
        })
    }

    /// Applies a fixed delay before every recorded chunk. The wait is part of
    /// the raw stream and is interrupted by the request cancellation token.
    pub fn with_pace_ms(mut self, pace_ms: u64) -> Self {
        self.pace = Duration::from_millis(pace_ms);
        self
    }

    /// Returns every cursor and first-call binding to the start of its plan.
    pub fn reset(&self) {
        *lock(&self.cursors) = ReplayCursors::default();
    }

    /// Refuses silent fixture underruns. A started request owns one script
    /// slot, even if its consumer later cancels the stream.
    pub fn assert_consumed(&self) -> Result<(), TessivumError> {
        let cursors = lock(&self.cursors);
        match self.plan.as_ref() {
            ReplayPlan::Routes(routes) => {
                let pending = routes
                    .iter()
                    .filter_map(|(route, attempts)| {
                        let cursor = cursors.routes.get(route).copied().unwrap_or_default();
                        (cursor < attempts.len()).then(|| {
                            json!({
                                "sessionId": route.session_id,
                                "provider": route.provider,
                                "model": route.model,
                                "consumed": cursor,
                                "recorded": attempts.len(),
                            })
                        })
                    })
                    .collect::<Vec<_>>();
                if pending.is_empty() {
                    Ok(())
                } else {
                    Err(replay_error(
                        "recorded LLM replay has trailing unconsumed attempts",
                        json!({"routes": pending}),
                    ))
                }
            }
            ReplayPlan::Scripts(scripts) => {
                let mut problems = Vec::new();
                if cursors.next_script < scripts.len() {
                    problems.push(json!({
                        "unboundScripts": scripts.len() - cursors.next_script,
                    }));
                }
                for (session, binding) in &cursors.scripts {
                    let script = &scripts[binding.script];
                    if binding.cursor < script.entries.len() {
                        problems.push(json!({
                            "sessionId": session,
                            "recordedId": script.header.id,
                            "consumed": binding.cursor,
                            "recorded": script.entries.len(),
                        }));
                    }
                }
                if problems.is_empty() {
                    Ok(())
                } else {
                    Err(replay_error(
                        "recorded LLM replay has trailing unconsumed attempts",
                        json!({"scripts": problems}),
                    ))
                }
            }
        }
    }

    fn parse_jsonl(
        recording: &str,
        default_route: Option<ReplayRoute>,
    ) -> Result<Self, TessivumError> {
        let mut routes = BTreeMap::<ReplayRoute, Vec<(Option<String>, Vec<StreamChunk>)>>::new();
        for (line_number, line) in recording.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let line: ReplayLine = serde_json::from_str(line).map_err(|error| {
                replay_error(
                    "a recorded LLM replay line is not valid JSON",
                    json!({"line": line_number + 1, "error": error.to_string()}),
                )
            })?;
            let route = ReplayRoute {
                session_id: line.session_id.or_else(|| {
                    default_route
                        .as_ref()
                        .and_then(|route| route.session_id.clone())
                }),
                provider: line
                    .provider
                    .or_else(|| default_route.as_ref().map(|route| route.provider.clone()))
                    .filter(|provider| !provider.trim().is_empty())
                    .ok_or_else(|| {
                        replay_error(
                            "a replay line must name a provider",
                            json!({"line": line_number + 1}),
                        )
                    })?,
                model: line
                    .model
                    .or_else(|| default_route.as_ref().map(|route| route.model.clone()))
                    .filter(|model| !model.trim().is_empty())
                    .ok_or_else(|| {
                        replay_error(
                            "a replay line must name a model",
                            json!({"line": line_number + 1}),
                        )
                    })?,
            };
            let mut chunks = line.chunks;
            if let Some(chunk) = line.chunk {
                if !chunks.is_empty() {
                    return Err(replay_error(
                        "a replay entry must contain either chunk or chunks, not both",
                        json!({"line": line_number + 1}),
                    ));
                }
                chunks.push(chunk);
            }
            if chunks.is_empty() {
                return Err(replay_error(
                    "a replay entry must contain at least one chunk",
                    json!({"line": line_number + 1}),
                ));
            }
            let attempts = routes.entry(route).or_default();
            if attempts.is_empty()
                || attempts
                    .last()
                    .is_some_and(|(id, _)| id != &line.request_id)
            {
                attempts.push((line.request_id, Vec::new()));
            }
            attempts
                .last_mut()
                .expect("an attempt is present after insertion")
                .1
                .append(&mut chunks);
        }
        let routes = routes
            .into_iter()
            .map(|(route, attempts)| {
                let attempts = attempts
                    .into_iter()
                    .enumerate()
                    .map(|(attempt_index, (_, chunks))| {
                        compile_replay_entry(
                            &ReplayEntry::Chunks { chunks },
                            json!({
                                "sessionId": route.session_id,
                                "provider": route.provider,
                                "model": route.model,
                                "attempt": attempt_index + 1,
                            }),
                        )
                    })
                    .collect::<Result<Vec<_>, TessivumError>>()?;
                Ok((route, attempts))
            })
            .collect::<Result<BTreeMap<_, _>, TessivumError>>()?;
        Ok(Self {
            plan: Arc::new(ReplayPlan::Routes(routes)),
            cursors: Arc::new(Mutex::new(ReplayCursors::default())),
            pace: Duration::ZERO,
        })
    }

    fn claim_attempt(
        &self,
        request: &GenerateRequest,
        cancellation: &CancellationToken,
    ) -> Result<ReplayAttempt, TessivumError> {
        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        let mut cursors = lock(&self.cursors);
        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        match self.plan.as_ref() {
            ReplayPlan::Routes(routes) => {
                let route = ReplayRoute {
                    session_id: request.session_id.clone(),
                    provider: request.provider.clone(),
                    model: request.model.clone(),
                };
                let attempts = routes.get(&route).ok_or_else(|| {
                    llm_error(
                        "RECORDED_LLM_ROUTE_NOT_FOUND",
                        "no recorded LLM replay matches this session, provider, and model",
                        json!({
                            "sessionId": route.session_id,
                            "provider": route.provider,
                            "model": route.model,
                        }),
                    )
                })?;
                let cursor = cursors.routes.get(&route).copied().unwrap_or_default();
                let attempt = attempts.get(cursor).cloned().ok_or_else(|| {
                    llm_error(
                        "RECORDED_LLM_EXHAUSTED",
                        "all recorded LLM replay attempts for this route have been consumed",
                        json!({
                            "sessionId": route.session_id,
                            "provider": route.provider,
                            "model": route.model,
                            "attempt": cursor + 1,
                        }),
                    )
                })?;
                cursors.routes.insert(route, cursor + 1);
                resolve_replay_attempt(attempt, &request.messages)
            }
            ReplayPlan::Scripts(scripts) => {
                let session = request.session_id.clone();
                if !cursors.scripts.contains_key(&session) {
                    let script = cursors.next_script;
                    if script == scripts.len() {
                        return Err(llm_error(
                            "RECORDED_LLM_SESSION_NOT_FOUND",
                            "a model call arrived from an unrecorded session",
                            json!({"sessionId": session, "recordedSessions": scripts.len()}),
                        ));
                    }
                    cursors.next_script += 1;
                    cursors
                        .scripts
                        .insert(session.clone(), BoundScript { script, cursor: 0 });
                }
                let binding = cursors
                    .scripts
                    .get_mut(&session)
                    .expect("a session was inserted or was already bound");
                let script = &scripts[binding.script];
                let cursor = binding.cursor;
                let attempt = script.entries.get(cursor).cloned().ok_or_else(|| {
                    llm_error(
                        "RECORDED_LLM_EXHAUSTED",
                        "all recorded LLM replay attempts for this session have been consumed",
                        json!({
                            "sessionId": session,
                            "recordedId": script.header.id,
                            "attempt": cursor + 1,
                            "recorded": script.entries.len(),
                        }),
                    )
                })?;
                binding.cursor += 1;
                resolve_replay_attempt(attempt, &request.messages)
            }
        }
    }
}

#[async_trait]
impl LlmAdapter for RecordedLlmAdapter {
    async fn generate(
        &self,
        request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        let attempt = self.claim_attempt(&request, &cancellation)?;
        Ok(replay_stream(attempt, self.pace, cancellation))
    }
}

/// Parses line zero of a durable JSONL fixture. Header fields intentionally
/// default like the frozen replay loader, because they only order scripts.
pub fn parse_replay_header(recording: &str) -> Result<ReplayHeader, TessivumError> {
    let Some((line_number, line)) = recording
        .lines()
        .enumerate()
        .find(|(_, line)| !line.trim().is_empty())
    else {
        return Ok(ReplayHeader {
            id: SessionId::from(""),
            created_at: 0,
            seed_length: 0,
        });
    };
    let value: Value = serde_json::from_str(line).map_err(|error| {
        replay_error(
            "the replay session header is not valid JSON",
            json!({"line": line_number + 1, "error": error.to_string()}),
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        replay_error(
            "the replay session header must be a JSON object",
            json!({"line": line_number + 1}),
        )
    })?;
    if object
        .get("type")
        .is_some_and(|kind| kind.as_str() != Some("session"))
    {
        return Err(replay_error(
            "the first replay JSONL record must have type=session",
            json!({"line": line_number + 1}),
        ));
    }
    Ok(ReplayHeader {
        id: object
            .get("id")
            .and_then(Value::as_str)
            .map(SessionId::from)
            .unwrap_or_else(|| SessionId::from("")),
        created_at: optional_safe_integer(object.get("createdAt"), "createdAt", line_number + 1)?,
        seed_length: optional_safe_integer(
            object.get("seedLength"),
            "seedLength",
            line_number + 1,
        )?,
    })
}

/// Parses one durable session JSONL into raw event envelopes after its header.
pub fn parse_replay_session_log(recording: &str) -> Result<Vec<SessionEvent>, TessivumError> {
    let Some((header_index, _)) = recording
        .lines()
        .enumerate()
        .find(|(_, line)| !line.trim().is_empty())
    else {
        return Ok(Vec::new());
    };
    let mut events = Vec::new();
    for (line_number, line) in recording.lines().enumerate().skip(header_index + 1) {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|error| {
            replay_error(
                "a replay session event is not valid JSON",
                json!({"line": line_number + 1, "error": error.to_string()}),
            )
        })?;
        let packed_kind = match value.get("type").and_then(Value::as_str) {
            Some("text-chunks") => Some("text-chunks"),
            Some("reasoning-chunks") => Some("reasoning-chunks"),
            Some("tool-call-chunks") => Some("tool-call-chunks"),
            _ => None,
        };
        if let Some(kind) = packed_kind {
            events.extend(unpack_chunks(value, line_number + 1, kind)?);
            continue;
        }
        let event: SessionEvent = serde_json::from_value(value).map_err(|error| {
            replay_error(
                "a replay session event has an invalid wire shape",
                json!({"line": line_number + 1, "error": error.to_string()}),
            )
        })?;
        event.validate().map_err(|error| {
            replay_error(
                "a replay session event violates the durable protocol",
                json!({"line": line_number + 1, "error": error.message}),
            )
        })?;
        events.push(event);
    }
    Ok(events)
}

/// Derives ordered raw provider attempts from durable agent-loop events.
pub fn derive_replay_script(events: &[SessionEvent]) -> Result<Vec<ReplayEntry>, TessivumError> {
    let mut entries = Vec::new();
    let mut key = None;
    let mut chunks = Vec::new();
    for event in events {
        if event.event_type == "compaction/summary" {
            close_derived_attempt(&mut entries, &mut key, &mut chunks)?;
            if event.data.get("llmStreamCall") == Some(&Value::Bool(true)) {
                let raw_output = event.data.get("rawOutput").ok_or_else(|| {
                    replay_error(
                        "a compaction LLM replay entry is missing rawOutput",
                        json!({"seq": event.seq}),
                    )
                })?;
                let output = serde_json::from_value::<Vec<ContentBlock>>(raw_output.clone())
                    .map_err(|error| {
                        replay_error(
                            "a compaction LLM replay rawOutput is malformed",
                            json!({"seq": event.seq, "error": error.to_string()}),
                        )
                    })?;
                let mut canonical = Vec::with_capacity(output.len() * 2 + 2);
                for (index, block) in output.into_iter().enumerate() {
                    let index = u64::try_from(index).map_err(|_| {
                        replay_error(
                            "a compaction replay has too many blocks",
                            json!({"seq": event.seq}),
                        )
                    })?;
                    canonical.push(StreamChunk::BlockStart {
                        index,
                        block_type: block_type(&block).into(),
                    });
                    canonical.push(StreamChunk::BlockEnd { index, block });
                }
                if let Some(usage) = event.data.get("usage") {
                    canonical.push(StreamChunk::Usage {
                        usage: serde_json::from_value(usage.clone()).map_err(|error| {
                            replay_error(
                                "a compaction LLM replay usage is malformed",
                                json!({"seq": event.seq, "error": error.to_string()}),
                            )
                        })?,
                    });
                }
                canonical.push(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                    replay_state: None,
                });
                entries.push(ReplayEntry::Chunks { chunks: canonical });
            }
            continue;
        }
        if event.event_type != "assistant/chunk" {
            continue;
        }
        let frame: ReplayChunkEvent =
            serde_json::from_value(event.data.clone()).map_err(|error| {
                replay_error(
                    "an assistant/chunk replay event is malformed",
                    json!({"seq": event.seq, "error": error.to_string()}),
                )
            })?;
        frame.chunk.validate().map_err(|error| {
            replay_error(
                "an assistant/chunk replay event has an invalid chunk",
                json!({"seq": event.seq, "error": error.message}),
            )
        })?;
        let next_key = (frame.turn, frame.step);
        if !chunks.is_empty() && key != Some(next_key) {
            close_derived_attempt(&mut entries, &mut key, &mut chunks)?;
        }
        if chunks.is_empty() {
            key = Some(next_key);
        }
        let is_finish = matches!(frame.chunk, StreamChunk::Finish { .. });
        chunks.push(frame.chunk);
        if is_finish {
            close_derived_attempt(&mut entries, &mut key, &mut chunks)?;
        }
    }
    close_derived_attempt(&mut entries, &mut key, &mut chunks)?;
    Ok(entries)
}

/// Parses and derives one durable session script.
pub fn parse_replay_script(recording: &str) -> Result<ReplayScript, TessivumError> {
    Ok(ReplayScript {
        header: parse_replay_header(recording)?,
        entries: derive_replay_script(&parse_replay_session_log(recording)?)?,
    })
}

/// Parses the frozen replacement-or-patches override document.
pub fn parse_replay_override(document: &str) -> Result<ReplayOverride, TessivumError> {
    let value: Value = serde_json::from_str(document).map_err(|error| {
        replay_error(
            "a replay override is not valid JSON",
            json!({"error": error.to_string()}),
        )
    })?;
    if let Some(entries) = value.as_array() {
        return entries
            .iter()
            .enumerate()
            .map(|(index, entry)| read_replay_entry(entry, format!("entry {index}")))
            .collect::<Result<Vec<_>, _>>()
            .map(ReplayOverride::Replace);
    }
    let object = value.as_object().ok_or_else(|| {
        replay_error(
            "a replay override must be an entry array or an object with patches",
            Value::Null,
        )
    })?;
    if object.len() != 1 || !object.contains_key("patches") {
        return Err(replay_error(
            "a replay override must be an entry array or an object with patches",
            Value::Null,
        ));
    }
    let patches = object
        .get("patches")
        .and_then(Value::as_array)
        .ok_or_else(|| replay_error("replay override patches must be an array", Value::Null))?;
    let mut seen = BTreeSet::new();
    patches
        .iter()
        .enumerate()
        .map(|(index, patch)| {
            let location = format!("patch {index}");
            let object = patch.as_object().ok_or_else(|| {
                replay_error(
                    "a replay override patch must be an object",
                    json!({"location": location}),
                )
            })?;
            if object.len() != 2 || !object.contains_key("at") || !object.contains_key("entry") {
                return Err(replay_error(
                    "a replay override patch must contain exactly at and entry",
                    json!({"location": location}),
                ));
            }
            let at = object.get("at").and_then(Value::as_u64).ok_or_else(|| {
                replay_error(
                    "a replay override patch index must be a non-negative safe integer",
                    json!({"location": location}),
                )
            })?;
            let at = usize::try_from(at).map_err(|_| {
                replay_error(
                    "a replay override patch index does not fit this platform",
                    json!({"location": location}),
                )
            })?;
            if !seen.insert(at) {
                return Err(replay_error(
                    "a replay override cannot contain duplicate patch indexes",
                    json!({"at": at}),
                ));
            }
            Ok(ReplayOverridePatch {
                at,
                entry: read_replay_entry(
                    object.get("entry").expect("entry presence was checked"),
                    format!("{location}.entry"),
                )?,
            })
        })
        .collect::<Result<Vec<_>, TessivumError>>()
        .map(ReplayOverride::Patches)
}

fn apply_replay_override(
    mut derived: Vec<ReplayEntry>,
    override_doc: ReplayOverride,
) -> Result<Vec<ReplayEntry>, TessivumError> {
    match override_doc {
        ReplayOverride::Replace(entries) => Ok(entries),
        ReplayOverride::Patches(patches) => {
            let derived_len = derived.len();
            for patch in patches {
                if patch.at > derived_len {
                    return Err(replay_error(
                        "a replay override patch index is out of range",
                        json!({"at": patch.at, "recorded": derived_len}),
                    ));
                }
                if patch.at == derived_len {
                    derived.push(patch.entry);
                } else {
                    derived[patch.at] = patch.entry;
                }
            }
            Ok(derived)
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayChunkEvent {
    turn: u64,
    step: u64,
    chunk: StreamChunk,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackedChunks {
    seq0: u64,
    time0: u64,
    data: PackedChunkData,
}

#[derive(Deserialize)]
struct PackedChunkData {
    turn: u64,
    step: u64,
    index: u64,
    dt: Vec<u64>,
    texts: Option<Vec<String>>,
    args: Option<Vec<String>>,
    id: Option<ToolCallId>,
    name: Option<String>,
}

fn unpack_chunks(
    value: Value,
    line: usize,
    kind: &str,
) -> Result<Vec<SessionEvent>, TessivumError> {
    let packed: PackedChunks = serde_json::from_value(value).map_err(|error| {
        replay_error(
            "a packed replay chunk row is malformed",
            json!({"line": line, "error": error.to_string()}),
        )
    })?;
    let PackedChunks { seq0, time0, data } = packed;
    let PackedChunkData {
        turn,
        step,
        index,
        dt,
        texts,
        args,
        id,
        name,
    } = data;
    let members = match kind {
        "text-chunks" | "reasoning-chunks" => texts,
        "tool-call-chunks" if id.is_some() => args,
        _ => None,
    }
    .ok_or_else(|| {
        replay_error(
            "a packed replay chunk row has invalid data",
            json!({"line": line}),
        )
    })?;
    if members.is_empty() || dt.len() != members.len() - 1 {
        return Err(replay_error(
            "a packed replay chunk row has mismatched dt and member lengths",
            json!({"line": line}),
        ));
    }
    let mut events = Vec::with_capacity(members.len());
    let mut elapsed = 0_u64;
    for (offset, member) in members.into_iter().enumerate() {
        let offset = u64::try_from(offset).map_err(|_| {
            replay_error(
                "a packed replay chunk row is too large",
                json!({"line": line}),
            )
        })?;
        let seq = seq0.checked_add(offset).ok_or_else(|| {
            replay_error(
                "a packed replay chunk sequence overflows",
                json!({"line": line}),
            )
        })?;
        if offset != 0 {
            elapsed = elapsed
                .checked_add(dt[offset as usize - 1])
                .ok_or_else(|| {
                    replay_error(
                        "a packed replay chunk time overflows",
                        json!({"line": line}),
                    )
                })?;
        }
        let time = time0.checked_add(elapsed).ok_or_else(|| {
            replay_error(
                "a packed replay chunk time overflows",
                json!({"line": line}),
            )
        })?;
        let chunk = match kind {
            "text-chunks" => StreamChunk::TextDelta {
                index,
                text: member,
            },
            "reasoning-chunks" => StreamChunk::ReasoningDelta {
                index,
                text: member,
            },
            "tool-call-chunks" => StreamChunk::ToolCallDelta {
                index,
                id: id.clone().expect("validated packed tool call id"),
                name: name.clone(),
                arguments_delta: member,
            },
            _ => unreachable!("validated packed chunk kind"),
        };
        events.push(SessionEvent {
            event_type: "assistant/chunk".into(),
            seq,
            time,
            data: json!({"turn": turn, "step": step, "chunk": chunk}),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
        });
    }
    Ok(events)
}

fn close_derived_attempt(
    entries: &mut Vec<ReplayEntry>,
    key: &mut Option<(u64, u64)>,
    chunks: &mut Vec<StreamChunk>,
) -> Result<(), TessivumError> {
    if chunks.is_empty() {
        return Ok(());
    }
    if !matches!(chunks.last(), Some(StreamChunk::Finish { .. })) {
        let (turn, step) = key.expect("a non-empty derived attempt has a turn and step");
        return Err(replay_error(
            "a recorded model call ended without a finish chunk; use an explicit replay override",
            json!({"turn": turn, "step": step}),
        ));
    }
    entries.push(ReplayEntry::Chunks {
        chunks: std::mem::take(chunks),
    });
    *key = None;
    Ok(())
}

fn block_type(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text { .. } => "text",
        ContentBlock::Reasoning { .. } => "reasoning",
        ContentBlock::Image { .. } => "image",
        ContentBlock::ToolCall { .. } => "tool-call",
        ContentBlock::ToolResult { .. } => "tool-result",
    }
}

fn optional_safe_integer(
    value: Option<&Value>,
    field: &str,
    line: usize,
) -> Result<u64, TessivumError> {
    match value {
        None => Ok(0),
        Some(value) => value
            .as_u64()
            .filter(|value| *value <= crate::MAX_SAFE_INTEGER)
            .ok_or_else(|| {
                replay_error(
                    "a replay header numeric field is invalid",
                    json!({"line": line, "field": field}),
                )
            }),
    }
}

fn read_replay_entry(value: &Value, location: String) -> Result<ReplayEntry, TessivumError> {
    let object = value.as_object().ok_or_else(|| {
        replay_error(
            "a replay override entry must be an object",
            json!({"location": location}),
        )
    })?;
    let kind = object.get("kind").and_then(Value::as_str).ok_or_else(|| {
        replay_error(
            "a replay override entry must name its kind",
            json!({"location": location}),
        )
    })?;
    match kind {
        "chunks" => {
            require_exact_keys(object, &["kind", "chunks"], &location)?;
            Ok(ReplayEntry::Chunks {
                chunks: read_replay_chunks(object.get("chunks"), &location)?,
            })
        }
        "throw" => {
            require_exact_keys(object, &["kind", "chunks", "message", "code"], &location)?;
            let message = nonempty_string(object.get("message"), "message", &location)?;
            let code = nonempty_string(object.get("code"), "code", &location)?;
            Ok(ReplayEntry::Throw {
                chunks: read_replay_chunks(object.get("chunks"), &location)?,
                message,
                code,
            })
        }
        "hang" => {
            if !object
                .keys()
                .all(|key| matches!(key.as_str(), "kind" | "chunks" | "readyFile"))
            {
                return Err(replay_error(
                    "a replay hang entry has invalid fields",
                    json!({"location": location}),
                ));
            }
            let chunks = object
                .get("chunks")
                .map(|value| read_replay_chunks(Some(value), &location))
                .transpose()?
                .unwrap_or_default();
            let ready_file = object
                .get("readyFile")
                .map(|value| nonempty_string(Some(value), "readyFile", &location))
                .transpose()?;
            Ok(ReplayEntry::Hang { chunks, ready_file })
        }
        _ => Err(replay_error(
            "a replay override entry has an unknown kind",
            json!({"location": location, "kind": kind}),
        )),
    }
}

fn require_exact_keys(
    object: &Map<String, Value>,
    keys: &[&str],
    location: &str,
) -> Result<(), TessivumError> {
    if object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key)) {
        Ok(())
    } else {
        Err(replay_error(
            "a replay override entry has invalid fields",
            json!({"location": location}),
        ))
    }
}

fn nonempty_string(
    value: Option<&Value>,
    field: &str,
    location: &str,
) -> Result<String, TessivumError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            replay_error(
                "a replay override string field must be non-empty",
                json!({"location": location, "field": field}),
            )
        })
}

fn read_replay_chunks(
    value: Option<&Value>,
    location: &str,
) -> Result<Vec<StreamChunk>, TessivumError> {
    let chunks = value
        .as_ref()
        .and_then(|value| value.as_array())
        .ok_or_else(|| {
            replay_error(
                "replay override chunks must be an array",
                json!({"location": location}),
            )
        })?;
    chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let kind = chunk
                .as_object()
                .and_then(|object| object.get("type"))
                .and_then(Value::as_str)
                .filter(|kind| {
                    matches!(
                        *kind,
                        "block-start"
                            | "text-delta"
                            | "reasoning-delta"
                            | "tool-call-delta"
                            | "block-end"
                            | "usage"
                            | "finish"
                    )
                })
                .ok_or_else(|| {
                    replay_error(
                        "a replay override chunk must have a known type",
                        json!({"location": location, "chunk": index}),
                    )
                })?;
            let chunk: StreamChunk = serde_json::from_value(chunk.clone()).map_err(|error| {
                replay_error(
                    "a replay override chunk is malformed",
                    json!({"location": location, "chunk": index, "type": kind, "error": error.to_string()}),
                )
            })?;
            chunk.validate().map_err(|error| {
                replay_error(
                    "a replay override chunk violates the provider wire contract",
                    json!({"location": location, "chunk": index, "error": error.message}),
                )
            })?;
            Ok(chunk)
        })
        .collect()
}

fn compile_replay_entry(
    entry: &ReplayEntry,
    details: Value,
) -> Result<ReplayAttempt, TessivumError> {
    match entry {
        ReplayEntry::Chunks { chunks } => {
            validate_success_chunks(chunks, details)?;
            Ok(ReplayAttempt::Chunks(Arc::from(chunks.clone())))
        }
        ReplayEntry::Throw {
            chunks,
            message,
            code,
        } => {
            if message.is_empty() || code.is_empty() {
                return Err(replay_error(
                    "a thrown replay entry requires non-empty message and code",
                    details,
                ));
            }
            validate_throw_prefix(chunks, details.clone())?;
            Ok(ReplayAttempt::Throw {
                chunks: Arc::from(chunks.clone()),
                error: TessivumError::new(code, message, "llm", details),
            })
        }
        ReplayEntry::Hang { chunks, ready_file } => {
            if !chunks.is_empty() {
                validate_throw_prefix(chunks, details)?;
            }
            Ok(ReplayAttempt::Hang {
                chunks: Arc::from(chunks.clone()),
                ready_file: ready_file.clone(),
            })
        }
    }
}

fn validate_success_chunks(chunks: &[StreamChunk], details: Value) -> Result<(), TessivumError> {
    if chunks.is_empty() {
        return Err(replay_error(
            "a successful replay attempt must contain a terminal finish chunk",
            details,
        ));
    }
    let finish_count = chunks
        .iter()
        .filter(|chunk| matches!(chunk, StreamChunk::Finish { .. }))
        .count();
    if finish_count != 1 || !matches!(chunks.last(), Some(StreamChunk::Finish { .. })) {
        return Err(replay_error(
            "a successful replay attempt must contain exactly one final finish chunk",
            details,
        ));
    }
    validate_chunk_sequence(chunks, details)
}

fn validate_throw_prefix(chunks: &[StreamChunk], details: Value) -> Result<(), TessivumError> {
    if chunks
        .iter()
        .any(|chunk| matches!(chunk, StreamChunk::Finish { .. }))
    {
        return Err(replay_error(
            "a thrown replay attempt cannot contain a finish chunk",
            details,
        ));
    }
    validate_chunk_sequence(chunks, details)
}

fn validate_chunk_sequence(chunks: &[StreamChunk], details: Value) -> Result<(), TessivumError> {
    for (index, chunk) in chunks.iter().enumerate() {
        chunk.validate().map_err(|error| {
            replay_error(
                "a replay chunk violates the provider wire contract",
                json!({"attempt": details, "chunk": index + 1, "error": error.message}),
            )
        })?;
    }
    Ok(())
}

fn replay_stream(
    attempt: ReplayAttempt,
    pace: Duration,
    cancellation: CancellationToken,
) -> LlmStream {
    Box::pin(async_stream::try_stream! {
        match attempt {
            ReplayAttempt::Chunks(chunks) => {
                for chunk in chunks.iter().cloned() {
                    wait_for_replay_pace(pace, &cancellation).await?;
                    yield chunk;
                }
            }
            ReplayAttempt::Throw { chunks, error } => {
                for chunk in chunks.iter().cloned() {
                    wait_for_replay_pace(pace, &cancellation).await?;
                    yield chunk;
                }
                Err(error)?;
            }
            ReplayAttempt::Hang { chunks, ready_file } => {
                if chunks.is_empty() {
                    yield StreamChunk::BlockStart {
                        index: 0,
                        block_type: "text".into(),
                    };
                    yield StreamChunk::TextDelta {
                        index: 0,
                        text: "partial".into(),
                    };
                } else {
                    for chunk in chunks.iter().cloned() {
                        wait_for_replay_pace(pace, &cancellation).await?;
                        yield chunk;
                    }
                }
                if let Some(path) = ready_file {
                    tokio::fs::write(&path, []).await.map_err(|error| TessivumError::new(
                        "REPLAY_READY_FILE_WRITE_FAILED",
                        format!("could not write replay ready file {path}: {error}"),
                        "llm",
                        Value::Null,
                    ))?;
                }
                cancellation.cancelled().await;
                Err(cancelled_error())?;
            }
        }
    })
}

async fn wait_for_replay_pace(
    pace: Duration,
    cancellation: &CancellationToken,
) -> Result<(), TessivumError> {
    if cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    if !pace.is_zero() {
        tokio::select! {
            _ = cancellation.cancelled() => return Err(cancelled_error()),
            _ = tokio::time::sleep(pace) => {},
        }
    }
    if cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    Ok(())
}

fn resolve_replay_attempt(
    attempt: ReplayAttempt,
    messages: &[Message],
) -> Result<ReplayAttempt, TessivumError> {
    if !attempt.has_request_placeholder() {
        return Ok(attempt);
    }
    let corpus = request_string_corpus(messages)?;
    match attempt {
        ReplayAttempt::Chunks(chunks) => {
            Ok(ReplayAttempt::Chunks(resolve_chunks(chunks, &corpus)?))
        }
        ReplayAttempt::Throw { chunks, error } => Ok(ReplayAttempt::Throw {
            chunks: resolve_chunks(chunks, &corpus)?,
            error: TessivumError::new(
                substitute_string(&error.code, &corpus)?,
                substitute_string(&error.message, &corpus)?,
                error.phase,
                error.details,
            ),
        }),
        ReplayAttempt::Hang { chunks, ready_file } => Ok(ReplayAttempt::Hang {
            chunks: resolve_chunks(chunks, &corpus)?,
            ready_file,
        }),
    }
}

fn request_string_corpus(messages: &[Message]) -> Result<String, TessivumError> {
    let value = serde_json::to_value(messages).map_err(|error| {
        replay_error(
            "the replay request could not be encoded for placeholder matching",
            json!({"error": error.to_string()}),
        )
    })?;
    let mut strings = Vec::new();
    collect_strings(&value, &mut strings);
    Ok(strings.join("\n"))
}

fn collect_strings(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::String(value) => output.push(value.clone()),
        Value::Array(values) => {
            for value in values {
                collect_strings(value, output);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_strings(value, output);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn chunks_have_placeholder(chunks: &[StreamChunk]) -> bool {
    chunks.iter().any(|chunk| {
        serde_json::to_string(chunk)
            .map(|chunk| chunk.contains(FROM_REQUEST_OPEN))
            .unwrap_or(false)
    })
}

fn resolve_chunks(
    chunks: Arc<[StreamChunk]>,
    corpus: &str,
) -> Result<Arc<[StreamChunk]>, TessivumError> {
    chunks
        .iter()
        .map(|chunk| {
            let mut value = serde_json::to_value(chunk).map_err(|error| {
                replay_error(
                    "a replay chunk could not be encoded for placeholder matching",
                    json!({"error": error.to_string()}),
                )
            })?;
            substitute_value(&mut value, corpus)?;
            let chunk: StreamChunk = serde_json::from_value(value).map_err(|error| {
                replay_error(
                    "a replay chunk became invalid after placeholder matching",
                    json!({"error": error.to_string()}),
                )
            })?;
            chunk.validate().map_err(|error| {
                replay_error(
                    "a replay chunk violates the provider wire contract after placeholder matching",
                    json!({"error": error.message}),
                )
            })?;
            Ok(chunk)
        })
        .collect::<Result<Vec<_>, TessivumError>>()
        .map(Arc::from)
}

fn substitute_value(value: &mut Value, corpus: &str) -> Result<(), TessivumError> {
    match value {
        Value::String(value) => {
            if value.contains(FROM_REQUEST_OPEN) {
                *value = substitute_string(value, corpus)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                substitute_value(value, corpus)?;
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                substitute_value(value, corpus)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn substitute_string(text: &str, corpus: &str) -> Result<String, TessivumError> {
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(open_offset) = text[cursor..].find(FROM_REQUEST_OPEN) {
        let open = cursor + open_offset;
        result.push_str(&text[cursor..open]);
        let marker_end = open + FROM_REQUEST_OPEN.len();
        let mut close = text[marker_end..]
            .find(FROM_REQUEST_CLOSE)
            .map(|offset| marker_end + offset)
            .ok_or_else(|| {
                replay_error(
                    "a replay fromRequest placeholder is unterminated",
                    json!({"text": text}),
                )
            })?;
        while text.as_bytes().get(close + FROM_REQUEST_CLOSE.len()) == Some(&b'}') {
            close += 1;
        }
        let pattern = &text[marker_end..close];
        result.push_str(&resolve_from_request(pattern, corpus)?);
        cursor = close + FROM_REQUEST_CLOSE.len();
    }
    result.push_str(&text[cursor..]);
    Ok(result)
}

fn resolve_from_request(pattern: &str, corpus: &str) -> Result<String, TessivumError> {
    let regex = Regex::new(pattern).map_err(|error| {
        replay_error(
            "a replay fromRequest placeholder has an invalid regex",
            json!({"pattern": pattern, "error": error.to_string()}),
        )
    })?;
    let Some(captures) = regex.captures_iter(corpus).last() else {
        return Err(replay_error(
            "a replay fromRequest placeholder matched nothing in the request",
            json!({"pattern": pattern}),
        ));
    };
    Ok(captures
        .get(1)
        .or_else(|| captures.get(0))
        .expect("a regex capture iterator always returns the whole match")
        .as_str()
        .into())
}

fn replay_error(message: impl Into<String>, details: Value) -> TessivumError {
    let message = message.into();
    llm_error("INVALID_LLM_REPLAY", &message, details)
}

fn cancellable_stream(stream: LlmStream, cancellation: CancellationToken) -> LlmStream {
    Box::pin(stream::unfold(
        (stream, cancellation, false),
        |(mut stream, cancellation, terminal)| async move {
            if terminal {
                return None;
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Some((
                    Ok(cancelled_finish()),
                    (stream, cancellation, true),
                )),
                chunk = stream.next() => match chunk {
                    Some(Ok(chunk)) => {
                        let terminal = matches!(chunk, StreamChunk::Finish { .. });
                        Some((Ok(chunk), (stream, cancellation, terminal)))
                    }
                    Some(Err(error)) => Some((
                        Ok(terminal_failure_chunk(error, &cancellation)),
                        (stream, cancellation, true),
                    )),
                    None => Some((
                        Ok(terminal_failure_chunk(
                            llm_error(
                                "LLM_STREAM_ENDED_EARLY",
                                "the LLM stream ended before a finish chunk",
                                Value::Null,
                            ),
                            &cancellation,
                        )),
                        (stream, cancellation, true),
                    )),
                },
            }
        },
    ))
}

fn terminal_failure_stream(error: TessivumError, cancellation: CancellationToken) -> LlmStream {
    Box::pin(stream::once(async move {
        Ok(terminal_failure_chunk(error, &cancellation))
    }))
}

fn terminal_failure_chunk(error: TessivumError, cancellation: &CancellationToken) -> StreamChunk {
    let failure = LlmFailure {
        message: error.message,
        code: error.code,
        status: error
            .details
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok())
            .filter(|status| (100..=599).contains(status)),
        provider_retry_after_ms: error
            .details
            .get("providerRetryAfterMs")
            .and_then(Value::as_u64)
            .filter(|retry_after_ms| *retry_after_ms > 0),
        request_id: error
            .details
            .get("requestId")
            .and_then(Value::as_str)
            .filter(|request_id| !request_id.trim().is_empty())
            .map(Into::into),
    };
    StreamChunk::Finish {
        reason: if cancellation.is_cancelled() {
            FinishReason::Aborted { failure }
        } else {
            FinishReason::Error { failure }
        },
        replay_state: None,
    }
}

fn cancelled_finish() -> StreamChunk {
    StreamChunk::Finish {
        reason: FinishReason::Aborted {
            failure: LlmFailure {
                message: "the LLM generation was cancelled".into(),
                code: "LLM_CANCELLED".into(),
                status: None,
                provider_retry_after_ms: None,
                request_id: None,
            },
        },
        replay_state: None,
    }
}

fn validate_request(request: &GenerateRequest) -> Result<(), TessivumError> {
    request.validate()?;
    if request.provider.trim().is_empty() || request.model.trim().is_empty() {
        return Err(llm_error(
            "INVALID_LLM_ROUTE",
            "LLM requests require non-empty provider and model values",
            Value::Null,
        ));
    }
    Ok(())
}

fn cancelled_error() -> TessivumError {
    llm_error(
        "LLM_CANCELLED",
        "the LLM generation was cancelled",
        Value::Null,
    )
}

fn model_discovery_error(
    code: impl Into<String>,
    message: impl Into<String>,
    phase: impl Into<String>,
    details: Value,
) -> TessivumError {
    TessivumError::new(code, message, phase, details)
}

fn with_model_discovery_metadata(
    error: TessivumError,
    provider: &str,
    attempts: u64,
    retryable: bool,
) -> TessivumError {
    let mut details = match error.details {
        Value::Object(details) => details,
        _ => Map::new(),
    };
    details.insert("provider".into(), Value::String(provider.into()));
    details.insert("attempts".into(), Value::from(attempts));
    details.insert("retries".into(), Value::from(attempts.saturating_sub(1)));
    details.insert("retryable".into(), Value::Bool(retryable));
    TessivumError::new(
        error.code,
        error.message,
        error.phase,
        Value::Object(details),
    )
}

fn is_transient_model_discovery_error(error: &TessivumError) -> bool {
    if error
        .details
        .get("transient")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    if matches!(
        error.details.get("status").and_then(Value::as_u64),
        Some(408 | 425 | 429 | 500..=599)
    ) {
        return true;
    }
    let code = error.code.to_ascii_uppercase();
    code.contains("TRANSIENT")
        || code.contains("TEMPORARY")
        || code.contains("TIMEOUT")
        || matches!(
            code.as_str(),
            "RATE_LIMIT" | "SERVER" | "UNAVAILABLE" | "NETWORK_ERROR" | "CONNECTION_ERROR"
        )
}

fn stream_error(message: &str, index: u64) -> TessivumError {
    llm_error("INVALID_LLM_STREAM", message, json!({"index": index}))
}

fn llm_error(code: &str, message: &str, details: Value) -> TessivumError {
    TessivumError::new(code, message, "llm", details)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

use std::{
    collections::BTreeMap,
    fmt,
    pin::Pin,
    sync::{Arc, Mutex, Weak},
};

use async_trait::async_trait;
use futures_util::{stream, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};

use crate::{
    ContentBlock, FinishReason, GenerateRequest, Message, MessageId, MessageRole, MessageSource,
    SessionId, StreamChunk, TessivumError, TokenUsage, ToolCallId,
};

/// The stable capability key for provider-neutral streamed LLM generation.
pub fn llm_service_key() -> ServiceKey {
    ServiceKey::new("harness.llm", "1")
}

/// One fallible, ordered stream of raw provider chunks.
pub type LlmStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, TessivumError>> + Send>>;

/// A model provider that performs exactly one generation attempt per call.
#[async_trait]
pub trait LlmAdapter: Send + Sync {
    async fn generate(
        &self,
        request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError>;
}

struct ProviderSlot {
    id: u64,
    adapter: Arc<dyn LlmAdapter>,
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
        providers
            .providers
            .insert(provider.clone(), ProviderSlot { id, adapter });
        Ok(LlmProviderRegistration {
            providers: Arc::downgrade(&self.providers),
            provider,
            id,
        })
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

        let adapter = lock(&self.providers)
            .providers
            .get(&request.provider)
            .map(|slot| Arc::clone(&slot.adapter))
            .ok_or_else(|| {
                llm_error(
                    "LLM_PROVIDER_NOT_FOUND",
                    "no LLM adapter is registered for the requested provider",
                    json!({"provider": request.provider}),
                )
            })?;
        let stream = adapter.generate(request, cancellation.clone()).await?;
        Ok(cancellable_stream(stream, cancellation))
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
        arguments: String,
    },
    Direct,
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
                if block_type.trim().is_empty() {
                    return Err(stream_error("block starts require a block type", *index));
                }
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
                                arguments: String::new(),
                            }),
                            _ => BlockStatus::Open(OpenBlock::Direct),
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
                arguments_delta,
            } => {
                self.with_open(*index, "tool-call delta", |block| match block {
                    OpenBlock::ToolCall {
                        id: seen_id,
                        name: seen_name,
                        arguments,
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
                        arguments.push_str(arguments_delta);
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
                    OpenBlock::ToolCall { .. } | OpenBlock::Direct => BlockStatus::Omitted,
                };
            }
        } else if self
            .blocks
            .values()
            .any(|record| matches!(record.status, BlockStatus::Open(_)))
        {
            return Err(llm_error(
                "LLM_FINISH_WITH_OPEN_BLOCK",
                "only max-tokens may finish with an open LLM block",
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
        (OpenBlock::Text(text), ContentBlock::Text { .. }) => {
            Ok(ContentBlock::Text { text: text.clone() })
        }
        (OpenBlock::Reasoning(text), ContentBlock::Reasoning { .. }) => {
            Ok(ContentBlock::Reasoning { text: text.clone() })
        }
        (
            OpenBlock::ToolCall {
                id,
                name,
                arguments,
            },
            ContentBlock::ToolCall {
                id: terminal_id,
                name: terminal_name,
                ..
            },
        ) => Ok(ContentBlock::ToolCall {
            id: id.clone().unwrap_or_else(|| terminal_id.clone()),
            name: name.clone().unwrap_or_else(|| terminal_name.clone()),
            arguments: arguments.clone(),
        }),
        (OpenBlock::Direct, block) => Ok(block.clone()),
        _ => Err(stream_error(
            "block end does not match its started block type",
            index,
        )),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
struct ReplayRoute {
    #[serde(rename = "sessionId")]
    session_id: Option<SessionId>,
    provider: String,
    model: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayLine {
    #[serde(flatten)]
    route: ReplayRoute,
    #[serde(default)]
    chunk: Option<StreamChunk>,
    #[serde(default)]
    chunks: Vec<StreamChunk>,
}

/// An in-memory, timing-free adapter created from newline-delimited recorded chunks.
#[derive(Clone, Debug, Default)]
pub struct RecordedLlmAdapter {
    routes: Arc<BTreeMap<ReplayRoute, Vec<StreamChunk>>>,
}

impl RecordedLlmAdapter {
    /// Parses one JSON object per line. Each object names its session, provider, model, and chunk(s).
    pub fn from_jsonl(recording: &str) -> Result<Self, TessivumError> {
        let mut routes = BTreeMap::<ReplayRoute, Vec<StreamChunk>>::new();
        for (line_number, line) in recording.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let line: ReplayLine = serde_json::from_str(line).map_err(|error| {
                llm_error(
                    "INVALID_LLM_REPLAY",
                    "a recorded LLM replay line is not valid JSON",
                    json!({"line": line_number + 1, "error": error.to_string()}),
                )
            })?;
            let mut chunks = line.chunks;
            if let Some(chunk) = line.chunk {
                if !chunks.is_empty() {
                    return Err(llm_error(
                        "INVALID_LLM_REPLAY",
                        "a replay entry must contain either chunk or chunks, not both",
                        json!({"line": line_number + 1}),
                    ));
                }
                chunks.push(chunk);
            }
            if chunks.is_empty() {
                return Err(llm_error(
                    "INVALID_LLM_REPLAY",
                    "a replay entry must contain at least one chunk",
                    json!({"line": line_number + 1}),
                ));
            }
            for chunk in &chunks {
                chunk.validate()?;
            }
            routes.entry(line.route).or_default().append(&mut chunks);
        }
        Ok(Self {
            routes: Arc::new(routes),
        })
    }
}

#[async_trait]
impl LlmAdapter for RecordedLlmAdapter {
    async fn generate(
        &self,
        request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        if cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        let route = ReplayRoute {
            session_id: request.session_id,
            provider: request.provider,
            model: request.model,
        };
        let chunks = self.routes.get(&route).cloned().ok_or_else(|| {
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
        Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
    }
}

fn cancellable_stream(stream: LlmStream, cancellation: CancellationToken) -> LlmStream {
    Box::pin(stream::unfold(
        (stream, cancellation, false),
        |(mut stream, cancellation, sent_cancellation)| async move {
            if sent_cancellation {
                return None;
            }
            tokio::select! {
                _ = cancellation.cancelled() => Some((Err(cancelled_error()), (stream, cancellation, true))),
                chunk = stream.next() => chunk.map(|chunk| (chunk, (stream, cancellation, false))),
            }
        },
    ))
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

fn stream_error(message: &str, index: u64) -> TessivumError {
    llm_error("INVALID_LLM_STREAM", message, json!({"index": index}))
}

fn llm_error(code: &str, message: &str, details: Value) -> TessivumError {
    TessivumError::new(code, message, "llm", details)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

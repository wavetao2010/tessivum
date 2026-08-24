use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{header, Client, Response, Url};
use serde_json::{json, Map, Value};
use tessivum_core::CancellationToken;

use crate::{
    attachments::AttachmentStore,
    llm::{LlmAdapter, LlmStream},
    openai_responses::{
        image_input, OpenAiResponsesAdapter, ProviderSnapshot, ResponsesRouteResolver, SseDecoder,
        ToolNames,
    },
    ContentBlock, FinishReason, GenerateRequest, MessageRole, StreamChunk, TessivumError,
    TokenUsage, ToolCallId,
};

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

#[derive(Clone)]
struct SharedResolver(Arc<dyn ResponsesRouteResolver>);

impl ResponsesRouteResolver for SharedResolver {
    fn resolve(&self, provider: &str, model: &str) -> Result<ProviderSnapshot, TessivumError> {
        self.0.resolve(provider, model)
    }
}

/// Dispatches each configured route to its declared wire protocol.
#[derive(Clone)]
pub struct CompatibleApiAdapter {
    client: Client,
    resolver: Arc<dyn ResponsesRouteResolver>,
    responses: OpenAiResponsesAdapter,
    attachment_store: Arc<AttachmentStore>,
}

impl CompatibleApiAdapter {
    pub fn with_resolver_and_store<R>(resolver: R, store: Arc<AttachmentStore>) -> Self
    where
        R: ResponsesRouteResolver + 'static,
    {
        let resolver: Arc<dyn ResponsesRouteResolver> = Arc::new(resolver);
        Self {
            client: Client::new(),
            responses: OpenAiResponsesAdapter::with_resolver_and_store(
                SharedResolver(Arc::clone(&resolver)),
                Arc::clone(&store),
            ),
            resolver,
            attachment_store: store,
        }
    }

    fn snapshot(&self, request: &GenerateRequest) -> Result<ProviderSnapshot, TessivumError> {
        let snapshot = self.resolver.resolve(&request.provider, &request.model)?;
        snapshot.route.validate()?;
        snapshot.model.validate()?;
        snapshot.validate_request(&request.provider, &request.model)?;
        Ok(snapshot)
    }
}

#[async_trait]
impl LlmAdapter for CompatibleApiAdapter {
    async fn generate(
        &self,
        request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        let snapshot = self.snapshot(&request)?;
        match snapshot.route.api.as_str() {
            "openai-responses" => self.responses.generate(request, cancellation).await,
            "openai-completions" => self.generate_chat(request, snapshot, cancellation).await,
            "anthropic-messages" => {
                self.generate_anthropic(request, snapshot, cancellation)
                    .await
            }
            api => Err(protocol_error(
                "INVALID_LLM_PROTOCOL",
                "provider route uses an unsupported API protocol",
                json!({"api": api}),
            )),
        }
    }
}

impl CompatibleApiAdapter {
    async fn generate_chat(
        &self,
        request: GenerateRequest,
        snapshot: ProviderSnapshot,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        let tool_names = ToolNames::from_request(&request)?;
        let body = chat_body(
            &request,
            &snapshot,
            &tool_names,
            self.attachment_store.as_ref(),
        )
        .await?;
        let response = send(
            &self.client,
            &snapshot,
            "chat/completions",
            Auth::Bearer,
            body,
            cancellation.clone(),
        )
        .await?;
        let mut bytes = response.bytes_stream();
        Ok(Box::pin(async_stream::try_stream! {
            let mut decoder = SseDecoder::default();
            let mut state = ChatState::new(tool_names);
            loop {
                let next = tokio::select! {
                    _ = cancellation.cancelled() => Err(cancelled()),
                    next = bytes.next() => match next {
                        Some(Ok(bytes)) => Ok(Some(bytes)),
                        Some(Err(error)) => Err(protocol_error(
                            "TRANSPORT",
                            "OpenAI Completions stream read failed",
                            json!({"error": error.to_string()}),
                        )),
                        None => Ok(None),
                    },
                }?;
                let Some(next) = next else { break };
                for data in decoder.push(&next)? {
                    if data == "[DONE]" {
                        for chunk in state.finish()? {
                            yield chunk;
                        }
                        continue;
                    }
                    let event: Value = serde_json::from_str(&data).map_err(|error| protocol_error(
                        "OPENAI_PROTOCOL",
                        "OpenAI Completions stream contained invalid JSON",
                        json!({"error": error.to_string()}),
                    ))?;
                    for chunk in state.accept(&event)? {
                        yield chunk;
                    }
                }
            }
            decoder.finish()?;
            if !state.terminal {
                Err(protocol_error(
                    "TRANSPORT",
                    "OpenAI Completions stream ended before [DONE]",
                    Value::Null,
                ))?;
            }
        }))
    }

    async fn generate_anthropic(
        &self,
        request: GenerateRequest,
        snapshot: ProviderSnapshot,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        let body = anthropic_body(&request, &snapshot, self.attachment_store.as_ref()).await?;
        let response = send(
            &self.client,
            &snapshot,
            "messages",
            Auth::Anthropic,
            body,
            cancellation.clone(),
        )
        .await?;
        let mut bytes = response.bytes_stream();
        Ok(Box::pin(async_stream::try_stream! {
            let mut decoder = SseDecoder::default();
            let mut state = AnthropicState::default();
            loop {
                let next = tokio::select! {
                    _ = cancellation.cancelled() => Err(cancelled()),
                    next = bytes.next() => match next {
                        Some(Ok(bytes)) => Ok(Some(bytes)),
                        Some(Err(error)) => Err(protocol_error(
                            "TRANSPORT",
                            "Anthropic Messages stream read failed",
                            json!({"error": error.to_string()}),
                        )),
                        None => Ok(None),
                    },
                }?;
                let Some(next) = next else { break };
                for data in decoder.push(&next)? {
                    let event: Value = serde_json::from_str(&data).map_err(|error| protocol_error(
                        "ANTHROPIC_PROTOCOL",
                        "Anthropic Messages stream contained invalid JSON",
                        json!({"error": error.to_string()}),
                    ))?;
                    for chunk in state.accept(&event)? {
                        yield chunk;
                    }
                }
            }
            decoder.finish()?;
            if !state.terminal {
                Err(protocol_error(
                    "TRANSPORT",
                    "Anthropic Messages stream ended before message_stop",
                    Value::Null,
                ))?;
            }
        }))
    }
}

#[derive(Clone, Copy)]
enum Auth {
    Bearer,
    Anthropic,
}

async fn send(
    client: &Client,
    snapshot: &ProviderSnapshot,
    suffix: &str,
    auth: Auth,
    body: Value,
    cancellation: CancellationToken,
) -> Result<Response, TessivumError> {
    let endpoint = endpoint(snapshot, suffix)?;
    let mut pending = client
        .post(endpoint.clone())
        .header(header::ACCEPT, "text/event-stream")
        .json(&body);
    if let Some(key) = snapshot.api_key() {
        let header_value = match auth {
            Auth::Bearer => format!("Bearer {key}"),
            Auth::Anthropic => key.to_owned(),
        };
        let mut value = header::HeaderValue::from_str(&header_value).map_err(|_| {
            protocol_error(
                "INVALID_CREDENTIAL",
                "provider credential cannot be sent in an HTTP header",
                Value::Null,
            )
        })?;
        value.set_sensitive(true);
        pending = match auth {
            Auth::Bearer => pending.header(header::AUTHORIZATION, value),
            Auth::Anthropic => pending
                .header("x-api-key", value)
                .header("anthropic-version", "2023-06-01"),
        };
    } else if matches!(auth, Auth::Anthropic) {
        pending = pending.header("anthropic-version", "2023-06-01");
    }
    let response = tokio::select! {
        _ = cancellation.cancelled() => return Err(cancelled()),
        response = pending.send() => response.map_err(|error| protocol_error(
            "TRANSPORT",
            "provider request failed before a response was received",
            json!({"endpoint": endpoint.as_str(), "error": error.to_string()}),
        ))?,
    };
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(http_error(response, cancellation).await)
    }
}

fn endpoint(snapshot: &ProviderSnapshot, suffix: &str) -> Result<Url, TessivumError> {
    let base = snapshot.route.validate()?;
    Url::parse(&format!(
        "{}/{}",
        base.as_str().trim_end_matches('/'),
        suffix
    ))
    .map_err(|_| {
        protocol_error(
            "INVALID_LLM_BASE_URL",
            "provider base URL cannot address the selected API protocol",
            json!({"api": snapshot.route.api}),
        )
    })
}

async fn chat_body(
    request: &GenerateRequest,
    snapshot: &ProviderSnapshot,
    tool_names: &ToolNames,
    store: &AttachmentStore,
) -> Result<Value, TessivumError> {
    let mut messages = Vec::new();
    if let Some(system) = request.system.as_ref().filter(|value| !value.is_empty()) {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        };
        let mut content = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => content.push(json!({"type": "text", "text": text})),
                ContentBlock::Image { attachment } => {
                    let image = image_input(attachment, &snapshot.model, Some(store)).await?;
                    let url = image
                        .get("image_url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            protocol_error(
                                "INVALID_ATTACHMENT_REFERENCE",
                                "image conversion returned no data URL",
                                Value::Null,
                            )
                        })?;
                    content.push(json!({"type": "image_url", "image_url": {"url": url}}));
                }
                ContentBlock::Reasoning { .. } => {}
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": tool_names.wire(name), "arguments": arguments},
                })),
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } => {
                    let mut parts = Vec::new();
                    for block in content {
                        match block {
                            ContentBlock::Text { text } => {
                                parts.push(json!({"type": "text", "text": text}))
                            }
                            ContentBlock::Image { attachment } => {
                                let image =
                                    image_input(attachment, &snapshot.model, Some(store)).await?;
                                let url = image
                                    .get("image_url")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| {
                                        protocol_error(
                                            "INVALID_ATTACHMENT_REFERENCE",
                                            "image conversion returned no data URL",
                                            Value::Null,
                                        )
                                    })?;
                                parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                            }
                            _ => {}
                        }
                    }
                    if parts.is_empty() {
                        parts.push(json!({"type": "text", "text": "(no tool output)"}));
                    }
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call_id,
                        "content": parts,
                    }));
                }
            }
        }
        if !content.is_empty() || !tool_calls.is_empty() {
            let mut wire = Map::from_iter([
                ("role".into(), Value::String(role.into())),
                ("content".into(), Value::Array(content)),
            ]);
            if !tool_calls.is_empty() {
                wire.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            messages.push(Value::Object(wire));
        }
        messages.extend(tool_results);
    }
    let mut body = Map::from_iter([
        ("model".into(), Value::String(request.model.clone())),
        ("messages".into(), Value::Array(messages)),
        ("stream".into(), Value::Bool(true)),
        ("stream_options".into(), json!({"include_usage": true})),
    ]);
    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        body.insert(
            "tools".into(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool_names.wire(&tool.name),
                                "description": tool.description,
                                "parameters": tool.parameters,
                            }
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = request.max_tokens {
        body.insert("max_tokens".into(), json!(max_tokens));
    }
    if let Some(stop) = request.stop.as_ref().filter(|stop| !stop.is_empty()) {
        body.insert("stop".into(), json!(stop));
    }
    if let Some(effort) = request
        .reasoning_effort
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        body.insert("reasoning_effort".into(), Value::String(effort.clone()));
    }
    Ok(Value::Object(body))
}

#[derive(Default)]
struct ChatState {
    blocks: BTreeMap<u64, ChatBlock>,
    wire_to_logical: ToolNames,
    next_index: u64,
    finish_reason: Option<FinishReason>,
    terminal: bool,
}

impl ChatState {
    fn new(wire_to_logical: ToolNames) -> Self {
        Self {
            wire_to_logical,
            ..Self::default()
        }
    }

    fn accept(&mut self, event: &Value) -> Result<Vec<StreamChunk>, TessivumError> {
        if let Some(error) = event.get("error") {
            return Err(provider_event_error("OPENAI_PROTOCOL", error));
        }
        let mut chunks = Vec::new();
        if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
            chunks.push(StreamChunk::Usage {
                usage: openai_usage(usage),
            });
        }
        for choice in event
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            if let Some(text) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
            {
                chunks.extend(self.append_text(0, text, true)?);
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                chunks.extend(self.append_text(1, text, false)?);
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let wire_index = call.get("index").and_then(Value::as_u64).ok_or_else(|| {
                    protocol_error(
                        "OPENAI_PROTOCOL",
                        "tool-call delta has no index",
                        Value::Null,
                    )
                })?;
                chunks.extend(self.append_tool(wire_index + 2, call)?);
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(match reason {
                    "tool_calls" | "function_call" => FinishReason::ToolCalls,
                    "length" => FinishReason::MaxTokens,
                    _ => FinishReason::Stop,
                });
            }
        }
        Ok(chunks)
    }

    fn append_text(
        &mut self,
        key: u64,
        delta: &str,
        reasoning: bool,
    ) -> Result<Vec<StreamChunk>, TessivumError> {
        if delta.is_empty() {
            return Ok(Vec::new());
        }
        let mut chunks = Vec::new();
        if !self.blocks.contains_key(&key) {
            let index = self.next_index;
            self.next_index += 1;
            self.blocks.insert(
                key,
                if reasoning {
                    ChatBlock::Reasoning {
                        index,
                        text: String::new(),
                    }
                } else {
                    ChatBlock::Text {
                        index,
                        text: String::new(),
                    }
                },
            );
            chunks.push(StreamChunk::BlockStart {
                index,
                block_type: if reasoning { "reasoning" } else { "text" }.into(),
            });
        }
        let block = self.blocks.get_mut(&key).expect("inserted block");
        let (index, text) = match block {
            ChatBlock::Text { index, text } if !reasoning => (*index, text),
            ChatBlock::Reasoning { index, text } if reasoning => (*index, text),
            _ => {
                return Err(protocol_error(
                    "OPENAI_PROTOCOL",
                    "stream changed content block type",
                    json!({"index": key}),
                ))
            }
        };
        text.push_str(delta);
        chunks.push(if reasoning {
            StreamChunk::ReasoningDelta {
                index,
                text: delta.into(),
            }
        } else {
            StreamChunk::TextDelta {
                index,
                text: delta.into(),
            }
        });
        Ok(chunks)
    }

    fn append_tool(&mut self, key: u64, delta: &Value) -> Result<Vec<StreamChunk>, TessivumError> {
        let function = delta.get("function").unwrap_or(&Value::Null);
        let id = delta.get("id").and_then(Value::as_str);
        let name = function.get("name").and_then(Value::as_str);
        let arguments = function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut chunks = Vec::new();
        if !self.blocks.contains_key(&key) {
            let id = id.ok_or_else(|| {
                protocol_error(
                    "OPENAI_PROTOCOL",
                    "first tool-call delta has no id",
                    json!({"index": key - 2}),
                )
            })?;
            let name = name.ok_or_else(|| {
                protocol_error(
                    "OPENAI_PROTOCOL",
                    "first tool-call delta has no name",
                    json!({"index": key - 2}),
                )
            })?;
            let logical = self.wire_to_logical.logical(name).to_owned();
            let index = self.next_index;
            self.next_index += 1;
            self.blocks.insert(
                key,
                ChatBlock::Tool {
                    index,
                    id: ToolCallId::from(id.to_owned()),
                    name: logical,
                    arguments: String::new(),
                },
            );
            chunks.push(StreamChunk::BlockStart {
                index,
                block_type: "tool-call".into(),
            });
        }
        let ChatBlock::Tool {
            index,
            id,
            name,
            arguments: all,
        } = self.blocks.get_mut(&key).expect("inserted block")
        else {
            return Err(protocol_error(
                "OPENAI_PROTOCOL",
                "stream changed tool-call block type",
                json!({"index": key - 2}),
            ));
        };
        all.push_str(arguments);
        chunks.push(StreamChunk::ToolCallDelta {
            index: *index,
            id: id.clone(),
            name: Some(name.clone()),
            arguments_delta: arguments.into(),
        });
        Ok(chunks)
    }

    fn finish(&mut self) -> Result<Vec<StreamChunk>, TessivumError> {
        if self.terminal {
            return Ok(Vec::new());
        }
        let mut blocks = std::mem::take(&mut self.blocks)
            .into_values()
            .collect::<Vec<_>>();
        blocks.sort_by_key(ChatBlock::index);
        let mut chunks = blocks
            .into_iter()
            .map(ChatBlock::finish)
            .collect::<Vec<_>>();
        chunks.push(StreamChunk::Finish {
            reason: self.finish_reason.take().unwrap_or(FinishReason::Stop),
            replay_state: None,
        });
        self.terminal = true;
        Ok(chunks)
    }
}

enum ChatBlock {
    Text {
        index: u64,
        text: String,
    },
    Reasoning {
        index: u64,
        text: String,
    },
    Tool {
        index: u64,
        id: ToolCallId,
        name: String,
        arguments: String,
    },
}

impl ChatBlock {
    fn index(&self) -> u64 {
        match self {
            Self::Text { index, .. } | Self::Reasoning { index, .. } | Self::Tool { index, .. } => {
                *index
            }
        }
    }

    fn finish(self) -> StreamChunk {
        let index = self.index();
        let block = match self {
            Self::Text { text, .. } => ContentBlock::Text { text },
            Self::Reasoning { text, .. } => ContentBlock::Reasoning { text },
            Self::Tool {
                id,
                name,
                arguments,
                ..
            } => ContentBlock::ToolCall {
                id,
                name,
                arguments,
            },
        };
        StreamChunk::BlockEnd { index, block }
    }
}

fn openai_usage(value: &Value) -> TokenUsage {
    TokenUsage {
        input_tokens: value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: value
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        cache_write_tokens: None,
        reasoning_tokens: value
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
    }
}

async fn anthropic_body(
    request: &GenerateRequest,
    snapshot: &ProviderSnapshot,
    store: &AttachmentStore,
) -> Result<Value, TessivumError> {
    let mut system = request.system.clone().unwrap_or_default();
    let mut messages = Vec::new();
    for message in &request.messages {
        if message.role == MessageRole::System {
            for block in &message.content {
                if let ContentBlock::Text { text } = block {
                    if !system.is_empty() {
                        system.push_str("\n\n");
                    }
                    system.push_str(text);
                }
            }
            continue;
        }
        let mut normal = Vec::new();
        let mut tool_results = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => normal.push(json!({"type": "text", "text": text})),
                ContentBlock::Image { attachment } => {
                    normal.push(anthropic_image(attachment, snapshot, store).await?)
                }
                ContentBlock::Reasoning { .. } => {}
                ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    let input = serde_json::from_str::<Value>(arguments).map_err(|error| {
                        protocol_error(
                            "INVALID_TOOL_ARGUMENTS",
                            "Anthropic tool-call arguments are not valid JSON",
                            json!({"tool": name, "error": error.to_string()}),
                        )
                    })?;
                    normal
                        .push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
                }
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } => {
                    let mut output = Vec::new();
                    for block in content {
                        match block {
                            ContentBlock::Text { text } => {
                                output.push(json!({"type": "text", "text": text}))
                            }
                            ContentBlock::Image { attachment } => {
                                output.push(anthropic_image(attachment, snapshot, store).await?)
                            }
                            _ => {}
                        }
                    }
                    if output.is_empty() {
                        output.push(json!({"type": "text", "text": "(no tool output)"}));
                    }
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": output,
                        "is_error": is_error.unwrap_or(false),
                    }));
                }
            }
        }
        if !normal.is_empty() {
            messages.push(json!({
                "role": if message.role == MessageRole::Assistant { "assistant" } else { "user" },
                "content": normal,
            }));
        }
        if !tool_results.is_empty() {
            messages.push(json!({"role": "user", "content": tool_results}));
        }
    }
    let mut body = Map::from_iter([
        ("model".into(), Value::String(request.model.clone())),
        ("messages".into(), Value::Array(messages)),
        ("stream".into(), Value::Bool(true)),
        (
            "max_tokens".into(),
            json!(request
                .max_tokens
                .or(snapshot.model.max_tokens)
                .unwrap_or(8192)),
        ),
    ]);
    if !system.is_empty() {
        body.insert("system".into(), Value::String(system));
    }
    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        body.insert(
            "tools".into(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.parameters,
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(stop) = request.stop.as_ref().filter(|stop| !stop.is_empty()) {
        body.insert("stop_sequences".into(), json!(stop));
    }
    Ok(Value::Object(body))
}

async fn anthropic_image(
    attachment: &Value,
    snapshot: &ProviderSnapshot,
    store: &AttachmentStore,
) -> Result<Value, TessivumError> {
    let image = image_input(attachment, &snapshot.model, Some(store)).await?;
    let url = image
        .get("image_url")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            protocol_error(
                "INVALID_ATTACHMENT_REFERENCE",
                "image conversion returned no data URL",
                Value::Null,
            )
        })?;
    let (header, data) = url.split_once(',').ok_or_else(|| {
        protocol_error(
            "INVALID_ATTACHMENT_REFERENCE",
            "image conversion returned an invalid data URL",
            Value::Null,
        )
    })?;
    let media_type = header
        .strip_prefix("data:")
        .and_then(|value| value.strip_suffix(";base64"))
        .ok_or_else(|| {
            protocol_error(
                "INVALID_ATTACHMENT_REFERENCE",
                "image conversion returned an invalid media type",
                Value::Null,
            )
        })?;
    Ok(
        json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": data}}),
    )
}

#[derive(Default)]
struct AnthropicState {
    blocks: BTreeMap<u64, AnthropicBlock>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    finish_reason: Option<FinishReason>,
    terminal: bool,
}

impl AnthropicState {
    fn accept(&mut self, event: &Value) -> Result<Vec<StreamChunk>, TessivumError> {
        let kind = event.get("type").and_then(Value::as_str).ok_or_else(|| {
            protocol_error(
                "ANTHROPIC_PROTOCOL",
                "Anthropic event has no type",
                Value::Null,
            )
        })?;
        match kind {
            "ping" => Ok(Vec::new()),
            "error" => Err(provider_event_error(
                "ANTHROPIC_PROTOCOL",
                event.get("error").unwrap_or(event),
            )),
            "message_start" => {
                if let Some(usage) = event.pointer("/message/usage") {
                    self.record_usage(usage);
                }
                Ok(Vec::new())
            }
            "content_block_start" => self.start_block(event),
            "content_block_delta" => self.delta(event),
            "content_block_stop" => self.stop_block(required_index(event)?),
            "message_delta" => {
                if let Some(usage) = event.get("usage") {
                    self.record_usage(usage);
                }
                self.finish_reason = event
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .map(|reason| match reason {
                        "tool_use" => FinishReason::ToolCalls,
                        "max_tokens" => FinishReason::MaxTokens,
                        _ => FinishReason::Stop,
                    });
                Ok(Vec::new())
            }
            "message_stop" => self.finish(),
            _ => Ok(Vec::new()),
        }
    }

    fn start_block(&mut self, event: &Value) -> Result<Vec<StreamChunk>, TessivumError> {
        let index = required_index(event)?;
        if self.blocks.contains_key(&index) {
            return Ok(Vec::new());
        }
        let block = event.get("content_block").unwrap_or(&Value::Null);
        let (block_type, state) = match block.get("type").and_then(Value::as_str) {
            Some("text") => (
                "text",
                AnthropicBlock::Text(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                ),
            ),
            Some("thinking") => (
                "reasoning",
                AnthropicBlock::Reasoning(
                    block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                ),
            ),
            Some("tool_use") => {
                let id = block.get("id").and_then(Value::as_str).ok_or_else(|| {
                    protocol_error(
                        "ANTHROPIC_PROTOCOL",
                        "tool_use block has no id",
                        json!({"index": index}),
                    )
                })?;
                let name = block.get("name").and_then(Value::as_str).ok_or_else(|| {
                    protocol_error(
                        "ANTHROPIC_PROTOCOL",
                        "tool_use block has no name",
                        json!({"index": index}),
                    )
                })?;
                (
                    "tool-call",
                    AnthropicBlock::Tool {
                        id: ToolCallId::from(id.to_owned()),
                        name: name.into(),
                        arguments: String::new(),
                    },
                )
            }
            _ => return Ok(Vec::new()),
        };
        self.blocks.insert(index, state);
        let mut chunks = vec![StreamChunk::BlockStart {
            index,
            block_type: block_type.into(),
        }];
        match self.blocks.get(&index).expect("inserted block") {
            AnthropicBlock::Text(text) if !text.is_empty() => chunks.push(StreamChunk::TextDelta {
                index,
                text: text.clone(),
            }),
            AnthropicBlock::Reasoning(text) if !text.is_empty() => {
                chunks.push(StreamChunk::ReasoningDelta {
                    index,
                    text: text.clone(),
                })
            }
            AnthropicBlock::Tool { id, name, .. } => chunks.push(StreamChunk::ToolCallDelta {
                index,
                id: id.clone(),
                name: Some(name.clone()),
                arguments_delta: String::new(),
            }),
            _ => {}
        }
        Ok(chunks)
    }

    fn delta(&mut self, event: &Value) -> Result<Vec<StreamChunk>, TessivumError> {
        let index = required_index(event)?;
        let delta = event.get("delta").unwrap_or(&Value::Null);
        let state = self.blocks.get_mut(&index).ok_or_else(|| {
            protocol_error(
                "ANTHROPIC_PROTOCOL",
                "content delta has no open block",
                json!({"index": index}),
            )
        })?;
        match (delta.get("type").and_then(Value::as_str), state) {
            (Some("text_delta"), AnthropicBlock::Text(all)) => {
                let text = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                all.push_str(text);
                Ok(vec![StreamChunk::TextDelta {
                    index,
                    text: text.into(),
                }])
            }
            (Some("thinking_delta"), AnthropicBlock::Reasoning(all)) => {
                let text = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                all.push_str(text);
                Ok(vec![StreamChunk::ReasoningDelta {
                    index,
                    text: text.into(),
                }])
            }
            (Some("signature_delta"), AnthropicBlock::Reasoning(_)) => Ok(Vec::new()),
            (
                Some("input_json_delta"),
                AnthropicBlock::Tool {
                    id,
                    name,
                    arguments,
                },
            ) => {
                let part = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                arguments.push_str(part);
                Ok(vec![StreamChunk::ToolCallDelta {
                    index,
                    id: id.clone(),
                    name: Some(name.clone()),
                    arguments_delta: part.into(),
                }])
            }
            _ => Err(protocol_error(
                "ANTHROPIC_PROTOCOL",
                "content delta changed block type",
                json!({"index": index}),
            )),
        }
    }

    fn stop_block(&mut self, index: u64) -> Result<Vec<StreamChunk>, TessivumError> {
        let state = self.blocks.remove(&index).ok_or_else(|| {
            protocol_error(
                "ANTHROPIC_PROTOCOL",
                "content block stop has no open block",
                json!({"index": index}),
            )
        })?;
        let block = match state {
            AnthropicBlock::Text(text) => ContentBlock::Text { text },
            AnthropicBlock::Reasoning(text) => ContentBlock::Reasoning { text },
            AnthropicBlock::Tool {
                id,
                name,
                arguments,
            } => ContentBlock::ToolCall {
                id,
                name,
                arguments,
            },
        };
        Ok(vec![StreamChunk::BlockEnd { index, block }])
    }

    fn finish(&mut self) -> Result<Vec<StreamChunk>, TessivumError> {
        if self.terminal {
            return Ok(Vec::new());
        }
        if !self.blocks.is_empty() {
            return Err(protocol_error(
                "ANTHROPIC_PROTOCOL",
                "message stopped with open content blocks",
                Value::Null,
            ));
        }
        self.terminal = true;
        Ok(vec![
            StreamChunk::Usage {
                usage: TokenUsage {
                    input_tokens: self.input_tokens,
                    output_tokens: self.output_tokens,
                    cache_read_tokens: self.cache_read_tokens,
                    cache_write_tokens: self.cache_write_tokens,
                    reasoning_tokens: None,
                },
            },
            StreamChunk::Finish {
                reason: self.finish_reason.take().unwrap_or(FinishReason::Stop),
                replay_state: None,
            },
        ])
    }

    fn record_usage(&mut self, usage: &Value) {
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = value;
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = value;
        }
        self.cache_read_tokens = usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .or(self.cache_read_tokens);
        self.cache_write_tokens = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .or(self.cache_write_tokens);
    }
}

enum AnthropicBlock {
    Text(String),
    Reasoning(String),
    Tool {
        id: ToolCallId,
        name: String,
        arguments: String,
    },
}

fn required_index(event: &Value) -> Result<u64, TessivumError> {
    event.get("index").and_then(Value::as_u64).ok_or_else(|| {
        protocol_error(
            "ANTHROPIC_PROTOCOL",
            "Anthropic content event has no index",
            Value::Null,
        )
    })
}

async fn http_error(response: Response, cancellation: CancellationToken) -> TessivumError {
    let status = response.status();
    let request_id = response
        .headers()
        .get("request-id")
        .or_else(|| response.headers().get("x-request-id"))
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut bytes = response.bytes_stream();
    let mut body = Vec::new();
    while body.len() <= MAX_ERROR_BODY_BYTES {
        let next = tokio::select! {
            _ = cancellation.cancelled() => return cancelled(),
            next = bytes.next() => next,
        };
        let Some(next) = next else { break };
        match next {
            Ok(next) => body.extend_from_slice(
                &next[..next
                    .len()
                    .min(MAX_ERROR_BODY_BYTES.saturating_sub(body.len()))],
            ),
            Err(_) => break,
        }
    }
    let value = serde_json::from_slice::<Value>(&body)
        .unwrap_or_else(|_| json!({"body": String::from_utf8_lossy(&body)}));
    protocol_error(
        "PROVIDER_HTTP_ERROR",
        value
            .pointer("/error/message")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("provider rejected the request"),
        json!({"status": status.as_u16(), "requestId": request_id, "error": value}),
    )
}

fn provider_event_error(code: &str, error: &Value) -> TessivumError {
    protocol_error(
        code,
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider stream reported an error"),
        error.clone(),
    )
}

fn cancelled() -> TessivumError {
    protocol_error("CANCELLED", "LLM request was cancelled", Value::Null)
}

fn protocol_error(code: &str, message: &str, details: Value) -> TessivumError {
    TessivumError::new(code, message, "llm-compatible", details)
}

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{header, Client, Response, StatusCode, Url};
use serde_json::{json, Map, Value};
use tessivum_core::CancellationToken;

use crate::{
    llm::{LlmAdapter, LlmStream},
    ContentBlock, FinishReason, GenerateRequest, Message, MessageRole, MessageSource, StreamChunk,
    TessivumError, TokenUsage, ToolCallId,
};

const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const REPLAY_TYPE: &str = "openai-responses";
const REPLAY_VERSION: u64 = 1;

/// Native, stateless OpenAI Responses API adapter for OpenAI-compatible relays.
#[derive(Clone)]
pub struct OpenAiResponsesAdapter {
    client: Client,
    endpoint: Url,
    authorization: header::HeaderValue,
}

impl fmt::Debug for OpenAiResponsesAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesAdapter")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl OpenAiResponsesAdapter {
    /// Creates an adapter whose base URL is a prefix such as `https://relay.example/v1`.
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, TessivumError> {
        let base_url = base_url.trim();
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(adapter_error(
                "MISSING_CREDENTIAL",
                "OPENAI_API_KEY must not be empty",
                Value::Null,
            ));
        }
        let base = Url::parse(base_url).map_err(|error| {
            adapter_error(
                "INVALID_OPENAI_BASE_URL",
                "OPENAI_BASE_URL is not a valid URL",
                json!({"error": error.to_string()}),
            )
        })?;
        if !matches!(base.scheme(), "http" | "https")
            || base.cannot_be_a_base()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(adapter_error(
                "INVALID_OPENAI_BASE_URL",
                "OPENAI_BASE_URL must be an HTTP(S) URL without a query or fragment",
                Value::Null,
            ));
        }
        let endpoint = Url::parse(&format!(
            "{}/responses",
            base.as_str().trim_end_matches('/')
        ))
        .expect("a validated HTTP base plus a path is a URL");
        let mut authorization = header::HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| {
                adapter_error(
                    "INVALID_CREDENTIAL",
                    "OPENAI_API_KEY contains bytes that cannot be sent in an HTTP header",
                    Value::Null,
                )
            })?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .user_agent(concat!("tessivum/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                adapter_error(
                    "OPENAI_CLIENT_FAILED",
                    "could not construct the OpenAI HTTP client",
                    json!({"error": error.to_string()}),
                )
            })?;
        Ok(Self {
            client,
            endpoint,
            authorization,
        })
    }

    async fn send(
        &self,
        request: &GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<Response, TessivumError> {
        let body = request_body(request)?;
        let pending = self
            .client
            .post(self.endpoint.clone())
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::ACCEPT, "text/event-stream")
            .json(&body)
            .send();
        let response = tokio::select! {
            _ = cancellation.cancelled() => return Err(cancelled_error()),
            response = pending => response.map_err(|error| adapter_error(
                "OPENAI_TRANSPORT",
                "OpenAI Responses request failed before a response was received",
                json!({"endpoint": self.endpoint.as_str(), "error": error.to_string()}),
            ))?,
        };
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(http_error(response, cancellation).await)
        }
    }
}

#[async_trait]
impl LlmAdapter for OpenAiResponsesAdapter {
    async fn generate(
        &self,
        request: GenerateRequest,
        cancellation: CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        let response = self.send(&request, cancellation.clone()).await?;
        let mut bytes = response.bytes_stream();
        Ok(Box::pin(async_stream::try_stream! {
            let mut decoder = SseDecoder::default();
            let mut state = ResponseState::default();
            loop {
                let next = tokio::select! {
                    _ = cancellation.cancelled() => Err(cancelled_error()),
                    next = bytes.next() => match next {
                        Some(Ok(bytes)) => Ok(Some(bytes)),
                        Some(Err(error)) => Err(adapter_error(
                            "OPENAI_TRANSPORT",
                            "OpenAI Responses stream read failed",
                            json!({"error": error.to_string()}),
                        )),
                        None => Ok(None),
                    },
                }?;
                let Some(next) = next else { break };
                for data in decoder.push(&next)? {
                    if data == "[DONE]" { continue; }
                    let event: Value = serde_json::from_str(&data).map_err(|error| adapter_error(
                        "OPENAI_PROTOCOL",
                        "OpenAI Responses stream contained invalid JSON",
                        json!({"error": error.to_string()}),
                    ))?;
                    for chunk in state.accept(&event)? {
                        yield chunk;
                    }
                }
            }
            decoder.finish()?;
            if !state.terminal {
                Err(adapter_error(
                    "OPENAI_STREAM_ENDED_EARLY",
                    "OpenAI Responses stream ended before a terminal response event",
                    Value::Null,
                ))?;
            }
        }))
    }
}

fn request_body(request: &GenerateRequest) -> Result<Value, TessivumError> {
    if request.stop.as_ref().is_some_and(|stop| !stop.is_empty()) {
        return Err(adapter_error(
            "UNSUPPORTED_OPTION",
            "OpenAI Responses does not support stop sequences",
            json!({"field": "stop"}),
        ));
    }
    let mut body = Map::from_iter([
        ("model".into(), Value::String(request.model.clone())),
        ("input".into(), Value::Array(response_input(request)?)),
        ("stream".into(), Value::Bool(true)),
        ("store".into(), Value::Bool(false)),
        ("include".into(), json!(["reasoning.encrypted_content"])),
    ]);
    if let Some(system) = request.system.as_ref().filter(|value| !value.is_empty()) {
        body.insert("instructions".into(), Value::String(system.clone()));
    }
    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        body.insert(
            "tools".into(),
            Value::Array(
                tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                            "strict": false,
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
        body.insert("max_output_tokens".into(), json!(max_tokens.max(16)));
    }
    if let Some(effort) = request
        .reasoning_effort
        .as_ref()
        .filter(|effort| !effort.is_empty() && effort.as_str() != "off")
    {
        body.insert(
            "reasoning".into(),
            json!({"effort": effort, "summary": "auto"}),
        );
    }
    Ok(Value::Object(body))
}

fn response_input(request: &GenerateRequest) -> Result<Vec<Value>, TessivumError> {
    let mut input = Vec::new();
    for message in &request.messages {
        if let Some(output) = native_replay_output(message, request) {
            input.extend(output.iter().cloned());
            continue;
        }
        append_message_input(&mut input, message)?;
    }
    Ok(input)
}

fn native_replay_output<'a>(
    message: &'a Message,
    request: &GenerateRequest,
) -> Option<&'a Vec<Value>> {
    let MessageSource::Model {
        provider,
        model,
        replay_state: Some(replay),
    } = &message.source
    else {
        return None;
    };
    if provider != &request.provider || model != &request.model {
        return None;
    }
    let replay = replay.as_object()?;
    if replay.get("type")?.as_str()? != REPLAY_TYPE
        || replay.get("version")?.as_u64()? != REPLAY_VERSION
    {
        return None;
    }
    let output = replay.get("output")?.as_array()?;
    output.iter().all(valid_replay_item).then_some(output)
}

fn valid_replay_item(item: &Value) -> bool {
    let Some(item) = item.as_object() else {
        return false;
    };
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") => item.get("id").and_then(Value::as_str).is_some(),
        Some("message") => {
            item.get("role").and_then(Value::as_str) == Some("assistant")
                && item.get("content").and_then(Value::as_array).is_some()
        }
        Some("function_call") => ["call_id", "name", "arguments"]
            .iter()
            .all(|field| item.get(*field).and_then(Value::as_str).is_some()),
        _ => false,
    }
}

fn append_message_input(input: &mut Vec<Value>, message: &Message) -> Result<(), TessivumError> {
    let role = match message.role {
        MessageRole::System => "developer",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    };
    let mut content = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => content.push(json!({
                "type": if message.role == MessageRole::Assistant { "output_text" } else { "input_text" },
                "text": text,
            })),
            ContentBlock::Reasoning { .. } => {}
            ContentBlock::Image { .. } => {
                return Err(adapter_error(
                    "UNSUPPORTED_CONTENT",
                    "OpenAI Responses image input is not wired to the attachment store",
                    json!({"type": "image"}),
                ));
            }
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => {
                flush_message_content(input, role, &mut content);
                input.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
            ContentBlock::ToolResult {
                tool_call_id,
                content: output,
                ..
            } => {
                flush_message_content(input, role, &mut content);
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": tool_output_text(output),
                }));
            }
        }
    }
    flush_message_content(input, role, &mut content);
    Ok(())
}

fn flush_message_content(input: &mut Vec<Value>, role: &str, content: &mut Vec<Value>) {
    if !content.is_empty() {
        input.push(json!({"role": role, "content": std::mem::take(content)}));
    }
}

fn tool_output_text(blocks: &[ContentBlock]) -> String {
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        "(no tool output)".into()
    } else {
        text
    }
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, TessivumError> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some((at, delimiter)) = event_boundary(&self.buffer) {
            if at > MAX_SSE_EVENT_BYTES {
                return Err(adapter_error(
                    "OPENAI_SSE_EVENT_TOO_LARGE",
                    "OpenAI Responses SSE event exceeded the byte limit",
                    json!({"limit": MAX_SSE_EVENT_BYTES}),
                ));
            }
            let frame = self.buffer.drain(..at).collect::<Vec<_>>();
            self.buffer.drain(..delimiter);
            if let Some(data) = event_data(&frame)? {
                events.push(data);
            }
        }
        if self.buffer.len() > MAX_SSE_EVENT_BYTES {
            return Err(adapter_error(
                "OPENAI_SSE_EVENT_TOO_LARGE",
                "OpenAI Responses SSE event exceeded the byte limit",
                json!({"limit": MAX_SSE_EVENT_BYTES}),
            ));
        }
        Ok(events)
    }

    fn finish(&self) -> Result<(), TessivumError> {
        if self.buffer.iter().all(u8::is_ascii_whitespace) {
            Ok(())
        } else {
            Err(adapter_error(
                "OPENAI_SSE_TRUNCATED",
                "OpenAI Responses SSE stream ended inside an event",
                Value::Null,
            ))
        }
    }
}

fn event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    (0..buffer.len()).find_map(|index| {
        buffer
            .get(index..index + 2)
            .filter(|bytes| *bytes == b"\n\n")
            .map(|_| (index, 2))
            .or_else(|| {
                buffer
                    .get(index..index + 4)
                    .filter(|bytes| *bytes == b"\r\n\r\n")
                    .map(|_| (index, 4))
            })
    })
}

fn event_data(frame: &[u8]) -> Result<Option<String>, TessivumError> {
    let frame = std::str::from_utf8(frame).map_err(|error| {
        adapter_error(
            "OPENAI_PROTOCOL",
            "OpenAI Responses SSE event was not UTF-8",
            json!({"error": error.to_string()}),
        )
    })?;
    let data = frame
        .lines()
        .filter_map(|line| {
            line.strip_suffix('\r')
                .unwrap_or(line)
                .strip_prefix("data:")
        })
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<_>>();
    Ok((!data.is_empty()).then(|| data.join("\n")))
}

#[derive(Default)]
struct ResponseState {
    blocks: BTreeMap<u64, OpenBlock>,
    closed: BTreeSet<u64>,
    next_index: u64,
    tool_calls: bool,
    items: BTreeMap<u64, Value>,
    terminal: bool,
}

struct OpenBlock {
    index: u64,
    kind: OpenKind,
}

enum OpenKind {
    Text(String),
    Reasoning {
        text: String,
        separator_pending: bool,
    },
    ToolCall {
        id: ToolCallId,
        name: String,
        arguments: String,
    },
}

impl ResponseState {
    fn accept(&mut self, event: &Value) -> Result<Vec<StreamChunk>, TessivumError> {
        let kind = event.get("type").and_then(Value::as_str).ok_or_else(|| {
            adapter_error(
                "OPENAI_PROTOCOL",
                "OpenAI Responses event has no type",
                Value::Null,
            )
        })?;
        match kind {
            "response.created" | "response.queued" | "response.in_progress" => Ok(Vec::new()),
            "response.output_item.added" => self.open_item(
                output_index(event)?,
                event.get("item").unwrap_or(&Value::Null),
            ),
            "response.output_text.delta" | "response.refusal.delta" => self.append_text(
                output_index(event)?,
                event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                false,
            ),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => self
                .append_text(
                    output_index(event)?,
                    event
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    true,
                ),
            "response.reasoning_summary_part.done" => {
                self.finish_reasoning_part(output_index(event)?)
            }
            "response.function_call_arguments.delta" => self.append_arguments(
                output_index(event)?,
                event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            "response.function_call_arguments.done" => self.finish_arguments(
                output_index(event)?,
                event
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            "response.output_item.done" => self.close_item(
                output_index(event)?,
                event.get("item").unwrap_or(&Value::Null),
            ),
            "response.completed" | "response.incomplete" => {
                self.finish_response(event.get("response").unwrap_or(&Value::Null))
            }
            "response.failed" => Err(provider_event_error(event.get("response"))),
            "error" => Err(provider_event_error(Some(event))),
            _ => Ok(Vec::new()),
        }
    }

    fn open_item(
        &mut self,
        output_index: u64,
        item: &Value,
    ) -> Result<Vec<StreamChunk>, TessivumError> {
        if self.blocks.contains_key(&output_index) || self.closed.contains(&output_index) {
            return Ok(Vec::new());
        }
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let (block_type, kind) = match item_type {
            "message" => ("text", OpenKind::Text(String::new())),
            "reasoning" => (
                "reasoning",
                OpenKind::Reasoning {
                    text: String::new(),
                    separator_pending: false,
                },
            ),
            "function_call" => {
                let id = required_string(item, "call_id")?;
                let name = required_string(item, "name")?;
                self.tool_calls = true;
                (
                    "tool-call",
                    OpenKind::ToolCall {
                        id: ToolCallId::from(id),
                        name,
                        arguments: String::new(),
                    },
                )
            }
            _ => return Ok(Vec::new()),
        };
        let index = self.next_index;
        self.next_index = self.next_index.checked_add(1).ok_or_else(|| {
            adapter_error(
                "OPENAI_PROTOCOL",
                "too many OpenAI response blocks",
                Value::Null,
            )
        })?;
        self.blocks.insert(output_index, OpenBlock { index, kind });
        let mut chunks = vec![StreamChunk::BlockStart {
            index,
            block_type: block_type.into(),
        }];
        if item_type == "function_call" {
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            chunks.extend(self.append_arguments(output_index, arguments)?);
        }
        Ok(chunks)
    }

    fn append_text(
        &mut self,
        output_index: u64,
        delta: &str,
        reasoning: bool,
    ) -> Result<Vec<StreamChunk>, TessivumError> {
        let mut chunks = if self.blocks.contains_key(&output_index) {
            Vec::new()
        } else {
            self.open_item(
                output_index,
                &json!({"type": if reasoning { "reasoning" } else { "message" }}),
            )?
        };
        let block = self.blocks.get_mut(&output_index).ok_or_else(|| {
            adapter_error(
                "OPENAI_PROTOCOL",
                "text delta has no response block",
                json!({"outputIndex": output_index}),
            )
        })?;
        let emitted = match (&mut block.kind, reasoning) {
            (OpenKind::Text(text), false) => {
                text.push_str(delta);
                delta.to_owned()
            }
            (
                OpenKind::Reasoning {
                    text,
                    separator_pending,
                },
                true,
            ) => {
                let mut emitted = String::new();
                if *separator_pending && !text.is_empty() && !delta.is_empty() {
                    emitted.push_str("\n\n");
                    text.push_str("\n\n");
                    *separator_pending = false;
                }
                emitted.push_str(delta);
                text.push_str(delta);
                emitted
            }
            _ => {
                return Err(adapter_error(
                    "OPENAI_PROTOCOL",
                    "response delta changed block type",
                    json!({"outputIndex": output_index}),
                ))
            }
        };
        chunks.push(if reasoning {
            StreamChunk::ReasoningDelta {
                index: block.index,
                text: emitted,
            }
        } else {
            StreamChunk::TextDelta {
                index: block.index,
                text: emitted,
            }
        });
        Ok(chunks)
    }

    fn finish_reasoning_part(
        &mut self,
        output_index: u64,
    ) -> Result<Vec<StreamChunk>, TessivumError> {
        let block = self.blocks.get_mut(&output_index).ok_or_else(|| {
            adapter_error(
                "OPENAI_PROTOCOL",
                "reasoning part has no response block",
                json!({"outputIndex": output_index}),
            )
        })?;
        let OpenKind::Reasoning {
            separator_pending, ..
        } = &mut block.kind
        else {
            return Err(adapter_error(
                "OPENAI_PROTOCOL",
                "reasoning part changed block type",
                json!({"outputIndex": output_index}),
            ));
        };
        *separator_pending = true;
        Ok(Vec::new())
    }

    fn append_arguments(
        &mut self,
        output_index: u64,
        delta: &str,
    ) -> Result<Vec<StreamChunk>, TessivumError> {
        let block = self.blocks.get_mut(&output_index).ok_or_else(|| {
            adapter_error(
                "OPENAI_PROTOCOL",
                "function arguments have no tool-call block",
                json!({"outputIndex": output_index}),
            )
        })?;
        let OpenKind::ToolCall {
            id,
            name,
            arguments,
        } = &mut block.kind
        else {
            return Err(adapter_error(
                "OPENAI_PROTOCOL",
                "function arguments changed block type",
                json!({"outputIndex": output_index}),
            ));
        };
        arguments.push_str(delta);
        Ok(vec![StreamChunk::ToolCallDelta {
            index: block.index,
            id: id.clone(),
            name: Some(name.clone()),
            arguments_delta: delta.into(),
        }])
    }

    fn finish_arguments(
        &mut self,
        output_index: u64,
        final_arguments: &str,
    ) -> Result<Vec<StreamChunk>, TessivumError> {
        let block = self.blocks.get(&output_index).ok_or_else(|| {
            adapter_error(
                "OPENAI_PROTOCOL",
                "completed function arguments have no tool-call block",
                json!({"outputIndex": output_index}),
            )
        })?;
        let OpenKind::ToolCall { arguments, .. } = &block.kind else {
            return Err(adapter_error(
                "OPENAI_PROTOCOL",
                "completed function arguments changed block type",
                json!({"outputIndex": output_index}),
            ));
        };
        if final_arguments == arguments {
            Ok(Vec::new())
        } else if let Some(delta) = final_arguments.strip_prefix(arguments) {
            self.append_arguments(output_index, delta)
        } else {
            Err(adapter_error(
                "OPENAI_PROTOCOL",
                "completed function arguments do not extend streamed arguments",
                json!({"outputIndex": output_index}),
            ))
        }
    }

    fn close_item(
        &mut self,
        output_index: u64,
        item: &Value,
    ) -> Result<Vec<StreamChunk>, TessivumError> {
        if self.closed.contains(&output_index) {
            return Ok(Vec::new());
        }
        if valid_replay_item(item) {
            self.items.insert(output_index, item.clone());
        }
        let mut chunks = self.open_item(output_index, item)?;
        let Some(block) = self.blocks.get(&output_index) else {
            self.closed.insert(output_index);
            return Ok(chunks);
        };
        match &block.kind {
            OpenKind::Text(current) => {
                let final_text = message_text(item);
                chunks.extend(self.reconcile_text(
                    output_index,
                    current.clone(),
                    final_text,
                    false,
                )?);
            }
            OpenKind::Reasoning { text, .. } => {
                let final_text = reasoning_text(item);
                chunks.extend(self.reconcile_text(output_index, text.clone(), final_text, true)?);
            }
            OpenKind::ToolCall { arguments, .. } => {
                let final_arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or(arguments)
                    .to_owned();
                chunks.extend(self.finish_arguments(output_index, &final_arguments)?);
            }
        }
        let block = self
            .blocks
            .remove(&output_index)
            .expect("open block remains");
        let terminal = match block.kind {
            OpenKind::Text(text) => ContentBlock::Text { text },
            OpenKind::Reasoning { text, .. } => ContentBlock::Reasoning { text },
            OpenKind::ToolCall {
                id,
                name,
                arguments,
            } => ContentBlock::ToolCall {
                id,
                name,
                arguments,
            },
        };
        chunks.push(StreamChunk::BlockEnd {
            index: block.index,
            block: terminal,
        });
        self.closed.insert(output_index);
        Ok(chunks)
    }

    fn reconcile_text(
        &mut self,
        output_index: u64,
        current: String,
        final_text: String,
        reasoning: bool,
    ) -> Result<Vec<StreamChunk>, TessivumError> {
        if final_text == current {
            Ok(Vec::new())
        } else if let Some(delta) = final_text.strip_prefix(&current) {
            self.append_text(output_index, delta, reasoning)
        } else {
            Err(adapter_error(
                "OPENAI_PROTOCOL",
                "completed response text does not extend streamed text",
                json!({"outputIndex": output_index}),
            ))
        }
    }

    fn finish_response(&mut self, response: &Value) -> Result<Vec<StreamChunk>, TessivumError> {
        if self.terminal {
            return Err(adapter_error(
                "OPENAI_PROTOCOL",
                "OpenAI Responses stream has more than one terminal event",
                Value::Null,
            ));
        }
        let mut chunks = Vec::new();
        let mut output = response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .filter(|output| !output.is_empty())
            .unwrap_or_else(|| self.items.values().cloned().collect());
        for (output_index, item) in output.iter_mut().enumerate() {
            if item.get("type").and_then(Value::as_str) != Some("reasoning")
                || item.get("encrypted_content").is_some()
            {
                continue;
            }
            let encrypted = self
                .items
                .get(&(output_index as u64))
                .and_then(|item| item.get("encrypted_content"))
                .cloned();
            if let (Some(item), Some(encrypted)) = (item.as_object_mut(), encrypted) {
                item.insert("encrypted_content".into(), encrypted);
            }
        }
        for (output_index, item) in output.iter().enumerate() {
            chunks.extend(self.close_item(output_index as u64, item)?);
        }
        if !self.blocks.is_empty() {
            return Err(adapter_error(
                "OPENAI_PROTOCOL",
                "terminal OpenAI response left output blocks open",
                Value::Null,
            ));
        }
        if let Some(usage) = response.get("usage") {
            chunks.push(StreamChunk::Usage {
                usage: token_usage(usage),
            });
        }
        let status = response
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        let reason = if status == "incomplete" {
            FinishReason::MaxTokens
        } else if status != "completed" {
            return Err(provider_event_error(Some(response)));
        } else if self.tool_calls {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        };
        chunks.push(StreamChunk::Finish {
            reason,
            replay_state: Some(
                json!({"type": REPLAY_TYPE, "version": REPLAY_VERSION, "output": output}),
            ),
        });
        self.terminal = true;
        Ok(chunks)
    }
}

fn output_index(event: &Value) -> Result<u64, TessivumError> {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            adapter_error(
                "OPENAI_PROTOCOL",
                "OpenAI Responses event has no output_index",
                Value::Null,
            )
        })
}

fn required_string(item: &Value, field: &str) -> Result<String, TessivumError> {
    item.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            adapter_error(
                "OPENAI_PROTOCOL",
                format!("OpenAI response item has no {field}"),
                Value::Null,
            )
        })
}

fn message_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| {
            part.get("text")
                .or_else(|| part.get("refusal"))
                .and_then(Value::as_str)
        })
        .collect()
}

fn reasoning_text(item: &Value) -> String {
    for field in ["summary", "content"] {
        let text = item
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

fn token_usage(usage: &Value) -> TokenUsage {
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cache_write = usage
        .pointer("/input_tokens_details/cache_write_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let total_input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    TokenUsage {
        input_tokens: total_input
            .saturating_sub(cached)
            .saturating_sub(cache_write),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cache_read_tokens: (cached > 0).then_some(cached),
        cache_write_tokens: (cache_write > 0).then_some(cache_write),
        reasoning_tokens: usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .filter(|tokens| *tokens > 0),
    }
}

async fn http_error(response: Response, cancellation: CancellationToken) -> TessivumError {
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    while bytes.len() < MAX_ERROR_BODY_BYTES {
        let next = tokio::select! {
            _ = cancellation.cancelled() => return cancelled_error(),
            next = body.next() => next,
        };
        let Some(Ok(next)) = next else { break };
        let remaining = MAX_ERROR_BODY_BYTES - bytes.len();
        bytes.extend_from_slice(&next[..next.len().min(remaining)]);
    }
    let body = String::from_utf8_lossy(&bytes);
    let code = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "AUTH",
        StatusCode::TOO_MANY_REQUESTS => {
            if body.to_ascii_lowercase().contains("quota") {
                "QUOTA"
            } else {
                "RATE_LIMIT"
            }
        }
        StatusCode::BAD_REQUEST => {
            if body.to_ascii_lowercase().contains("context") {
                "CONTEXT_WINDOW_EXCEEDED"
            } else {
                "INVALID_REQUEST"
            }
        }
        status if status.is_server_error() => "SERVER",
        _ => "OPENAI_HTTP_ERROR",
    };
    adapter_error(
        code,
        format!(
            "OpenAI Responses endpoint returned HTTP {}",
            status.as_u16()
        ),
        json!({"status": status.as_u16(), "requestId": request_id, "body": body}),
    )
}

fn provider_event_error(event: Option<&Value>) -> TessivumError {
    let error = event
        .and_then(|event| event.get("error"))
        .unwrap_or(&Value::Null);
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("OPENAI_RESPONSE_FAILED");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| {
            event
                .and_then(|event| event.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or("OpenAI Responses request failed");
    adapter_error(code, message, Value::Null)
}

fn cancelled_error() -> TessivumError {
    adapter_error(
        "CANCELLED",
        "OpenAI Responses request was cancelled",
        Value::Null,
    )
}

fn adapter_error(
    code: impl Into<String>,
    message: impl Into<String>,
    details: Value,
) -> TessivumError {
    TessivumError::new(code, message, "llm-adapter", details)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_decoder_handles_split_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        assert!(decoder
            .push(b"event: response\r\ndata: {\"type\":")
            .unwrap()
            .is_empty());
        assert_eq!(
            decoder
                .push(b"\"response.created\"}\r\n\r\ndata: one\ndata: two\n\n")
                .unwrap(),
            vec!["{\"type\":\"response.created\"}", "one\ntwo"]
        );
        decoder.finish().unwrap();
    }
}

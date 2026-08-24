use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{header, Client, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tessivum_core::CancellationToken;

use crate::{
    attachments::{AttachmentError, AttachmentRef, AttachmentStore},
    llm::{LlmAdapter, LlmStream},
    mcp::decode_mcp_image,
    ContentBlock, FinishReason, GenerateRequest, Message, MessageRole, MessageSource, StreamChunk,
    TessivumError, TokenUsage, ToolCallId,
};

const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const REPLAY_TYPE: &str = "openai-responses";
const REPLAY_VERSION: u64 = 1;
const DEFAULT_ROUTE_ID: &str = "openai-responses";

/// Input capability declared by a Responses model.
pub const RESPONSES_TEXT_MODALITY: &str = "text";
pub const RESPONSES_IMAGE_MODALITY: &str = "image";

/// One selectable reasoning level and its provider wire spelling.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponsesReasoningEffort {
    pub id: String,
    pub wire: Option<String>,
}

/// The smallest model descriptor needed to validate a Responses request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponsesModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, alias = "inputModalities")]
    pub input: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_efforts: Vec<ResponsesReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
}

impl ResponsesModel {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            description: None,
            input: vec![RESPONSES_TEXT_MODALITY.into()],
            context_window: None,
            max_tokens: None,
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
        }
    }

    pub fn with_input<I, S>(mut self, input: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.input = input.into_iter().map(Into::into).collect();
        self
    }

    pub fn validate(&self) -> Result<(), TessivumError> {
        if self.id.trim().is_empty() {
            return Err(adapter_error(
                "INVALID_OPENAI_MODEL",
                "OpenAI Responses model ids must not be empty",
                Value::Null,
            ));
        }
        for modality in &self.input {
            if !matches!(
                modality.as_str(),
                RESPONSES_TEXT_MODALITY | RESPONSES_IMAGE_MODALITY
            ) {
                return Err(adapter_error(
                    "INVALID_OPENAI_MODALITY",
                    "OpenAI Responses models support only text and image input",
                    json!({"modality": modality}),
                ));
            }
        }
        if self.context_window == Some(0) || self.max_tokens == Some(0) {
            return Err(adapter_error(
                "INVALID_OPENAI_MODEL",
                "OpenAI Responses model limits must be positive when present",
                Value::Null,
            ));
        }
        if self
            .default_reasoning_effort
            .as_ref()
            .is_some_and(|effort| {
                !self
                    .reasoning_efforts
                    .iter()
                    .any(|candidate| candidate.id == *effort)
            })
        {
            return Err(adapter_error(
                "INVALID_OPENAI_MODEL",
                "defaultReasoningEffort must be declared by reasoningEfforts",
                Value::Null,
            ));
        }
        Ok(())
    }

    fn supports(&self, modality: &str) -> bool {
        let input = if self.input.is_empty() {
            &[RESPONSES_TEXT_MODALITY.to_owned()][..]
        } else {
            &self.input
        };
        input.iter().any(|value| value == modality)
    }
}

/// A configured provider route. Credentials are referenced, never embedded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponsesRoute {
    pub id: String,
    #[serde(default = "default_responses_api")]
    pub api: String,
    pub display_name: String,
    pub base_url: String,
    pub credential_ref: String,
    #[serde(default)]
    pub models: Vec<ResponsesModel>,
    #[serde(default)]
    pub generation: u64,
}

fn default_responses_api() -> String {
    "openai-responses".into()
}

impl ResponsesRoute {
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        base_url: impl Into<String>,
        credential_ref: impl Into<String>,
        models: Vec<ResponsesModel>,
    ) -> Self {
        Self {
            id: id.into(),
            api: default_responses_api(),
            display_name: display_name.into(),
            base_url: base_url.into(),
            credential_ref: credential_ref.into(),
            models,
            generation: 0,
        }
    }

    pub fn with_api(mut self, api: impl Into<String>) -> Self {
        self.api = api.into();
        self
    }

    pub fn validate(&self) -> Result<Url, TessivumError> {
        if !matches!(
            self.api.as_str(),
            "openai-completions" | "openai-responses" | "anthropic-messages"
        ) {
            return Err(adapter_error(
                "INVALID_LLM_PROTOCOL",
                "provider route uses an unsupported API protocol",
                json!({"api": self.api}),
            ));
        }
        if self.id.trim().is_empty() || self.display_name.trim().is_empty() {
            return Err(adapter_error(
                "INVALID_OPENAI_ROUTE",
                "OpenAI Responses route id and display name must not be empty",
                Value::Null,
            ));
        }
        let base = Url::parse(self.base_url.trim()).map_err(|error| {
            adapter_error(
                "INVALID_OPENAI_BASE_URL",
                "OPENAI_BASE_URL is not a valid URL",
                json!({"error": error.to_string()}),
            )
        })?;
        if !matches!(base.scheme(), "http" | "https")
            || base.cannot_be_a_base()
            || base.username() != ""
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(adapter_error(
                "INVALID_OPENAI_BASE_URL",
                "OPENAI_BASE_URL must be an HTTP(S) URL without userinfo, query, or fragment",
                Value::Null,
            ));
        }
        let mut ids = BTreeSet::new();
        for model in &self.models {
            model.validate()?;
            if !ids.insert(model.id.clone()) {
                return Err(adapter_error(
                    "INVALID_OPENAI_MODEL",
                    "OpenAI Responses routes must not contain duplicate model ids",
                    json!({"model": model.id}),
                ));
            }
        }
        Ok(base)
    }
}

/// Immutable route/model facts captured for one generation attempt.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSnapshot {
    pub route: ResponsesRoute,
    pub model: ResponsesModel,
    #[serde(skip)]
    api_key: Option<String>,
}

impl fmt::Debug for ProviderSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderSnapshot")
            .field("route", &self.route)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl ProviderSnapshot {
    pub fn new(
        route: ResponsesRoute,
        model: ResponsesModel,
        api_key: impl Into<String>,
    ) -> Result<Self, TessivumError> {
        Self::with_optional_key(route, model, Some(api_key.into()))
    }

    pub fn without_key(
        route: ResponsesRoute,
        model: ResponsesModel,
    ) -> Result<Self, TessivumError> {
        Self::with_optional_key(route, model, None::<String>)
    }

    fn with_optional_key(
        route: ResponsesRoute,
        model: ResponsesModel,
        api_key: Option<String>,
    ) -> Result<Self, TessivumError> {
        route.validate()?;
        model.validate()?;
        if !route.models.is_empty()
            && route
                .models
                .iter()
                .find(|candidate| candidate.id == model.id)
                != Some(&model)
        {
            return Err(adapter_error(
                "INVALID_OPENAI_MODEL",
                "the selected model is not the route's declared model descriptor",
                json!({"model": model.id}),
            ));
        }
        Ok(Self {
            route,
            model,
            api_key: api_key.map(|key| key.trim().to_owned()),
        })
    }

    fn endpoint(&self) -> Result<Url, TessivumError> {
        let base = self.route.validate()?;
        Url::parse(&format!(
            "{}/responses",
            base.as_str().trim_end_matches('/')
        ))
        .map_err(|_| {
            adapter_error(
                "INVALID_OPENAI_BASE_URL",
                "OPENAI_BASE_URL cannot address the Responses endpoint",
                Value::Null,
            )
        })
    }

    pub(crate) fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref().filter(|key| !key.is_empty())
    }

    pub(crate) fn validate_request(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<(), TessivumError> {
        if self.route.id != provider {
            return Err(adapter_error(
                "INVALID_OPENAI_ROUTE",
                "the resolved OpenAI Responses route does not match the request provider",
                json!({"provider": provider}),
            ));
        }
        if self.model.id != model {
            return Err(adapter_error(
                "INVALID_OPENAI_MODEL",
                "the resolved OpenAI Responses model does not match the request model",
                json!({"model": model}),
            ));
        }
        if !self.route.models.is_empty()
            && self
                .route
                .models
                .iter()
                .all(|candidate| candidate.id != model)
        {
            return Err(adapter_error(
                "INVALID_OPENAI_MODEL",
                "the requested model is not declared by the OpenAI Responses route",
                json!({"model": model}),
            ));
        }
        Ok(())
    }
}

/// Resolves a fresh immutable route/model snapshot for each generation.
pub trait ResponsesRouteResolver: Send + Sync {
    fn resolve(&self, provider: &str, model: &str) -> Result<ProviderSnapshot, TessivumError>;
}

impl<F> ResponsesRouteResolver for F
where
    F: Fn(&str, &str) -> Result<ProviderSnapshot, TessivumError> + Send + Sync,
{
    fn resolve(&self, provider: &str, model: &str) -> Result<ProviderSnapshot, TessivumError> {
        self(provider, model)
    }
}
struct StaticResponsesRouteResolver {
    route: ResponsesRoute,
    api_key: String,
}

impl ResponsesRouteResolver for StaticResponsesRouteResolver {
    fn resolve(&self, provider: &str, model: &str) -> Result<ProviderSnapshot, TessivumError> {
        if provider != self.route.id {
            return Err(adapter_error(
                "INVALID_OPENAI_ROUTE",
                "the request provider is not the configured OpenAI Responses route",
                json!({"provider": provider}),
            ));
        }
        ProviderSnapshot::new(
            self.route.clone(),
            ResponsesModel::new(model),
            self.api_key.clone(),
        )
    }
}

/// Native, stateless OpenAI Responses API adapter for OpenAI-compatible relays.
#[derive(Clone)]
pub struct OpenAiResponsesAdapter {
    client: Client,
    resolver: Arc<dyn ResponsesRouteResolver>,
    attachment_store: Option<Arc<AttachmentStore>>,
}

impl fmt::Debug for OpenAiResponsesAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiResponsesAdapter")
            .field("resolver", &"redacted")
            .field(
                "attachment_store",
                &self.attachment_store.as_ref().map(|_| "configured"),
            )
            .finish_non_exhaustive()
    }
}

impl OpenAiResponsesAdapter {
    /// Creates an adapter whose base URL is a prefix such as `https://relay.example/v1`.
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, TessivumError> {
        if api_key.trim().is_empty() {
            return Err(adapter_error(
                "MISSING_CREDENTIAL",
                "OPENAI_API_KEY must not be empty",
                Value::Null,
            ));
        }
        let route = ResponsesRoute::new(
            DEFAULT_ROUTE_ID,
            DEFAULT_ROUTE_ID,
            base_url.trim(),
            "OPENAI_API_KEY",
            Vec::new(),
        );
        route.validate()?;
        let api_key = api_key.trim().to_owned();
        Self::with_resolver(StaticResponsesRouteResolver { route, api_key }).build_client()
    }

    /// Builds an adapter that resolves a fresh route/model snapshot per request.
    pub fn with_resolver<R>(resolver: R) -> Self
    where
        R: ResponsesRouteResolver + 'static,
    {
        Self {
            client: Client::new(),
            resolver: Arc::new(resolver),
            attachment_store: None,
        }
    }

    /// Attaches the durable image store used to materialize `AttachmentRef`s.
    pub fn with_attachment_store(mut self, store: Arc<AttachmentStore>) -> Self {
        self.attachment_store = Some(store);
        self
    }

    /// Builds a resolver-backed adapter with durable image support.
    pub fn with_resolver_and_store<R>(resolver: R, store: Arc<AttachmentStore>) -> Self
    where
        R: ResponsesRouteResolver + 'static,
    {
        Self::with_resolver(resolver).with_attachment_store(store)
    }

    /// Alias spelling for callers that name the attachment dependency explicitly.
    pub fn with_resolver_and_attachment_store<R>(resolver: R, store: Arc<AttachmentStore>) -> Self
    where
        R: ResponsesRouteResolver + 'static,
    {
        Self::with_resolver_and_store(resolver, store)
    }

    /// Alias for callers that prefer constructor naming over builder naming.
    pub fn new_with_resolver<R>(resolver: R) -> Self
    where
        R: ResponsesRouteResolver + 'static,
    {
        Self::with_resolver(resolver)
    }

    fn build_client(self) -> Result<Self, TessivumError> {
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
        Ok(Self { client, ..self })
    }

    fn snapshot(&self, request: &GenerateRequest) -> Result<ProviderSnapshot, TessivumError> {
        let snapshot = self.resolver.resolve(&request.provider, &request.model)?;
        snapshot.route.validate()?;
        snapshot.model.validate()?;
        snapshot.validate_request(&request.provider, &request.model)?;
        validate_modalities(request, &snapshot.model)?;
        Ok(snapshot)
    }

    async fn send(
        &self,
        request: &GenerateRequest,
        snapshot: &ProviderSnapshot,
        tool_names: &ToolNames,
        cancellation: CancellationToken,
    ) -> Result<Response, TessivumError> {
        let api_key = snapshot.api_key().ok_or_else(|| {
            adapter_error(
                "MISSING_CREDENTIAL",
                "the OpenAI Responses route credential is not configured",
                json!({"route": snapshot.route.id}),
            )
        })?;
        let mut authorization = header::HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| {
                adapter_error(
                    "INVALID_CREDENTIAL",
                    "the OpenAI Responses route credential cannot be sent in an HTTP header",
                    Value::Null,
                )
            })?;
        authorization.set_sensitive(true);
        let endpoint = snapshot.endpoint()?;
        let body = request_body(
            request,
            snapshot,
            tool_names,
            self.attachment_store.as_deref(),
        )
        .await?;
        let pending = self
            .client
            .post(endpoint.clone())
            .header(header::AUTHORIZATION, authorization)
            .header(header::ACCEPT, "text/event-stream")
            .json(&body)
            .send();
        let response = tokio::select! {
            _ = cancellation.cancelled() => return Err(cancelled_error()),
            response = pending => response.map_err(|error| adapter_error(
                "TRANSPORT",
                "OpenAI Responses request failed before a response was received",
                json!({"endpoint": endpoint.as_str(), "error": error.to_string()}),
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
        let snapshot = self.snapshot(&request)?;
        let tool_names = ToolNames::from_request(&request)?;
        let response = self
            .send(&request, &snapshot, &tool_names, cancellation.clone())
            .await?;
        let mut bytes = response.bytes_stream();
        Ok(Box::pin(async_stream::try_stream! {
            let mut decoder = SseDecoder::default();
            let mut state = ResponseState { tool_names, ..ResponseState::default() };
            loop {
                let next = tokio::select! {
                    _ = cancellation.cancelled() => Err(cancelled_error()),
                    next = bytes.next() => match next {
                        Some(Ok(bytes)) => Ok(Some(bytes)),
                        Some(Err(error)) => Err(adapter_error(
                            "TRANSPORT",
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
                    "TRANSPORT",
                    "OpenAI Responses stream ended before a terminal response event",
                    Value::Null,
                ))?;
            }
        }))
    }
}
#[derive(Default)]
pub(crate) struct ToolNames {
    wire_by_logical: BTreeMap<String, String>,
    logical_by_wire: BTreeMap<String, String>,
}

impl ToolNames {
    pub(crate) fn from_request(request: &GenerateRequest) -> Result<Self, TessivumError> {
        let mut names = Self::default();
        if let Some(tools) = &request.tools {
            for tool in tools {
                names.insert(&tool.name)?;
            }
        }
        for message in &request.messages {
            if native_replay_output(message, request).is_some() {
                continue;
            }
            for block in &message.content {
                if let ContentBlock::ToolCall { name, .. } = block {
                    names.insert(name)?;
                }
            }
        }
        Ok(names)
    }

    fn insert(&mut self, logical: &str) -> Result<(), TessivumError> {
        if self.wire_by_logical.contains_key(logical) {
            return Ok(());
        }
        let wire: String = logical
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        if let Some(existing) = self.logical_by_wire.get(&wire) {
            return Err(adapter_error(
                "OPENAI_TOOL_NAME_COLLISION",
                "distinct tool names map to the same OpenAI Responses name",
                json!({"first": existing, "second": logical, "wire": wire}),
            ));
        }
        self.wire_by_logical.insert(logical.into(), wire.clone());
        self.logical_by_wire.insert(wire, logical.into());
        Ok(())
    }

    pub(crate) fn wire<'a>(&'a self, logical: &'a str) -> &'a str {
        self.wire_by_logical
            .get(logical)
            .map(String::as_str)
            .unwrap_or(logical)
    }

    pub(crate) fn logical<'a>(&'a self, wire: &'a str) -> &'a str {
        self.logical_by_wire
            .get(wire)
            .map(String::as_str)
            .unwrap_or(wire)
    }
}

async fn request_body(
    request: &GenerateRequest,
    snapshot: &ProviderSnapshot,
    tool_names: &ToolNames,
    attachment_store: Option<&AttachmentStore>,
) -> Result<Value, TessivumError> {
    if request.stop.as_ref().is_some_and(|stop| !stop.is_empty()) {
        return Err(adapter_error(
            "UNSUPPORTED_OPTION",
            "OpenAI Responses does not support stop sequences",
            json!({"field": "stop"}),
        ));
    }
    let mut body = Map::from_iter([
        ("model".into(), Value::String(request.model.clone())),
        (
            "input".into(),
            Value::Array(response_input(request, snapshot, tool_names, attachment_store).await?),
        ),
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
                            "name": tool_names.wire(&tool.name),
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
        .filter(|effort| !effort.is_empty())
    {
        let candidate = snapshot
            .model
            .reasoning_efforts
            .iter()
            .find(|candidate| candidate.id == *effort)
            .ok_or_else(|| {
                adapter_error(
                    "INVALID_REASONING_EFFORT",
                    "reasoning effort is not declared by the selected model",
                    json!({"provider": request.provider.as_str(), "model": request.model.as_str(), "reasoningEffort": effort.as_str()}),
                )
            })?;
        if let Some(wire) = candidate.wire.as_deref() {
            body.insert(
                "reasoning".into(),
                json!({"effort": wire, "summary": "auto"}),
            );
        }
    }
    Ok(Value::Object(body))
}

async fn response_input(
    request: &GenerateRequest,
    snapshot: &ProviderSnapshot,
    tool_names: &ToolNames,
    attachment_store: Option<&AttachmentStore>,
) -> Result<Vec<Value>, TessivumError> {
    let mut input = Vec::new();
    for message in &request.messages {
        if let Some(output) = native_replay_output(message, request) {
            input.extend(output.iter().cloned());
            continue;
        }
        append_message_input(
            &mut input,
            message,
            &snapshot.model,
            tool_names,
            attachment_store,
        )
        .await?;
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

fn validate_modalities(
    request: &GenerateRequest,
    model: &ResponsesModel,
) -> Result<(), TessivumError> {
    fn validate_block(block: &ContentBlock, model: &ResponsesModel) -> Result<(), TessivumError> {
        match block {
            ContentBlock::Image { .. } => {
                if !model.supports(RESPONSES_IMAGE_MODALITY) {
                    return Err(adapter_error(
                        "UNSUPPORTED_MODALITY",
                        "the selected OpenAI Responses model does not support this input modality",
                        json!({"modality": RESPONSES_IMAGE_MODALITY, "model": model.id}),
                    ));
                }
            }
            ContentBlock::Text { .. } => {
                if !model.supports(RESPONSES_TEXT_MODALITY) {
                    return Err(adapter_error(
                        "UNSUPPORTED_MODALITY",
                        "the selected OpenAI Responses model does not support this input modality",
                        json!({"modality": RESPONSES_TEXT_MODALITY, "model": model.id}),
                    ));
                }
            }
            ContentBlock::ToolResult { content, .. } => {
                for block in content {
                    validate_block(block, model)?;
                }
            }
            ContentBlock::Reasoning { .. } | ContentBlock::ToolCall { .. } => {}
        }
        Ok(())
    }
    for message in &request.messages {
        for block in &message.content {
            validate_block(block, model)?;
        }
    }
    Ok(())
}
async fn append_message_input(
    input: &mut Vec<Value>,
    message: &Message,
    model: &ResponsesModel,
    tool_names: &ToolNames,
    attachment_store: Option<&AttachmentStore>,
) -> Result<(), TessivumError> {
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
            ContentBlock::Image { attachment } => {
                content.push(image_input(attachment, model, attachment_store).await?);
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
                    "name": tool_names.wire(name),
                    "arguments": arguments,
                }));
            }
            ContentBlock::ToolResult {
                tool_call_id,
                content: output,
                ..
            } => {
                flush_message_content(input, role, &mut content);
                let output = tool_output_content(output, model, attachment_store).await?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_call_id,
                    "output": output,
                }));
            }
        }
    }
    flush_message_content(input, role, &mut content);
    Ok(())
}

pub(crate) async fn image_input(
    attachment: &Value,
    model: &ResponsesModel,
    attachment_store: Option<&AttachmentStore>,
) -> Result<Value, TessivumError> {
    if !model.supports(RESPONSES_IMAGE_MODALITY) {
        return Err(adapter_error(
            "UNSUPPORTED_MODALITY",
            "the selected OpenAI Responses model does not support image input",
            json!({"modality": RESPONSES_IMAGE_MODALITY, "model": model.id}),
        ));
    }
    let store = attachment_store.ok_or_else(|| {
        adapter_error(
            "INVALID_ATTACHMENT_REFERENCE",
            "durable image input requires an attachment store",
            Value::Null,
        )
    })?;
    let reference = match AttachmentRef::from_value(attachment) {
        Ok(reference) => reference,
        Err(_) => {
            let input = decode_mcp_image(attachment)
                .map_err(|error| attachment_error("image attachment is invalid", error))?;
            store
                .save(input)
                .await
                .map_err(|error| attachment_error("could not save image attachment", error))?
        }
    };
    let data = store
        .read_ref_bounded(&reference, store.limits().max_image_bytes)
        .await
        .map_err(|error| attachment_error("could not read image attachment", error))?;
    Ok(json!({
        "type": "input_image",
        "image_url": format!("{}{}", reference.data_url_prefix(), base64_encode(&data.data)),
        "detail": "auto",
    }))
}

async fn tool_output_content(
    blocks: &[ContentBlock],
    model: &ResponsesModel,
    attachment_store: Option<&AttachmentStore>,
) -> Result<Value, TessivumError> {
    if !blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }))
    {
        return Ok(Value::String(tool_output_text(blocks)));
    }
    let mut content = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                content.push(json!({"type": "input_text", "text": text}));
            }
            ContentBlock::Image { attachment } => {
                content.push(image_input(attachment, model, attachment_store).await?);
            }
            _ => {}
        }
    }
    Ok(Value::Array(content))
}

fn attachment_error(message: &str, error: AttachmentError) -> TessivumError {
    adapter_error(error.code(), message, json!({"error": error.to_string()}))
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        output.push(TABLE[(first >> 2) as usize] as char);
        let second = chunk.get(1).copied();
        output.push(TABLE[((first & 0x03) << 4 | second.unwrap_or(0) >> 4) as usize] as char);
        if let Some(second) = second {
            output.push(
                TABLE[((second & 0x0f) << 2 | chunk.get(2).copied().unwrap_or(0) >> 6) as usize]
                    as char,
            );
        } else {
            output.push('=');
        }
        if let Some(third) = chunk.get(2).copied() {
            output.push(TABLE[(third & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
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
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, TessivumError> {
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

    pub(crate) fn finish(&self) -> Result<(), TessivumError> {
        if self.buffer.iter().all(u8::is_ascii_whitespace) {
            Ok(())
        } else {
            Err(adapter_error(
                "TRANSPORT",
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
    tool_names: ToolNames,
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
                let wire_name = required_string(item, "name")?;
                let name = self.tool_names.logical(&wire_name).to_owned();
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

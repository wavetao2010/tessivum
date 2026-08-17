use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};
use url::Url;

use crate::TessivumError;

/// Stable capability key for provider-neutral web search and fetch.
pub fn web_service_key() -> ServiceKey {
    ServiceKey::new("harness.web", "1")
}

/// Explicit provider routes and the maximum number of sources exposed to a model call.
#[derive(Clone, Debug)]
pub struct WebRuntimeConfig {
    pub search_provider: Option<String>,
    pub fetch_provider: Option<String>,
    pub max_search_results: usize,
}

impl Default for WebRuntimeConfig {
    fn default() -> Self {
        Self {
            search_provider: None,
            fetch_provider: None,
            max_search_results: 8,
        }
    }
}

impl WebRuntimeConfig {
    /// Reads only provider identifiers; credentials remain owned by concrete provider seams.
    pub fn from_env() -> Self {
        Self {
            search_provider: env_provider("DSH_WEB_SEARCH_PROVIDER"),
            fetch_provider: env_provider("DSH_WEB_FETCH_PROVIDER"),
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), TessivumError> {
        if self.max_search_results == 0 {
            return Err(web_error(
                "INVALID_WEB_CONFIG",
                "max search results must be positive",
                Value::Null,
            ));
        }
        Ok(())
    }
}

/// A normalized search request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchRequest {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<usize>,
}

/// One model-visible search source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchSource {
    pub title: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// A bounded provider-neutral search response.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResult {
    #[serde(default)]
    pub sources: Vec<WebSearchSource>,
    #[serde(default)]
    pub truncated: bool,
}

/// A web fetch request. Runtime and HTTP backend both enforce HTTP(S) and no credentials.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchRequest {
    pub url: String,
}

/// Closed body union returned by web fetch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum WebBody {
    Html { html: String },
    Text { text: String },
}

/// A completed HTTP result. Non-2xx status codes are data, not provider failures.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchResult {
    pub final_url: String,
    pub status_code: u16,
    pub body: WebBody,
    pub truncated: bool,
}

/// Local capability probe plus a cancellable search implementation.
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    fn available(&self) -> Result<bool, TessivumError>;
    async fn search(
        &self,
        request: WebSearchRequest,
        cancellation: CancellationToken,
    ) -> Result<WebSearchResult, TessivumError>;
}

/// Local capability probe plus a cancellable web fetch implementation.
#[async_trait]
pub trait WebFetchProvider: Send + Sync {
    fn available(&self) -> Result<bool, TessivumError>;
    async fn fetch(
        &self,
        request: WebFetchRequest,
        cancellation: CancellationToken,
    ) -> Result<WebFetchResult, TessivumError>;
}

struct ProviderSlot<T: ?Sized> {
    id: u64,
    provider: Arc<T>,
}

#[derive(Default)]
struct WebRegistry {
    next_id: u64,
    search: BTreeMap<String, ProviderSlot<dyn WebSearchProvider>>,
    fetch: BTreeMap<String, ProviderSlot<dyn WebFetchProvider>>,
}

/// In-process registry of native web providers. Provider selection happens at execution time.
#[derive(Clone)]
pub struct WebRuntime {
    config: WebRuntimeConfig,
    registry: Arc<Mutex<WebRegistry>>,
}

impl fmt::Debug for WebRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let registry = lock(&self.registry);
        formatter
            .debug_struct("WebRuntime")
            .field(
                "search_providers",
                &registry.search.keys().collect::<Vec<_>>(),
            )
            .field(
                "fetch_providers",
                &registry.fetch.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Default for WebRuntime {
    fn default() -> Self {
        Self::new(WebRuntimeConfig::default()).expect("default web config is valid")
    }
}

impl WebRuntime {
    pub fn new(config: WebRuntimeConfig) -> Result<Self, TessivumError> {
        config.validate()?;
        Ok(Self {
            config,
            registry: Arc::new(Mutex::new(WebRegistry::default())),
        })
    }

    pub fn from_env() -> Result<Self, TessivumError> {
        Self::new(WebRuntimeConfig::from_env())
    }

    /// Publishes this runtime as a scope-owned `harness.web@1` service.
    pub fn publish(self, context: &ContextHandle) -> Result<ServiceHandle<Self>, CoreError> {
        context.provide(web_service_key(), self)
    }

    /// Registers one synchronous-disposal search provider under an exact identifier.
    pub fn register_search(
        &self,
        id: impl Into<String>,
        provider: Arc<dyn WebSearchProvider>,
    ) -> Result<WebSearchProviderRegistration, TessivumError> {
        let id = validate_provider_id(id.into())?;
        let mut registry = lock(&self.registry);
        if registry.search.contains_key(&id) {
            return Err(duplicate_provider(&id));
        }
        let registration = next_registration_id(&mut registry);
        registry.search.insert(
            id.clone(),
            ProviderSlot {
                id: registration,
                provider,
            },
        );
        Ok(WebSearchProviderRegistration {
            registry: Arc::downgrade(&self.registry),
            id,
            registration,
        })
    }

    /// Registers one synchronous-disposal fetch provider under an exact identifier.
    pub fn register_fetch(
        &self,
        id: impl Into<String>,
        provider: Arc<dyn WebFetchProvider>,
    ) -> Result<WebFetchProviderRegistration, TessivumError> {
        let id = validate_provider_id(id.into())?;
        let mut registry = lock(&self.registry);
        if registry.fetch.contains_key(&id) {
            return Err(duplicate_provider(&id));
        }
        let registration = next_registration_id(&mut registry);
        registry.fetch.insert(
            id.clone(),
            ProviderSlot {
                id: registration,
                provider,
            },
        );
        Ok(WebFetchProviderRegistration {
            registry: Arc::downgrade(&self.registry),
            id,
            registration,
        })
    }

    /// Selects then calls a search provider, enforcing the request cap after provider output.
    pub async fn search(
        &self,
        request: WebSearchRequest,
        cancellation: CancellationToken,
    ) -> Result<WebSearchResult, TessivumError> {
        if request.query.trim().is_empty() {
            return Err(web_error(
                "INVALID_WEB_SEARCH_REQUEST",
                "web search query must not be empty",
                Value::Null,
            ));
        }
        if cancellation.is_cancelled() {
            return Err(aborted_error());
        }
        let provider = self.select_search()?;
        let limit = request
            .max_results
            .unwrap_or(self.config.max_search_results)
            .min(self.config.max_search_results);
        let mut result = tokio::select! {
            _ = cancellation.cancelled() => return Err(aborted_error()),
            result = provider.search(request, cancellation.clone()) => result?,
        };
        if result.sources.len() > limit {
            result.sources.truncate(limit);
            result.truncated = true;
        }
        Ok(result)
    }

    /// Selects then calls a fetch provider. HTTP safety checks precede provider selection.
    pub async fn fetch(
        &self,
        request: WebFetchRequest,
        cancellation: CancellationToken,
    ) -> Result<WebFetchResult, TessivumError> {
        validate_fetch_url(&request.url)?;
        if cancellation.is_cancelled() {
            return Err(aborted_error());
        }
        let provider = self.select_fetch()?;
        let cancelled = cancellation.clone();
        tokio::select! {
            _ = cancelled.cancelled() => Err(aborted_error()),
            result = provider.fetch(request, cancellation) => result,
        }
    }

    fn select_search(&self) -> Result<Arc<dyn WebSearchProvider>, TessivumError> {
        let configured = self.config.search_provider.as_deref();
        select_provider(
            &lock(&self.registry).search,
            configured,
            "search",
            |slot| slot.provider.available(),
            |slot| Arc::clone(&slot.provider),
        )
    }

    fn select_fetch(&self) -> Result<Arc<dyn WebFetchProvider>, TessivumError> {
        let configured = self.config.fetch_provider.as_deref();
        select_provider(
            &lock(&self.registry).fetch,
            configured,
            "fetch",
            |slot| slot.provider.available(),
            |slot| Arc::clone(&slot.provider),
        )
    }
}

/// Drop-owned search registration. Disposing is synchronous and never waits for a provider call.
pub struct WebSearchProviderRegistration {
    registry: Weak<Mutex<WebRegistry>>,
    id: String,
    registration: u64,
}

impl fmt::Debug for WebSearchProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSearchProviderRegistration")
            .field("id", &self.id)
            .finish()
    }
}

impl WebSearchProviderRegistration {
    pub fn unregister(&mut self) -> bool {
        let Some(registry) = self.registry.upgrade() else {
            return false;
        };
        let mut registry = lock(&registry);
        if registry
            .search
            .get(&self.id)
            .is_none_or(|slot| slot.id != self.registration)
        {
            return false;
        }
        registry.search.remove(&self.id);
        true
    }
}

impl Drop for WebSearchProviderRegistration {
    fn drop(&mut self) {
        self.unregister();
    }
}

/// Drop-owned fetch registration. Disposing is synchronous and never waits for a provider call.
pub struct WebFetchProviderRegistration {
    registry: Weak<Mutex<WebRegistry>>,
    id: String,
    registration: u64,
}

impl fmt::Debug for WebFetchProviderRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebFetchProviderRegistration")
            .field("id", &self.id)
            .finish()
    }
}

impl WebFetchProviderRegistration {
    pub fn unregister(&mut self) -> bool {
        let Some(registry) = self.registry.upgrade() else {
            return false;
        };
        let mut registry = lock(&registry);
        if registry
            .fetch
            .get(&self.id)
            .is_none_or(|slot| slot.id != self.registration)
        {
            return false;
        }
        registry.fetch.remove(&self.id);
        true
    }
}

impl Drop for WebFetchProviderRegistration {
    fn drop(&mut self) {
        self.unregister();
    }
}

/// Bounds for the built-in HTTP(S) fetch seam.
#[derive(Clone, Debug)]
pub struct HttpFetchConfig {
    pub timeout: Duration,
    pub max_redirects: usize,
    pub max_body_bytes: usize,
    pub max_body_chars: usize,
}

impl Default for HttpFetchConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(15),
            max_redirects: 5,
            max_body_bytes: 1_048_576,
            max_body_chars: 262_144,
        }
    }
}

impl HttpFetchConfig {
    fn validate(&self) -> Result<(), TessivumError> {
        if self.timeout.is_zero() || self.max_body_bytes == 0 || self.max_body_chars == 0 {
            return Err(web_error(
                "INVALID_HTTP_FETCH_CONFIG",
                "HTTP fetch timeout and body limits must be positive",
                Value::Null,
            ));
        }
        Ok(())
    }
}

/// Hardened HTTP(S) fetch provider.
///
/// This deliberately does not resolve or block private-network addresses. Deployments must not
/// expose `web_fetch` to sensitive internal networks without a network-level egress policy.
#[derive(Clone)]
pub struct HttpFetchProvider {
    client: reqwest::Client,
    config: HttpFetchConfig,
}

impl HttpFetchProvider {
    pub fn new(config: HttpFetchConfig) -> Result<Self, TessivumError> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                web_error(
                    "WEB_PROVIDER_ERROR",
                    "could not construct HTTP client",
                    json!({"error": error.to_string()}),
                )
            })?;
        Ok(Self { client, config })
    }
}

#[async_trait]
impl WebFetchProvider for HttpFetchProvider {
    fn available(&self) -> Result<bool, TessivumError> {
        Ok(true)
    }

    async fn fetch(
        &self,
        request: WebFetchRequest,
        cancellation: CancellationToken,
    ) -> Result<WebFetchResult, TessivumError> {
        let initial = validate_fetch_url(&request.url)?;
        let mut current = initial.clone();
        for redirects in 0..=self.config.max_redirects {
            let response = tokio::select! {
                _ = cancellation.cancelled() => return Err(aborted_error()),
                response = tokio::time::timeout(self.config.timeout, self.client.get(current.clone()).send()) => match response {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => return Err(web_error("WEB_PROVIDER_ERROR", "HTTP request failed", json!({"error": error.to_string()}))),
                    Err(_) => return Err(web_error("WEB_FETCH_TIMEOUT", "HTTP request timed out", json!({"url": current.as_str()}))),
                },
            };
            if response.status().is_redirection() {
                if redirects == self.config.max_redirects {
                    return Err(web_error(
                        "WEB_FETCH_REDIRECT_LIMIT",
                        "HTTP redirect limit exceeded",
                        json!({"url": current.as_str()}),
                    ));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        web_error(
                            "WEB_FETCH_REDIRECT_INVALID",
                            "redirect response has no valid Location",
                            Value::Null,
                        )
                    })?;
                let next = current.join(location).map_err(|_| {
                    web_error(
                        "WEB_FETCH_REDIRECT_INVALID",
                        "redirect Location is not a valid URL",
                        Value::Null,
                    )
                })?;
                validate_url(&next)?;
                if !same_origin(&initial, &next) {
                    return Err(web_error(
                        "WEB_FETCH_CROSS_ORIGIN_REDIRECT",
                        "cross-origin redirects are not allowed",
                        json!({"from": current.as_str(), "to": next.as_str()}),
                    ));
                }
                current = next;
                continue;
            }
            let status_code = response.status().as_u16();
            let is_html = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/html"));
            let (text, truncated) = read_body(response, &self.config, cancellation).await?;
            return Ok(WebFetchResult {
                final_url: current.into(),
                status_code,
                body: if is_html {
                    WebBody::Html { html: text }
                } else {
                    WebBody::Text { text }
                },
                truncated,
            });
        }
        unreachable!("redirect loop returns or fetches")
    }
}

fn select_provider<T: ?Sized, R>(
    providers: &BTreeMap<String, ProviderSlot<T>>,
    configured: Option<&str>,
    kind: &str,
    available: impl Fn(&ProviderSlot<T>) -> Result<bool, TessivumError>,
    route: impl Fn(&ProviderSlot<T>) -> R,
) -> Result<R, TessivumError> {
    if let Some(configured) = configured {
        let Some(slot) = providers.get(configured) else {
            return Err(web_error(
                "WEB_PROVIDER_CONFIGURED_MISSING",
                "configured web provider is not registered",
                json!({"kind": kind, "provider": configured}),
            ));
        };
        return if available(slot)? {
            Ok(route(slot))
        } else {
            Err(web_error(
                "WEB_PROVIDER_CONFIGURED_UNAVAILABLE",
                "configured web provider is unavailable",
                json!({"kind": kind, "provider": configured}),
            ))
        };
    }
    let usable: Vec<_> = providers
        .iter()
        .filter_map(|(id, slot)| match available(slot) {
            Ok(true) => Some(Ok((id, slot))),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<_, _>>()?;
    match usable.as_slice() {
        [] => Err(web_error(
            "WEB_PROVIDER_UNAVAILABLE",
            "no usable web provider is registered",
            json!({"kind": kind}),
        )),
        [(_, slot)] => Ok(route(slot)),
        _ => Err(web_error(
            "WEB_PROVIDER_AMBIGUOUS",
            "multiple usable web providers are registered; configure one explicitly",
            json!({"kind": kind, "providers": usable.iter().map(|(id, _)| (*id).clone()).collect::<Vec<_>>() }),
        )),
    }
}

async fn read_body(
    response: reqwest::Response,
    config: &HttpFetchConfig,
    cancellation: CancellationToken,
) -> Result<(String, bool), TessivumError> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::with_capacity(config.max_body_bytes.min(8192));
    let mut truncated = false;
    while let Some(chunk) = tokio::select! {
        _ = cancellation.cancelled() => return Err(aborted_error()),
        chunk = stream.next() => chunk,
    } {
        let chunk = chunk.map_err(|error| {
            web_error(
                "WEB_PROVIDER_ERROR",
                "could not read HTTP response body",
                json!({"error": error.to_string()}),
            )
        })?;
        let remaining = config.max_body_bytes.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    let decoded = String::from_utf8_lossy(&bytes);
    let mut text = String::with_capacity(decoded.len().min(config.max_body_chars));
    for (index, character) in decoded.chars().enumerate() {
        if index == config.max_body_chars {
            truncated = true;
            break;
        }
        text.push(character);
    }
    Ok((text, truncated))
}

fn validate_fetch_url(input: &str) -> Result<Url, TessivumError> {
    let url = Url::parse(input).map_err(|_| {
        web_error(
            "WEB_FETCH_INVALID_URL",
            "web fetch requires an absolute HTTP(S) URL",
            json!({"url": input}),
        )
    })?;
    validate_url(&url)?;
    Ok(url)
}

fn validate_url(url: &Url) -> Result<(), TessivumError> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(web_error(
            "WEB_FETCH_INVALID_URL",
            "web fetch requires an absolute HTTP(S) URL",
            json!({"url": url.as_str()}),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(web_error(
            "WEB_FETCH_CREDENTIALS_FORBIDDEN",
            "web fetch URLs must not contain credentials",
            json!({"url": url.as_str()}),
        ));
    }
    Ok(())
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case(right.host_str().unwrap_or_default()))
        && left.port_or_known_default() == right.port_or_known_default()
}

fn env_provider(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn validate_provider_id(id: String) -> Result<String, TessivumError> {
    if id.trim().is_empty() {
        return Err(web_error(
            "INVALID_WEB_PROVIDER",
            "web provider identifiers must not be empty",
            Value::Null,
        ));
    }
    Ok(id)
}

fn next_registration_id(registry: &mut WebRegistry) -> u64 {
    registry.next_id = registry.next_id.wrapping_add(1).max(1);
    registry.next_id
}

fn duplicate_provider(id: &str) -> TessivumError {
    web_error(
        "WEB_DUPLICATE_PROVIDER",
        "a web provider is already registered under this identifier",
        json!({"provider": id}),
    )
}

fn aborted_error() -> TessivumError {
    web_error("WEB_ABORTED", "web operation was cancelled", Value::Null)
}

fn web_error(code: impl Into<String>, message: impl Into<String>, details: Value) -> TessivumError {
    TessivumError::new(code, message, "web", details)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{json, Value};
use tessivum::{
    mcp::{
        public_tool_name, McpClientConfig, McpConnection, McpConnector, McpReconnectPolicy,
        McpTaskSupport, McpTool, McpToolPage, McpTransport,
    },
    tools::{ToolRunContext, ToolRuntime},
    web::{
        WebBody, WebFetchProvider, WebFetchRequest, WebFetchResult, WebRuntime, WebRuntimeConfig,
        WebSearchProvider, WebSearchRequest, WebSearchResult, WebSearchSource,
    },
    SessionId, TessivumError, ToolCallId,
};
use tessivum_core::CancellationToken;

struct ScriptedTransport {
    pages: Mutex<VecDeque<McpToolPage>>,
    calls: Mutex<Vec<String>>,
    results: Mutex<VecDeque<Value>>,
}

impl ScriptedTransport {
    fn with_pages(pages: impl IntoIterator<Item = McpToolPage>) -> Self {
        Self {
            pages: Mutex::new(pages.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
            results: Mutex::new(VecDeque::from([
                json!({"content": [{"type": "text", "text": "ok"}]}),
            ])),
        }
    }
}

#[async_trait]
impl McpTransport for ScriptedTransport {
    async fn list_tools(
        &self,
        _cursor: Option<String>,
        _cancellation: CancellationToken,
    ) -> Result<McpToolPage, TessivumError> {
        self.pages
            .lock()
            .expect("script lock")
            .pop_front()
            .ok_or_else(|| err("MCP_SCRIPT_EXHAUSTED"))
    }

    async fn call_tool(
        &self,
        raw_name: &str,
        _arguments: Value,
        _cancellation: CancellationToken,
    ) -> Result<Value, TessivumError> {
        self.calls
            .lock()
            .expect("script lock")
            .push(raw_name.to_owned());
        Ok(self
            .results
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or_else(|| json!({"content": []})))
    }

    async fn close(&self) -> Result<(), TessivumError> {
        Ok(())
    }
}

struct ScriptedConnector {
    transports: Mutex<VecDeque<Result<Arc<dyn McpTransport>, TessivumError>>>,
}

#[async_trait]
impl McpConnector for ScriptedConnector {
    async fn connect(
        &self,
        _config: &McpClientConfig,
    ) -> Result<Arc<dyn McpTransport>, TessivumError> {
        self.transports
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or_else(|| Err(err("MCP_SCRIPT_EXHAUSTED")))
    }
}

fn tool(name: &str) -> McpTool {
    McpTool {
        name: name.to_owned(),
        description: "scripted tool".into(),
        input_schema: json!({"type": "object", "additionalProperties": true}),
        task_support: McpTaskSupport::Optional,
    }
}

fn page(tools: Vec<McpTool>, next_cursor: Option<&str>) -> McpToolPage {
    McpToolPage {
        tools,
        next_cursor: next_cursor.map(str::to_owned),
    }
}

fn err(code: &str) -> TessivumError {
    TessivumError::new(code, code, "test", Value::Null)
}

fn context() -> ToolRunContext {
    ToolRunContext {
        session: SessionId::from("session"),
        call: ToolCallId::from("call"),
        cancellation: tessivum_core::ContextHandle::root().scope().cancellation(),
    }
}

#[tokio::test]
async fn mcp_paginates_keeps_raw_mapping_and_replaces_snapshot_atomically() {
    let first = Arc::new(ScriptedTransport::with_pages([
        page(vec![tool("raw/name")], Some("next")),
        page(vec![tool("later")], None),
    ]));
    let connector = Arc::new(ScriptedConnector {
        transports: Mutex::new(VecDeque::from([Ok(
            Arc::clone(&first) as Arc<dyn McpTransport>
        )])),
    });
    let runtime = ToolRuntime::new();
    let connection = McpConnection::connect(
        McpClientConfig::new("server").expect("valid config"),
        runtime.clone(),
        connector,
    )
    .await
    .expect("initial snapshot connects");

    let public = public_tool_name("server", "raw/name");
    assert_eq!(connection.raw_name(&public).as_deref(), Some("raw/name"));
    assert_eq!(
        runtime.schemas().len(),
        2,
        "both paginated tools install together"
    );
    let output = runtime.execute(context(), &public, json!({})).await;
    assert!(!output.is_error);
    assert_eq!(
        first.calls.lock().expect("script lock").as_slice(),
        ["raw/name"]
    );

    let duplicate = Arc::new(ScriptedTransport::with_pages([page(
        vec![tool("same"), tool("same")],
        None,
    )]));
    let connector = Arc::new(ScriptedConnector {
        transports: Mutex::new(VecDeque::from([Ok(
            Arc::clone(&duplicate) as Arc<dyn McpTransport>
        )])),
    });
    let second_runtime = ToolRuntime::new();
    assert_eq!(
        McpConnection::connect(
            McpClientConfig::new("server").expect("valid config"),
            second_runtime.clone(),
            connector,
        )
        .await
        .expect_err("duplicate remote names reject")
        .code,
        "DUPLICATE_MCP_TOOL"
    );
    assert!(
        second_runtime.schemas().is_empty(),
        "failed snapshot registers no partial tools"
    );
}

#[tokio::test]
async fn mcp_reconnect_exhaustion_removes_owned_tools() {
    let initial = Arc::new(ScriptedTransport::with_pages([page(
        vec![tool("alive")],
        None,
    )]));
    let connector = Arc::new(ScriptedConnector {
        transports: Mutex::new(VecDeque::from([
            Ok(Arc::clone(&initial) as Arc<dyn McpTransport>),
            Err(err("MCP_CONNECT_FAILED")),
            Err(err("MCP_CONNECT_FAILED")),
        ])),
    });
    let mut config = McpClientConfig::new("server").expect("valid config");
    config.reconnect = McpReconnectPolicy {
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
        stability_window: Duration::from_millis(1),
        max_attempts: 2,
        ..McpReconnectPolicy::default()
    };
    let runtime = ToolRuntime::new();
    let connection = McpConnection::connect(config, runtime.clone(), connector)
        .await
        .expect("initial connection");
    assert_eq!(runtime.schemas().len(), 1);

    connection.closed(1).await;
    assert!(
        runtime.schemas().is_empty(),
        "exhaustion leaves no stale MCP tools"
    );
    assert_eq!(
        connection.ready().await.expect_err("exhausted").code,
        "MCP_RECONNECT_EXHAUSTED"
    );
    connection.dispose().await.expect("idempotent dispose");
}

struct Search {
    available: bool,
    sources: Vec<WebSearchSource>,
}

#[async_trait]
impl WebSearchProvider for Search {
    fn available(&self) -> Result<bool, TessivumError> {
        Ok(self.available)
    }

    async fn search(
        &self,
        _request: WebSearchRequest,
        _cancellation: CancellationToken,
    ) -> Result<WebSearchResult, TessivumError> {
        Ok(WebSearchResult {
            sources: self.sources.clone(),
            truncated: false,
        })
    }
}

struct Fetch;

#[async_trait]
impl WebFetchProvider for Fetch {
    fn available(&self) -> Result<bool, TessivumError> {
        Ok(true)
    }

    async fn fetch(
        &self,
        request: WebFetchRequest,
        _cancellation: CancellationToken,
    ) -> Result<WebFetchResult, TessivumError> {
        Ok(WebFetchResult {
            final_url: request.url,
            status_code: 404,
            body: WebBody::Text {
                text: "not found".into(),
            },
            truncated: false,
        })
    }
}

fn cancellation() -> CancellationToken {
    tessivum_core::ContextHandle::root().scope().cancellation()
}

#[tokio::test]
async fn web_selection_is_unambiguous_and_search_is_capped() {
    let runtime = WebRuntime::new(WebRuntimeConfig::default()).expect("config");
    assert_eq!(
        runtime
            .search(
                WebSearchRequest {
                    query: "q".into(),
                    max_results: None
                },
                cancellation()
            )
            .await
            .expect_err("no providers")
            .code,
        "WEB_PROVIDER_UNAVAILABLE"
    );
    let missing = WebRuntime::new(WebRuntimeConfig {
        search_provider: Some("chosen".into()),
        ..WebRuntimeConfig::default()
    })
    .expect("config");
    assert_eq!(
        missing
            .search(
                WebSearchRequest {
                    query: "q".into(),
                    max_results: None
                },
                cancellation()
            )
            .await
            .expect_err("configured provider is absent")
            .code,
        "WEB_PROVIDER_CONFIGURED_MISSING"
    );
    let unavailable = WebRuntime::new(WebRuntimeConfig {
        search_provider: Some("chosen".into()),
        ..WebRuntimeConfig::default()
    })
    .expect("config");
    let _chosen = unavailable
        .register_search(
            "chosen",
            Arc::new(Search {
                available: false,
                sources: vec![],
            }),
        )
        .expect("registration");
    assert_eq!(
        unavailable
            .search(
                WebSearchRequest {
                    query: "q".into(),
                    max_results: None
                },
                cancellation()
            )
            .await
            .expect_err("configured provider is unavailable")
            .code,
        "WEB_PROVIDER_CONFIGURED_UNAVAILABLE"
    );

    let first = runtime
        .register_search(
            "first",
            Arc::new(Search {
                available: true,
                sources: vec![],
            }),
        )
        .expect("first registration");
    let _second = runtime
        .register_search(
            "second",
            Arc::new(Search {
                available: true,
                sources: vec![],
            }),
        )
        .expect("second registration");
    assert_eq!(
        runtime
            .search(
                WebSearchRequest {
                    query: "q".into(),
                    max_results: None
                },
                cancellation()
            )
            .await
            .expect_err("ambiguous providers")
            .code,
        "WEB_PROVIDER_AMBIGUOUS"
    );
    assert_eq!(
        runtime
            .register_search(
                "first",
                Arc::new(Search {
                    available: true,
                    sources: vec![]
                })
            )
            .expect_err("duplicate registration")
            .code,
        "WEB_DUPLICATE_PROVIDER"
    );
    drop(first);

    let configured = WebRuntime::new(WebRuntimeConfig {
        search_provider: Some("only".into()),
        fetch_provider: None,
        max_search_results: 1,
    })
    .expect("config");
    let _only = configured
        .register_search(
            "only",
            Arc::new(Search {
                available: true,
                sources: vec![
                    WebSearchSource {
                        title: "one".into(),
                        url: "https://one.example".into(),
                        snippet: None,
                    },
                    WebSearchSource {
                        title: "two".into(),
                        url: "https://two.example".into(),
                        snippet: None,
                    },
                ],
            }),
        )
        .expect("registration");
    let result = configured
        .search(
            WebSearchRequest {
                query: "q".into(),
                max_results: Some(9),
            },
            cancellation(),
        )
        .await
        .expect("configured provider selected");
    assert_eq!(result.sources.len(), 1);
    assert!(result.truncated);
}

#[tokio::test]
async fn web_fetch_rejects_unsafe_urls_but_preserves_http_error_results() {
    let runtime = WebRuntime::new(WebRuntimeConfig::default()).expect("config");
    let _fetch = runtime
        .register_fetch("test", Arc::new(Fetch))
        .expect("provider");
    for url in ["file:///etc/passwd", "https://user:secret@example.test/"] {
        assert!(matches!(
            runtime.fetch(WebFetchRequest { url: url.into() }, cancellation()).await,
            Err(TessivumError { code, .. }) if code == "WEB_FETCH_INVALID_URL" || code == "WEB_FETCH_CREDENTIALS_FORBIDDEN"
        ));
    }
    let result = runtime
        .fetch(
            WebFetchRequest {
                url: "https://example.test/missing".into(),
            },
            cancellation(),
        )
        .await
        .expect("HTTP status is result data");
    assert_eq!(result.status_code, 404);
    assert!(matches!(result.body, WebBody::Text { .. }));
}

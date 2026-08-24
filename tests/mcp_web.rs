use std::{
    collections::VecDeque,
    future,
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{json, Value};
use tessivum::{
    mcp::{
        public_tool_name, McpClientConfig, McpConnection, McpConnector, McpReconnectPolicy,
        McpTaskSupport, McpTool, McpToolPage, McpTransport,
    },
    tools::{ToolApproval, ToolRestrictions, ToolRunContext, ToolRuntime},
    web::{
        HttpFetchConfig, HttpFetchProvider, WebBody, WebFetchProvider, WebFetchRequest,
        WebFetchResult, WebRuntime, WebRuntimeConfig, WebSearchProvider, WebSearchRequest,
        WebSearchResult, WebSearchSource,
    },
    SessionId, TessivumError, ToolCallId,
};
use tessivum_core::CancellationToken;
use tokio::{sync::Notify, time};

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
                        published_at: None,
                    },
                    WebSearchSource {
                        title: "two".into(),
                        url: "https://two.example".into(),
                        snippet: None,
                        published_at: None,
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

#[tokio::test]
async fn mcp_rejects_oversized_page_schema_result_and_arguments() {
    let mut invalid = McpClientConfig::new("server").expect("valid config");
    invalid.max_pages = 0;
    assert_eq!(
        invalid
            .validate()
            .expect_err("zero page limit is invalid")
            .code,
        "INVALID_MCP_CONFIG"
    );

    let page_transport = Arc::new(ScriptedTransport::with_pages([page(
        vec![tool("too-large-page")],
        None,
    )]));
    let mut page_config = McpClientConfig::new("server").expect("valid config");
    page_config.max_decoded_json_bytes = 1;
    assert_eq!(
        McpConnection::connect(
            page_config,
            ToolRuntime::new(),
            Arc::new(ScriptedConnector {
                transports: Mutex::new(VecDeque::from([Ok(
                    Arc::clone(&page_transport) as Arc<dyn McpTransport>,
                )])),
            }),
        )
        .await
        .expect_err("oversized page is rejected")
        .code,
        "MCP_JSON_LIMIT"
    );

    let mut oversized_schema = tool("schema");
    oversized_schema.input_schema = json!({"type": "object", "description": "x".repeat(128)});
    let schema_transport = Arc::new(ScriptedTransport::with_pages([page(
        vec![oversized_schema],
        None,
    )]));
    let mut schema_config = McpClientConfig::new("server").expect("valid config");
    schema_config.max_schema_bytes = 1;
    assert_eq!(
        McpConnection::connect(
            schema_config,
            ToolRuntime::new(),
            Arc::new(ScriptedConnector {
                transports: Mutex::new(VecDeque::from([Ok(
                    Arc::clone(&schema_transport) as Arc<dyn McpTransport>,
                )])),
            }),
        )
        .await
        .expect_err("oversized schema is rejected")
        .code,
        "MCP_SCHEMA_LIMIT"
    );

    let result_transport = Arc::new(ScriptedTransport {
        pages: Mutex::new(VecDeque::from([page(vec![tool("result")], None)])),
        calls: Mutex::new(Vec::new()),
        results: Mutex::new(VecDeque::from([json!({"content": "x".repeat(128)})])),
    });
    let mut result_config = McpClientConfig::new("server").expect("valid config");
    result_config.max_result_bytes = 1;
    let result_runtime = ToolRuntime::new();
    let _result_connection = McpConnection::connect(
        result_config,
        result_runtime.clone(),
        Arc::new(ScriptedConnector {
            transports: Mutex::new(VecDeque::from([Ok(
                Arc::clone(&result_transport) as Arc<dyn McpTransport>
            )])),
        }),
    )
    .await
    .expect("result limit does not reject a tool listing");
    let result = result_runtime
        .execute(context(), public_tool_name("server", "result"), json!({}))
        .await;
    assert_eq!(result.meta["code"], "MCP_RESULT_LIMIT");

    let arguments_transport = Arc::new(ScriptedTransport::with_pages([page(
        vec![tool("arguments")],
        None,
    )]));
    let mut arguments_config = McpClientConfig::new("server").expect("valid config");
    arguments_config.max_args_bytes = 1;
    let arguments_runtime = ToolRuntime::new();
    let _arguments_connection = McpConnection::connect(
        arguments_config,
        arguments_runtime.clone(),
        Arc::new(ScriptedConnector {
            transports: Mutex::new(VecDeque::from([Ok(
                Arc::clone(&arguments_transport) as Arc<dyn McpTransport>
            )])),
        }),
    )
    .await
    .expect("argument limit does not reject a tool listing");
    let arguments = arguments_runtime
        .execute(
            context(),
            public_tool_name("server", "arguments"),
            json!({"value": "x".repeat(128)}),
        )
        .await;
    assert_eq!(arguments.meta["code"], "MCP_ARGUMENTS_LIMIT");
}

struct HangingConnector;

#[async_trait]
impl McpConnector for HangingConnector {
    async fn connect(
        &self,
        _config: &McpClientConfig,
    ) -> Result<Arc<dyn McpTransport>, TessivumError> {
        future::pending().await
    }
}

struct HangingListTransport {
    initial: Mutex<Option<McpToolPage>>,
    listed: Arc<Notify>,
}

#[async_trait]
impl McpTransport for HangingListTransport {
    async fn list_tools(
        &self,
        _cursor: Option<String>,
        _cancellation: CancellationToken,
    ) -> Result<McpToolPage, TessivumError> {
        if let Some(page) = self.initial.lock().expect("initial lock").take() {
            return Ok(page);
        }
        self.listed.notify_one();
        future::pending().await
    }

    async fn call_tool(
        &self,
        _raw_name: &str,
        _arguments: Value,
        _cancellation: CancellationToken,
    ) -> Result<Value, TessivumError> {
        Err(err("MCP_UNEXPECTED_CALL"))
    }

    async fn close(&self) -> Result<(), TessivumError> {
        Ok(())
    }
}

#[tokio::test]
async fn mcp_bounds_hanging_connect_and_disposes_during_hanging_sync() {
    let mut connect_config = McpClientConfig::new("server").expect("valid config");
    connect_config.timeout = Duration::from_millis(20);
    assert_eq!(
        McpConnection::connect(
            connect_config,
            ToolRuntime::new(),
            Arc::new(HangingConnector)
        )
        .await
        .expect_err("hanging connector is timed out")
        .code,
        "MCP_CONNECT_TIMEOUT"
    );

    let listed = Arc::new(Notify::new());
    let transport = Arc::new(HangingListTransport {
        initial: Mutex::new(Some(page(vec![tool("alive")], None))),
        listed: Arc::clone(&listed),
    });
    let mut sync_config = McpClientConfig::new("server").expect("valid config");
    sync_config.timeout = Duration::from_secs(1);
    let connection = McpConnection::connect(
        sync_config,
        ToolRuntime::new(),
        Arc::new(ScriptedConnector {
            transports: Mutex::new(VecDeque::from([Ok(
                Arc::clone(&transport) as Arc<dyn McpTransport>
            )])),
        }),
    )
    .await
    .expect("initial synchronization succeeds");
    let sync = tokio::spawn({
        let connection = connection.clone();
        async move { connection.sync_tools().await }
    });
    listed.notified().await;
    time::timeout(Duration::from_millis(100), connection.dispose())
        .await
        .expect("dispose is not blocked by a list call")
        .expect("close succeeds");
    assert_eq!(
        sync.await
            .expect("sync task completes")
            .expect_err("disposed synchronization fails")
            .code,
        "MCP_DISPOSED"
    );
}

struct PausingApproval {
    entered: Arc<Notify>,
    released: Arc<Notify>,
}

#[async_trait]
impl ToolApproval for PausingApproval {
    async fn approve(
        &self,
        _context: &ToolRunContext,
        _schema: &tessivum::ToolSchema,
        _arguments: &Value,
    ) -> Result<Option<bool>, TessivumError> {
        self.entered.notify_one();
        self.released.notified().await;
        Ok(Some(true))
    }
}

#[tokio::test]
async fn mcp_stale_cloned_handler_fails_after_reconnect() {
    let public = public_tool_name("server", "tool");
    let runtime = ToolRuntime::new()
        .scoped(ToolRestrictions::new().ask(public.clone()))
        .expect("valid restricted runtime");
    let entered = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    runtime.set_approval(Some(Arc::new(PausingApproval {
        entered: Arc::clone(&entered),
        released: Arc::clone(&released),
    })));
    let first = Arc::new(ScriptedTransport::with_pages([page(
        vec![tool("tool")],
        None,
    )]));
    let second = Arc::new(ScriptedTransport::with_pages([page(
        vec![tool("tool")],
        None,
    )]));
    let connection = McpConnection::connect(
        McpClientConfig::new("server").expect("valid config"),
        runtime.clone(),
        Arc::new(ScriptedConnector {
            transports: Mutex::new(VecDeque::from([
                Ok(Arc::clone(&first) as Arc<dyn McpTransport>),
                Ok(Arc::clone(&second) as Arc<dyn McpTransport>),
            ])),
        }),
    )
    .await
    .expect("initial connection");
    let call = tokio::spawn({
        let runtime = runtime.clone();
        let public = public.clone();
        async move { runtime.execute(context(), public, json!({})).await }
    });
    entered.notified().await;
    connection
        .reconnect()
        .await
        .expect("new generation connects");
    released.notify_one();
    let output = call.await.expect("call task completes");
    assert!(output.is_error);
    assert_eq!(output.meta["code"], "MCP_GENERATION_REPLACED");
}

#[tokio::test]
async fn web_fetch_timeout_covers_a_header_then_drip_body() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener binds");
    let address = listener.local_addr().expect("listener address");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("client connects");
        let mut request = [0; 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na")
            .expect("headers write");
        stream.flush().expect("headers flush");
        thread::sleep(Duration::from_millis(150));
        let _ = stream.write_all(b"b");
    });
    let provider = HttpFetchProvider::new(HttpFetchConfig {
        timeout: Duration::from_millis(50),
        ..HttpFetchConfig::default()
    })
    .expect("provider");
    assert_eq!(
        provider
            .fetch(
                WebFetchRequest {
                    url: format!("http://{address}/"),
                },
                cancellation(),
            )
            .await
            .expect_err("body drip exceeds whole-fetch deadline")
            .code,
        "WEB_FETCH_TIMEOUT"
    );
    server.join().expect("server completes");
}

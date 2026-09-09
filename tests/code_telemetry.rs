use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tessivum::{
    code_runtime::{
        register_code_tool, CodeBindingNamespace, CodeRunFailureKind, CodeRunRequest, CodeRuntime,
        CodeRuntimeError, JavaScriptRuntime, ProcessCodeRuntime, ProcessCodeRuntimeConfig,
        PTC_RUNTIME_UNAVAILABLE,
    },
    invariants::{InvariantConfig, InvariantInstallerError, InvariantRegistry},
    telemetry::{
        TelemetryBackend, TelemetryChannel, TelemetryCoordinator, TelemetryError, TelemetryRecord,
        TelemetryRedactor, TelemetrySeverity, TelemetrySharing,
    },
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRestrictions,
        ToolRunContext, ToolRuntime,
    },
    ContentBlock, SessionEvent, SessionId, ToolCallId,
};
use tessivum_core::{CancellationToken, ContextHandle};

fn runtime(cap: usize) -> ProcessCodeRuntime {
    let mut config = ProcessCodeRuntimeConfig::ptc_javascript()
        .expect("Bun is required for PTC runtime tests; install a usable bun executable");
    config.max_output_bytes = cap;
    ProcessCodeRuntime::new(config).expect("runtime")
}
fn event(seq: u64, kind: &str, data: Value) -> SessionEvent {
    SessionEvent {
        event_type: kind.into(),
        seq,
        time: seq,
        data,
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
    }
}

#[test]
fn ptc_bun_configuration_resolves_and_reports_unavailable_executables() {
    let config = ProcessCodeRuntimeConfig::ptc_javascript()
        .expect("Bun is required for PTC runtime tests; install a usable bun executable");
    assert_eq!(config.javascript_runtime, JavaScriptRuntime::Bun);
    assert!(config.executable.is_absolute());

    let unavailable = match ProcessCodeRuntime::new(ProcessCodeRuntimeConfig::ptc_javascript_with(
        std::env::temp_dir().join(format!(
            "tessivum-ptc-bun-{}-does-not-exist",
            std::process::id(),
        )),
    )) {
        Err(error) => error,
        Ok(_) => panic!("missing PTC Bun executable must fail configuration"),
    };
    assert!(matches!(
        unavailable,
        CodeRuntimeError::PtcRuntimeUnavailable(_)
    ));
    assert_eq!(unavailable.diagnostic_code(), Some(PTC_RUNTIME_UNAVAILABLE));
}

#[tokio::test]
async fn ptc_bun_worker_invocation_uses_bun() {
    let result = runtime(1024)
        .run(CodeRunRequest::new(
            "return { bun: process.versions.bun, argv: process.argv }",
            vec![],
        ))
        .await
        .expect("service call");
    let value = result.value.expect("Bun worker completion");
    assert!(value["bun"]
        .as_str()
        .is_some_and(|version| !version.is_empty()));
    let argv = value["argv"].as_array().expect("Bun eval argv");
    assert_eq!(argv.len(), 1);
    assert!(argv[0].as_str().is_some_and(|path| path.contains("bun")));
}

#[tokio::test]
async fn ptc_bun_validates_binding_arguments_and_orders_nested_calls() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let first_calls = calls.clone();
    let second_calls = calls.clone();
    let result = runtime(1024)
        .run(CodeRunRequest::new(
            "const second = await tools.second({first: await tools.first({})}); try { await tools.first({nan: NaN}); } catch (error) { return {second, rejection: error.message}; }",
            vec![CodeBindingNamespace::new("tools")
                .function("first", move |_| {
                    let calls = first_calls.clone();
                    async move {
                        calls.lock().push("first");
                        Ok(json!("first"))
                    }
                })
                .function("second", move |_| {
                    let calls = second_calls.clone();
                    async move {
                        calls.lock().push("second");
                        Ok(json!("second"))
                    }
                })],
        ))
        .await
        .expect("service call");
    assert_eq!(
        result.value,
        Some(json!({
            "second": "second",
            "rejection": "binding arguments must be lossless JSON",
        }))
    );
    assert_eq!(*calls.lock(), ["first", "second"]);
}

#[tokio::test]
async fn process_runtime_resolves_success_exception_invalid_output_and_limit() {
    let result = runtime(1024)
        .run(CodeRunRequest::new(
            "console.log('ok'); return await tools.echo({n: 1})",
            vec![CodeBindingNamespace::new("tools")
                .function("echo", |value| async move { Ok(value) })],
        ))
        .await
        .expect("service call");
    assert_eq!(result.value, Some(json!({"n": 1})));
    assert_eq!(result.logs, ["ok"]);
    let exception = runtime(1024)
        .run(CodeRunRequest::new("throw new Error('bad')", vec![]))
        .await
        .expect("result");
    assert_eq!(
        exception.error.expect("failure").kind,
        CodeRunFailureKind::Exception
    );
    let invalid = runtime(1024)
        .run(CodeRunRequest::new("return () => {}", vec![]))
        .await
        .expect("result");
    assert_eq!(
        invalid.error.expect("failure").kind,
        CodeRunFailureKind::InvalidOutput
    );
    let limited = runtime(16)
        .run(CodeRunRequest::new(
            "console.log('this cannot fit')",
            vec![],
        ))
        .await
        .expect("result");
    assert_eq!(
        limited.error.expect("failure").kind,
        CodeRunFailureKind::OutputLimit
    );
}

struct CountingOutput {
    calls: Arc<AtomicUsize>,
    value: Value,
}

#[async_trait]
impl ToolHandler for CountingOutput {
    async fn run(&self, _: ToolRunContext, _: Value) -> ToolHandlerResult {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(ToolOutput::new(Vec::new(), false, self.value.clone()))
    }
}

fn tool_context(call: &str, cancellation: CancellationToken) -> ToolRunContext {
    ToolRunContext {
        session: SessionId::from("code-runtime-test"),
        call: ToolCallId::from(call),
        cancellation,
    }
}

fn text_json(output: &ToolOutput) -> Value {
    let ContentBlock::Text { text } = &output.content[0] else {
        panic!("run_code must return text")
    };
    serde_json::from_str(text).expect("run_code JSON result")
}

#[tokio::test]
async fn flat_tool_name_errors_recover_without_dispatching_hidden_or_unknown_tools() {
    let native = ToolRuntime::new();
    let visible_calls = Arc::new(AtomicUsize::new(0));
    let hidden_calls = Arc::new(AtomicUsize::new(0));
    let _visible = native
        .register(ToolDefinition::new(
            "jobs.list",
            "list",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            CountingOutput {
                calls: Arc::clone(&visible_calls),
                value: json!({"listed": true}),
            },
        ))
        .expect("visible tool");
    let _hidden = native
        .register(ToolDefinition::new(
            "jobs.secret",
            "hidden",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            CountingOutput {
                calls: Arc::clone(&hidden_calls),
                value: json!({"secret": true}),
            },
        ))
        .expect("hidden tool");
    let dispatch = native
        .scoped(ToolRestrictions::new().deny("jobs.secret"))
        .expect("restricted tools");
    let tools = ToolRuntime::new();
    let _run_code = register_code_tool(&tools, dispatch, runtime(4096)).expect("run_code");
    let cancellation = ContextHandle::root().scope().cancellation();
    let output = tools
        .execute(
            tool_context("flat-names", cancellation),
            "run_code",
            json!({
                "description": "exercise flat bindings",
                "code": r#"
                    let nested, hidden, unknown;
                    try { await tools.jobs.list({}); } catch (error) { nested = {name: error.name, toolName: error.toolName, message: error.message}; }
                    const keys = Object.keys(tools);
                    const valid = await tools["jobs.list"]({});
                    try { await tools["jobs.secret"]({}); } catch (error) { hidden = {name: error.name, toolName: error.toolName, message: error.message}; }
                    try { await tools.unknown({}); } catch (error) { unknown = {name: error.name, toolName: error.toolName, message: error.message}; }
                    return {nested, hidden, unknown, keys, valid};
                "#,
            }),
        )
        .await;
    assert!(!output.is_error);
    let value = text_json(&output);
    assert_eq!(value["nested"]["name"], "ToolError");
    assert_eq!(value["nested"]["toolName"], "jobs");
    assert!(value["nested"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("tools[\"jobs.list\"]")));
    assert_eq!(value["valid"]["listed"], true);
    assert_eq!(value["keys"], json!(["jobs.list"]));
    assert_eq!(value["hidden"]["name"], "ToolError");
    assert_eq!(value["unknown"]["name"], "ToolError");
    assert_eq!(visible_calls.load(Ordering::Acquire), 1);
    assert_eq!(hidden_calls.load(Ordering::Acquire), 0);
    assert_eq!(output.meta["codeDispatches"].as_array().unwrap().len(), 2);
}

struct BlockingOutput {
    tokens: Arc<Mutex<BTreeMap<String, CancellationToken>>>,
    first_started: Arc<tokio::sync::Notify>,
    second_started: Arc<tokio::sync::Notify>,
    detached_started: Arc<tokio::sync::Notify>,
    second_release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ToolHandler for BlockingOutput {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        let id = arguments["id"].as_str().expect("id").to_owned();
        self.tokens
            .lock()
            .insert(id.clone(), context.cancellation.clone());
        match id.as_str() {
            "first" => {
                self.first_started.notify_one();
                context.cancellation.cancelled().await;
                Ok(ToolOutput::new(Vec::new(), false, Value::Null))
            }
            "second" => {
                self.second_started.notify_one();
                tokio::select! {
                    _ = context.cancellation.cancelled() => Ok(ToolOutput::new(Vec::new(), false, Value::Null)),
                    _ = self.second_release.notified() => Ok(ToolOutput::new(Vec::new(), false, json!({"released": true}))),
                }
            }
            "detached" => {
                self.detached_started.notify_one();
                context.cancellation.cancelled().await;
                Ok(ToolOutput::new(Vec::new(), false, Value::Null))
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn run_code_cancellation_is_confined_and_pending_dispatches_remain_actionable() {
    let tokens = Arc::new(Mutex::new(BTreeMap::new()));
    let first_started = Arc::new(tokio::sync::Notify::new());
    let second_started = Arc::new(tokio::sync::Notify::new());
    let detached_started = Arc::new(tokio::sync::Notify::new());
    let second_release = Arc::new(tokio::sync::Notify::new());
    let native = ToolRuntime::new();
    let _blocking = native
        .register(ToolDefinition::new(
            "block",
            "block",
            json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}),
            BlockingOutput {
                tokens: Arc::clone(&tokens),
                first_started: Arc::clone(&first_started),
                second_started: Arc::clone(&second_started),
                detached_started: Arc::clone(&detached_started),
                second_release: Arc::clone(&second_release),
            },
        ))
        .expect("blocking tool");
    let mut config = ProcessCodeRuntimeConfig::ptc_javascript()
        .expect("Bun is required for PTC runtime tests; install a usable bun executable");
    config.timeout = std::time::Duration::from_millis(300);
    let tools = ToolRuntime::new();
    let _run_code = register_code_tool(
        &tools,
        native,
        ProcessCodeRuntime::new(config).expect("runtime"),
    )
    .expect("run_code");

    let first_parent = ContextHandle::root().scope().cancellation();
    let first = {
        let tools = tools.clone();
        let cancellation = first_parent.clone();
        tokio::spawn(async move {
            tools
                .execute(
                    tool_context("flat-cancel", cancellation),
                    "run_code",
                    json!({"description":"timeout", "code":"return await tools.block({id: 'first'});"}),
                )
                .await
        })
    };
    first_started.notified().await;
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;

    let second_parent = ContextHandle::root().scope().cancellation();
    let second = {
        let tools = tools.clone();
        let cancellation = second_parent.clone();
        tokio::spawn(async move {
            tools
                .execute(
                    tool_context("later-run", cancellation),
                    "run_code",
                    json!({"description":"independent", "code":"return await tools.block({id: 'second'});"}),
                )
                .await
        })
    };
    second_started.notified().await;

    let first = first.await.expect("first run");
    assert!(first.is_error);
    let message = match &first.content[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("timeout must be text"),
    };
    assert!(message.contains("flat-cancel:code:1"));
    assert!(message.contains("cleanup may still be in progress"));
    assert_eq!(
        first.meta["codeDispatches"][1]["data"]["status"],
        "cancellation-signalled"
    );
    assert!(!first_parent.is_cancelled());
    assert!(!second_parent.is_cancelled());
    assert!(tokens.lock()["first"].is_cancelled());
    assert!(!tokens.lock()["second"].is_cancelled());

    second_release.notify_one();
    let second = second.await.expect("second run");
    assert!(!second.is_error);
    assert_eq!(text_json(&second)["released"], true);

    let detached = {
        let tools = tools.clone();
        tokio::spawn(async move {
            tools
                .execute(
                    tool_context(
                        "completed-with-pending",
                        ContextHandle::root().scope().cancellation(),
                    ),
                    "run_code",
                    json!({
                        "description":"return with pending call",
                        "code":"tools.block({id: 'detached'}); await new Promise(resolve => setTimeout(resolve, 25)); return 'done';"
                    }),
                )
                .await
        })
    };
    detached_started.notified().await;
    let detached = detached.await.expect("completed run");
    assert!(!detached.is_error);
    assert!(tokens.lock()["detached"].is_cancelled());
    assert!(detached.meta["codeDispatches"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| {
            event["data"]["subCallId"] == "completed-with-pending:code:1"
                && event["data"]["status"] == "cancellation-signalled"
        }));
}

#[tokio::test]
async fn process_runtime_timeout_abort_isolation_and_disposal() {
    let mut config = ProcessCodeRuntimeConfig::ptc_javascript()
        .expect("Bun is required for PTC runtime tests; install a usable bun executable");
    config.timeout = std::time::Duration::from_millis(30);
    let slow = ProcessCodeRuntime::new(config).expect("runtime");
    assert_eq!(
        slow.run(CodeRunRequest::new("for (;;) {}", vec![]))
            .await
            .expect("result")
            .error
            .expect("failure")
            .kind,
        CodeRunFailureKind::Timeout
    );
    let code = runtime(1024);
    let root = ContextHandle::root();
    let request =
        CodeRunRequest::new("for (;;) {}", vec![]).cancelled_by(root.scope().cancellation());
    let running = {
        let code = code.clone();
        tokio::spawn(async move { code.run(request).await.expect("result") })
    };
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    root.scope().dispose().await.expect("cancel");
    assert_eq!(
        running.await.expect("join").error.expect("failure").kind,
        CodeRunFailureKind::Abort
    );
    assert_eq!(
        code.run(CodeRunRequest::new(
            "globalThis.retained = 9; return 1",
            vec![]
        ))
        .await
        .expect("run")
        .value,
        Some(json!(1))
    );
    assert_eq!(
        code.run(CodeRunRequest::new(
            "return globalThis.retained || 0",
            vec![]
        ))
        .await
        .expect("run")
        .value,
        Some(json!(0))
    );
    let running = {
        let code = code.clone();
        tokio::spawn(async move {
            code.run(CodeRunRequest::new("for (;;) {}", vec![]))
                .await
                .expect("result")
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    code.dispose().await.expect("disposed");
    assert_eq!(
        running.await.expect("join").error.expect("failure").kind,
        CodeRunFailureKind::Abort
    );
}

#[tokio::test]
async fn dropped_process_run_releases_runtime_disposal() {
    let runtime = runtime(1024);
    let started = Arc::new(tokio::sync::Notify::new());
    let binding_started = Arc::clone(&started);
    let running = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .run(CodeRunRequest::new(
                    "return await tools.block({});",
                    vec![
                        CodeBindingNamespace::new("tools").function("block", move |_| {
                            let started = Arc::clone(&binding_started);
                            async move {
                                started.notify_one();
                                std::future::pending::<Result<Value, String>>().await
                            }
                        }),
                    ],
                ))
                .await
        })
    };
    started.notified().await;
    running.abort();
    assert!(running
        .await
        .expect_err("run must be dropped")
        .is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(1), runtime.dispose())
        .await
        .expect("disposal must not remain busy")
        .expect("dispose runtime");
}

#[derive(Default)]
struct Backend {
    records: Mutex<Vec<TelemetryRecord>>,
    reject: bool,
}
#[async_trait]
impl TelemetryBackend for Backend {
    fn sharing(&self) -> TelemetrySharing {
        TelemetrySharing::FeedbackOnly
    }
    async fn emit(&self, record: TelemetryRecord) -> Result<(), String> {
        if self.reject {
            Err("no".into())
        } else {
            self.records.lock().push(record);
            Ok(())
        }
    }
    async fn shutdown(&self) -> Result<(), String> {
        Ok(())
    }
}
struct Strip;
impl TelemetryRedactor for Strip {
    fn redact(&self, mut record: TelemetryRecord) -> Result<TelemetryRecord, TelemetryError> {
        record.body["secret"] = Value::String("[redacted]".into());
        Ok(record)
    }
}
struct Reject;
impl TelemetryRedactor for Reject {
    fn redact(&self, _: TelemetryRecord) -> Result<TelemetryRecord, TelemetryError> {
        Err(TelemetryError::Redactor("policy".into()))
    }
}

#[tokio::test]
async fn telemetry_redacts_deduplicates_samples_and_drains() {
    let backend = Arc::new(Backend::default());
    let telemetry = TelemetryCoordinator::new(backend.clone(), 8).expect("telemetry");
    telemetry.add_redactor(Arc::new(Strip));
    let source = event(
        1,
        "assistant/chunk",
        json!({"turn": 1, "step": 1, "secret": "keep"}),
    );
    assert!(telemetry.capture_event(&SessionId::from("s"), &source));
    assert!(!telemetry.capture_event(&SessionId::from("s"), &source));
    assert!(!telemetry.capture_event(
        &SessionId::from("s"),
        &event(2, "assistant/chunk", json!({"turn": 1, "step": 1}))
    ));
    telemetry.capture_ops(TelemetryRecord {
        channel: TelemetryChannel::Ops,
        time: 1,
        severity: TelemetrySeverity::Info,
        attributes: Default::default(),
        body: Value::Null,
    });
    telemetry.flush_hint();
    telemetry.shutdown_marker(SessionId::from("s"));
    telemetry.drain().await;
    assert_eq!(source.data["secret"], "keep");
    {
        let records = backend.records.lock();
        assert_eq!(records[0].body["secret"], "[redacted]");
        assert_eq!(records.len(), 3);
    }
    let blocked = TelemetryCoordinator::new(backend.clone(), 1).expect("telemetry");
    blocked.add_redactor(Arc::new(Reject));
    assert!(!blocked.capture_ops(TelemetryRecord {
        channel: TelemetryChannel::Ops,
        time: 1,
        severity: TelemetrySeverity::Info,
        attributes: Default::default(),
        body: json!({"secret":"never leaves"})
    }));
    blocked.drain().await;
    assert_eq!(backend.records.lock().len(), 3);
}

#[tokio::test]
async fn invariant_filters_reserve_and_release() {
    let registry = InvariantRegistry::new(
        ContextHandle::root(),
        InvariantConfig {
            enabled: Some(true),
            package_allowlist: vec!["^allowed$".into()],
            package_blocklist: vec!["blocked".into()],
        },
    )
    .expect("config");
    let installer = Arc::new(
        |_: ContextHandle, _: tessivum::invariants::InvariantFailure| async {
            Ok::<_, InvariantInstallerError>(())
        },
    );
    let filtered = registry
        .register("other", installer.clone())
        .await
        .expect("filtered reservation");
    assert!(registry.register("other", installer.clone()).await.is_err());
    filtered.dispose().await.expect("release");
    registry
        .register("other", installer.clone())
        .await
        .expect("released")
        .dispose()
        .await
        .expect("dispose");
    assert!(InvariantRegistry::new(
        ContextHandle::root(),
        InvariantConfig {
            enabled: None,
            package_allowlist: vec![" ".into()],
            package_blocklist: vec![]
        }
    )
    .is_err());
}

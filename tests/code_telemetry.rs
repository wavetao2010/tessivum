use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;
use tessivum::{
    code_runtime::{
        CodeBindingNamespace, CodeRunFailureKind, CodeRunRequest, CodeRuntime, CodeRuntimeError,
        JavaScriptRuntime, ProcessCodeRuntime, ProcessCodeRuntimeConfig, PTC_RUNTIME_UNAVAILABLE,
    },
    invariants::{InvariantConfig, InvariantInstallerError, InvariantRegistry},
    telemetry::{
        TelemetryBackend, TelemetryChannel, TelemetryCoordinator, TelemetryError, TelemetryRecord,
        TelemetryRedactor, TelemetrySeverity, TelemetrySharing,
    },
    SessionEvent, SessionId,
};
use tessivum_core::ContextHandle;

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

    let unavailable = match ProcessCodeRuntime::new(
        ProcessCodeRuntimeConfig::ptc_javascript_with(std::env::temp_dir().join(format!(
            "tessivum-ptc-bun-{}-does-not-exist",
            std::process::id(),
        ))),
    ) {
        Err(error) => error,
        Ok(_) => panic!("missing PTC Bun executable must fail configuration"),
    };
    assert!(matches!(unavailable, CodeRuntimeError::PtcRuntimeUnavailable(_)));
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
    assert!(value["bun"].as_str().is_some_and(|version| !version.is_empty()));
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
    assert_eq!(result.value, Some(json!({
        "second": "second",
        "rejection": "binding arguments must be lossless JSON",
    })));
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

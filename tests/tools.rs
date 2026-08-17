use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use parking_lot::Mutex as ParkingMutex;
use serde_json::{json, Value};
use tessivum::{
    tools::{
        tools_service_key, ToolApproval, ToolApprovalResult, ToolChange, ToolDefinition,
        ToolHandler, ToolHandlerResult, ToolOutput, ToolRestrictions, ToolRunContext, ToolRuntime,
    },
    ContentBlock, SessionId, ToolCallId, ToolSchema,
};
use tessivum_core::ContextHandle;
use tokio::sync::Notify;

#[derive(Clone)]
struct Echo {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolHandler for Echo {
    async fn run(&self, _context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: arguments.to_string(),
            }],
            false,
            json!({"private": "handler output"}),
        ))
    }
}

struct WaitingHandler {
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
}

#[async_trait]
impl ToolHandler for WaitingHandler {
    async fn run(&self, context: ToolRunContext, _arguments: Value) -> ToolHandlerResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        context.cancellation.cancelled().await;
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: "too late".into(),
            }],
            false,
            Value::Null,
        ))
    }
}

struct Approval(Option<bool>);

#[async_trait]
impl ToolApproval for Approval {
    async fn approve(
        &self,
        _context: &ToolRunContext,
        _schema: &ToolSchema,
        _arguments: &Value,
    ) -> ToolApprovalResult {
        Ok(self.0)
    }
}

fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "mode": {"type": "string", "enum": ["fast", "safe"]},
            "values": {
                "type": "array",
                "items": {"oneOf": [{"type": "integer"}, {"type": "null"}]}
            }
        },
        "required": ["mode"],
        "additionalProperties": false
    })
}

fn definition(name: &str, calls: Arc<AtomicUsize>) -> ToolDefinition {
    ToolDefinition::new(
        name,
        "Echoes validated arguments",
        parameters(),
        Echo { calls },
    )
}

fn call(context: &ContextHandle, session: &str, id: &str) -> ToolRunContext {
    ToolRunContext {
        session: SessionId::from(session),
        call: ToolCallId::from(id),
        cancellation: context.scope().cancellation(),
    }
}

fn code(output: &ToolOutput) -> &str {
    output.meta["code"]
        .as_str()
        .expect("error output has a stable code")
}

#[test]
fn publishes_only_model_visible_schemas() {
    let runtime = ToolRuntime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let _registration = runtime
        .register(definition("echo", calls))
        .expect("tool registers");

    let schemas = runtime.schemas();
    assert_eq!(schemas.len(), 1);
    let published = serde_json::to_value(&schemas).expect("schemas serialize");
    let fields = published[0].as_object().expect("schema is object");
    assert_eq!(
        fields.keys().collect::<Vec<_>>(),
        vec!["description", "name", "parameters"],
        "schemas exclude private handler and output state"
    );
    assert!(!published.to_string().contains("private"));
}

#[tokio::test]
async fn validates_strict_schema_boundaries_before_handler_dispatch() {
    let runtime = ToolRuntime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let registration = runtime
        .register(definition("echo", Arc::clone(&calls)))
        .expect("strict subset registers");
    let context = ContextHandle::root();

    for arguments in [
        json!({}),
        json!({"mode": "slow"}),
        json!({"mode": "fast", "values": [1.5]}),
        json!({"mode": "fast", "extra": true}),
    ] {
        let output = runtime
            .execute(call(&context, "s", "invalid"), "echo", arguments)
            .await;
        assert!(output.is_error);
        assert_eq!(code(&output), "INVALID_TOOL_ARGUMENTS");
    }
    let output = runtime
        .execute(
            call(&context, "s", "valid"),
            "echo",
            json!({"mode": "safe", "values": [1, null]}),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    assert_eq!(
        runtime
            .register(ToolDefinition::new(
                "unsupported",
                "bad",
                json!({"type": "string", "minimum": 0}),
                Echo { calls },
            ))
            .expect_err("unsupported JSON schema keywords reject")
            .code,
        "INVALID_TOOL_SCHEMA"
    );
    drop(registration);
}

#[tokio::test]
async fn access_scopes_only_narrow_and_ask_fails_closed() {
    let runtime = ToolRuntime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let _registration = runtime
        .register(definition("echo", Arc::clone(&calls)))
        .expect("tool registers");
    let context = ContextHandle::root();

    let denied = runtime
        .scoped(ToolRestrictions::new().deny("echo"))
        .expect("deny narrows scope");
    let output = denied
        .execute(call(&context, "s", "deny"), "echo", json!({"mode": "fast"}))
        .await;
    assert_eq!(code(&output), "TOOL_DENIED");
    assert!(denied.schemas().is_empty());

    let asking = runtime
        .scoped(ToolRestrictions::new().ask("echo"))
        .expect("ask narrows scope");
    let output = asking
        .execute(
            call(&context, "s", "ask-none"),
            "echo",
            json!({"mode": "fast"}),
        )
        .await;
    assert_eq!(code(&output), "TOOL_APPROVAL_DENIED");

    runtime.set_approval(Some(Arc::new(Approval(Some(true)))));
    let output = asking
        .execute(
            call(&context, "s", "ask-yes"),
            "echo",
            json!({"mode": "fast"}),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    assert_eq!(
        asking
            .scoped(ToolRestrictions::allow_only(["echo"]))
            .expect_err("child cannot turn ask back into allow")
            .code,
        "TOOL_RESTRICTION_BROADENS_SCOPE"
    );
}

#[tokio::test]
async fn cancellation_prevents_dispatch_and_overrides_late_handler_output() {
    let runtime = ToolRuntime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let _registration = runtime
        .register(definition("echo", Arc::clone(&calls)))
        .expect("tool registers");
    let cancelled_context = ContextHandle::root();
    cancelled_context
        .scope()
        .dispose()
        .await
        .expect("scope cancels cleanly");
    let output = runtime
        .execute(
            call(&cancelled_context, "s", "before"),
            "echo",
            json!({"mode": "fast"}),
        )
        .await;
    assert_eq!(code(&output), "CANCELLED");
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let started = Arc::new(Notify::new());
    let waiting_calls = Arc::new(AtomicUsize::new(0));
    let runtime = ToolRuntime::new();
    let _registration = runtime
        .register(ToolDefinition::new(
            "wait",
            "waits for cancellation",
            parameters(),
            WaitingHandler {
                calls: Arc::clone(&waiting_calls),
                started: Arc::clone(&started),
            },
        ))
        .expect("wait tool registers");
    let context = ContextHandle::root();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let context = call(&context, "s", "during");
        async move {
            runtime
                .execute(context, "wait", json!({"mode": "fast"}))
                .await
        }
    });
    started.notified().await;
    context
        .scope()
        .dispose()
        .await
        .expect("scope cancels running call");
    let output = task.await.expect("tool task settles");
    assert_eq!(code(&output), "CANCELLED");
    assert_eq!(waiting_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn registration_handles_remove_once_and_reject_blank_or_duplicate_names() {
    let runtime = ToolRuntime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    assert_eq!(
        runtime
            .register(definition("  ", Arc::clone(&calls)))
            .expect_err("blank names reject")
            .code,
        "INVALID_TOOL_NAME"
    );
    let registration = runtime
        .register(definition("echo", Arc::clone(&calls)))
        .expect("tool registers");
    assert_eq!(
        runtime
            .register(definition("echo", calls))
            .expect_err("duplicate names reject")
            .code,
        "DUPLICATE_TOOL_NAME"
    );
    assert!(registration.close());
    assert!(!registration.close());
    assert!(runtime.schemas().is_empty());

    let temporary = runtime
        .register(definition("temporary", Arc::new(AtomicUsize::new(0))))
        .expect("tool registers");
    drop(temporary);
    assert!(runtime.schemas().is_empty());
}

#[tokio::test]
async fn observers_reenter_without_locks_and_panics_do_not_escape() {
    let runtime = ToolRuntime::new();
    let reentrant = runtime.clone();
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_by_observer = Arc::clone(&seen);
    let _result_observer = runtime.on_result(move |_: &ToolRunContext, _: &ToolOutput| {
        let _ = reentrant.schemas();
        seen_by_observer.fetch_add(1, Ordering::SeqCst);
    });
    let _panicking_result_observer =
        runtime.on_result(|_: &ToolRunContext, _: &ToolOutput| panic!("observer failure"));
    let _panicking_change_observer = runtime.on_change(|_: &ToolChange| panic!("observer failure"));

    let calls = Arc::new(AtomicUsize::new(0));
    let _registration = runtime
        .register(definition("echo", calls))
        .expect("observer panic does not prevent registration");
    let context = ContextHandle::root();
    let output = runtime
        .execute(
            call(&context, "s", "observers"),
            "echo",
            json!({"mode": "fast"}),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(seen.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn result_observers_receive_correlated_owned_output() {
    let runtime = ToolRuntime::new();
    let observed = Arc::new(Mutex::new(None));
    let observed_by_callback = Arc::clone(&observed);
    let _observer = runtime.on_result(move |context: &ToolRunContext, output: &ToolOutput| {
        *observed_by_callback
            .lock()
            .expect("observer record lock is available") = Some((
            context.session.clone(),
            context.call.clone(),
            output.clone(),
        ));
    });
    let _registration = runtime
        .register(definition("echo", Arc::new(AtomicUsize::new(0))))
        .expect("tool registers");
    let context = ContextHandle::root();
    let output = runtime
        .execute(
            call(&context, "session-42", "tool-call-9"),
            "echo",
            json!({"mode": "fast"}),
        )
        .await;
    let (session, call_id, observed_output) = observed
        .lock()
        .expect("observer record lock is available")
        .clone()
        .expect("one settled output is observed");
    assert_eq!(session.as_str(), "session-42");
    assert_eq!(call_id.as_str(), "tool-call-9");
    assert_eq!(observed_output, output);
    assert_eq!(
        output.into_content_block(ToolCallId::from("tool-call-9")),
        ContentBlock::ToolResult {
            tool_call_id: ToolCallId::from("tool-call-9"),
            content: vec![ContentBlock::Text {
                text: json!({"mode": "fast"}).to_string(),
            }],
            is_error: Some(false),
        }
    );
}

#[test]
fn context_handle_publishes_the_thread_safe_tools_service() {
    let context = ContextHandle::root();
    let runtime = ToolRuntime::new();
    let provider = runtime.publish(&context).expect("tools service publishes");
    assert!(provider.is_current());
    let resolved = context
        .get::<ToolRuntime>(&tools_service_key())
        .expect("typed tools lookup succeeds")
        .expect("published tools runtime is visible");
    assert!(resolved
        .with(|tools| tools.schemas().is_empty())
        .expect("current runtime is callable"));
}

#[test]
fn atomic_replacement_never_exposes_partial_tools_and_rolls_back_validation_failures() {
    let runtime = ToolRuntime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let old = vec![
        runtime
            .register(definition("old_a", Arc::clone(&calls)))
            .unwrap(),
        runtime
            .register(definition("old_b", Arc::clone(&calls)))
            .unwrap(),
    ];
    let snapshots = Arc::new(ParkingMutex::new(Vec::<Vec<String>>::new()));
    let observed_runtime = runtime.clone();
    let observed_snapshots = Arc::clone(&snapshots);
    let _observer = runtime.on_change(move |_: &ToolChange| {
        observed_snapshots.lock().push(
            observed_runtime
                .schemas()
                .into_iter()
                .map(|schema| schema.name)
                .collect(),
        );
    });
    snapshots.lock().clear();

    let replacement = runtime
        .replace(
            &old,
            vec![
                definition("new_a", Arc::clone(&calls)),
                definition("new_b", Arc::clone(&calls)),
            ],
        )
        .unwrap();
    assert_eq!(
        runtime
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>(),
        vec!["new_a", "new_b"]
    );
    assert!(snapshots
        .lock()
        .iter()
        .all(|snapshot| snapshot == &vec![String::from("new_a"), String::from("new_b")]));

    let error = runtime.replace(
        &replacement,
        vec![ToolDefinition::new(
            "invalid",
            "invalid schema",
            json!({"type": "object"}),
            Echo { calls },
        )],
    );
    assert_eq!(error.unwrap_err().code, "INVALID_TOOL_SCHEMA");
    assert_eq!(
        runtime
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>(),
        vec!["new_a", "new_b"],
        "a failed replacement leaves the complete prior set intact"
    );
}

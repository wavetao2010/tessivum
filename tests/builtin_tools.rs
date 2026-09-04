use std::{
    fs,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{process::Command, sync::Arc, time::Duration};

use serde_json::json;
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use tessivum::workspace::{SessionResourceResolver, WorkspaceRegistry};
use tessivum::{
    builtin_tools::{BuiltinTools, BuiltinToolsConfig, DEFAULT_MAX_OUTPUT_BYTES, MAX_OUTPUT_BYTES},
    tools::{ToolRunContext, ToolRuntime},
    ContentBlock, SessionId, ToolCallId,
};
use tessivum_core::ContextHandle;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("tessivum-builtin-tools-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("temporary directory creates");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn context(root: &ContextHandle, call: &str) -> ToolRunContext {
    ToolRunContext {
        session: SessionId::from("builtin-tools"),
        call: ToolCallId::from(call),
        cancellation: root.scope().cancellation(),
    }
}

#[cfg(unix)]
fn context_for(root: &ContextHandle, session: &str, call: &str) -> ToolRunContext {
    ToolRunContext {
        session: SessionId::from(session),
        call: ToolCallId::from(call),
        cancellation: root.scope().cancellation(),
    }
}

#[cfg(unix)]
async fn assert_reaped(pid: &str) {
    for _ in 0..100 {
        let status = Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .status()
            .expect("kill command starts");
        if !status.success() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("persistent shell process {pid} must be reaped");
}

fn text(output: &tessivum::tools::ToolOutput) -> &str {
    match output.content.as_slice() {
        [ContentBlock::Text { text }] => text,
        _ => panic!("builtins return one text block"),
    }
}

#[cfg(unix)]
fn code(output: &tessivum::tools::ToolOutput) -> &str {
    output.meta["code"]
        .as_str()
        .expect("error output has a stable code")
}

#[cfg(unix)]
fn bash_config(cwd: &Path) -> BuiltinToolsConfig {
    BuiltinToolsConfig {
        enable_bash: true,
        cwd: cwd.to_path_buf(),
        resolver: None,
        max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
    }
}

#[tokio::test]
async fn echo_is_model_visible_and_returns_the_exact_text() {
    let runtime = ToolRuntime::new();
    let builtins =
        BuiltinTools::new(&runtime, BuiltinToolsConfig::default()).expect("builtins register");
    let root = ContextHandle::root();

    let schemas = runtime.schemas();
    assert_eq!(schemas.len(), 2);
    let echo = schemas
        .iter()
        .find(|schema| schema.name == "echo")
        .expect("echo schema");
    assert_eq!(echo.parameters["required"], json!(["text"]));
    let output = runtime
        .execute(
            context(&root, "echo"),
            "echo",
            json!({"text": "exact\ntext"}),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(text(&output), "exact\ntext");

    drop(builtins);
    assert!(runtime.schemas().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn bash_is_absent_by_default_and_opt_in_executes_the_fixture_once() {
    let runtime = ToolRuntime::new();
    let default_builtins =
        BuiltinTools::new(&runtime, BuiltinToolsConfig::default()).expect("builtins register");
    assert_eq!(
        runtime
            .schemas()
            .iter()
            .map(|schema| schema.name.as_str())
            .collect::<Vec<_>>(),
        ["echo", "read"]
    );
    drop(default_builtins);

    let directory = TempDir::new();
    let builtins =
        BuiltinTools::new(&runtime, bash_config(directory.path())).expect("bash registers on Unix");
    let root = ContextHandle::root();
    let output = runtime
        .execute(
            context(&root, "fixture"),
            "bash",
            json!({"command": "printf CLI_TOOL_ROUND_TRIP"}),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(text(&output), "CLI_TOOL_ROUND_TRIP");
    assert_eq!(output.meta["truncated"], Value::Bool(false));

    drop(builtins);
    assert!(runtime.schemas().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn bash_uses_configured_cwd_and_captures_stdout_stderr_and_nonzero_status() {
    let directory = TempDir::new();
    let runtime = ToolRuntime::new();
    let _builtins =
        BuiltinTools::new(&runtime, bash_config(directory.path())).expect("bash registers");
    let root = ContextHandle::root();

    let cwd = runtime
        .execute(context(&root, "cwd"), "bash", json!({"command": "pwd"}))
        .await;
    assert!(!cwd.is_error);
    assert_eq!(
        text(&cwd).trim(),
        directory.path().canonicalize().unwrap().to_string_lossy()
    );

    let output = runtime
        .execute(
            context(&root, "output"),
            "bash",
            json!({"command": "printf stdout; printf stderr >&2; exit 7", "description": "capture output"}),
        )
        .await;
    assert!(output.is_error);
    assert_eq!(text(&output), "stdout\n[stderr]\nstderr\n[exit code: 7]");
    assert_eq!(output.meta["exitCode"], json!(7));
    assert_eq!(output.meta["truncated"], Value::Bool(false));
}

#[cfg(unix)]
#[tokio::test]
async fn bash_reports_self_termination_as_a_signal() {
    let directory = TempDir::new();
    let runtime = ToolRuntime::new();
    let _builtins =
        BuiltinTools::new(&runtime, bash_config(directory.path())).expect("bash registers");
    let root = ContextHandle::root();

    let output = runtime
        .execute(
            context(&root, "signal"),
            "bash",
            json!({"command": "kill -TERM $$"}),
        )
        .await;

    assert!(output.is_error);
    assert_eq!(output.meta["signal"], json!("SIGTERM"));
    assert_eq!(output.meta["exitCode"], Value::Null);
}

#[cfg(unix)]
#[tokio::test]
async fn bash_bounds_combined_output_and_marks_truncation() {
    let directory = TempDir::new();
    let runtime = ToolRuntime::new();
    let _builtins = BuiltinTools::new(
        &runtime,
        BuiltinToolsConfig {
            enable_bash: true,
            cwd: directory.path().to_path_buf(),
            resolver: None,
            max_output_bytes: 8,
        },
    )
    .expect("bash registers");
    let root = ContextHandle::root();

    let output = runtime
        .execute(
            context(&root, "bound"),
            "bash",
            json!({"command": "printf 1234567890; printf ABCDEFGHIJ >&2"}),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(output.meta["outputBytes"], json!(8));
    assert_eq!(output.meta["truncated"], Value::Bool(true));
    assert!(text(&output).len() <= 8);
}

#[cfg(unix)]
#[tokio::test]
async fn bash_cancellation_kills_and_reaps_the_child() {
    let directory = TempDir::new();
    let runtime = ToolRuntime::new();
    let _builtins =
        BuiltinTools::new(&runtime, bash_config(directory.path())).expect("bash registers");
    let root = ContextHandle::root();
    let call = context(&root, "cancel");
    let task = tokio::spawn({
        let runtime = runtime.clone();
        async move {
            runtime
                .execute(
                    call,
                    "bash",
                    json!({"command": "echo $$ > shell.pid; exec sleep 30"}),
                )
                .await
        }
    });
    let pid_path = directory.path().join("shell.pid");
    let pid = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(pid) = fs::read_to_string(&pid_path) {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shell writes its pid before cancellation");

    root.scope().dispose().await.expect("scope cancels");
    let output = task.await.expect("tool call settles");
    assert_eq!(code(&output), "CANCELLED");
    let status = Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .status()
        .expect("kill command starts");
    assert!(!status.success(), "cancelled child must be reaped");
}

#[tokio::test]
async fn invalid_configuration_and_arguments_fail_explicitly() {
    let directory = TempDir::new();
    for config in [
        BuiltinToolsConfig {
            enable_bash: false,
            cwd: directory.path().to_path_buf(),
            resolver: None,
            max_output_bytes: 0,
        },
        BuiltinToolsConfig {
            enable_bash: false,
            cwd: directory.path().to_path_buf(),
            resolver: None,
            max_output_bytes: MAX_OUTPUT_BYTES + 1,
        },
        BuiltinToolsConfig {
            enable_bash: false,
            cwd: directory.path().join("missing"),
            resolver: None,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        },
    ] {
        let error = BuiltinTools::new(&ToolRuntime::new(), config)
            .expect_err("invalid configuration rejects");
        assert_eq!(error.code, "INVALID_BUILTIN_TOOLS_CONFIG");
    }

    #[cfg(unix)]
    {
        let runtime = ToolRuntime::new();
        let _builtins =
            BuiltinTools::new(&runtime, bash_config(directory.path())).expect("bash registers");
        let root = ContextHandle::root();
        for (name, arguments) in [
            ("echo", json!({})),
            ("echo", json!({"text": 1})),
            ("bash", json!({"command": "   "})),
            ("bash", json!({"command": "printf ok", "description": 1})),
        ] {
            let output = runtime
                .execute(context(&root, "invalid"), name, arguments)
                .await;

            assert!(output.is_error);
            assert_eq!(code(&output), "INVALID_TOOL_ARGUMENTS");
        }
    }
}
#[cfg(unix)]
#[tokio::test]
async fn bash_uses_only_the_workspace_bound_to_its_session() {
    let root = TempDir::new();
    let first = root.path().join("first");
    let second = root.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let registry = WorkspaceRegistry::open(root.path().join("data"), &first, Vec::new()).unwrap();
    let first_workspace = registry.list().into_iter().next().unwrap().workspace_id;
    let second_workspace = registry
        .create(&second, None)
        .unwrap()
        .workspace
        .workspace_id;
    for (session, workspace) in [
        (SessionId::from("workspace-first"), first_workspace.clone()),
        (
            SessionId::from("workspace-second"),
            second_workspace.clone(),
        ),
    ] {
        registry.recognize_session(&session).unwrap();
        registry.attach_session(&workspace, &session, None).unwrap();
    }
    let runtime = ToolRuntime::new();
    let _builtins = BuiltinTools::new(
        &runtime,
        BuiltinToolsConfig {
            enable_bash: true,
            cwd: root.path().to_path_buf(),
            resolver: Some(Arc::new(SessionResourceResolver::new(registry))),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        },
    )
    .unwrap();
    let context_root = ContextHandle::root();
    let run = |session: &str, call: &str| ToolRunContext {
        session: SessionId::from(session),
        call: ToolCallId::from(call),
        cancellation: context_root.scope().cancellation(),
    };

    let first_output = runtime
        .execute(
            run("workspace-first", "first"),
            "bash",
            json!({"command": "pwd"}),
        )
        .await;
    assert_eq!(
        text(&first_output).trim(),
        first.canonicalize().unwrap().to_string_lossy()
    );
    let second_output = runtime
        .execute(
            run("workspace-second", "second"),
            "bash",
            json!({"command": "pwd"}),
        )
        .await;
    assert_eq!(
        text(&second_output).trim(),
        second.canonicalize().unwrap().to_string_lossy()
    );
    let traversal = runtime
        .execute(
            run("workspace-first", "traversal"),
            "bash",
            json!({"command": "pwd", "cwd": "/"}),
        )
        .await;
    assert!(traversal.is_error);
    assert_eq!(code(&traversal), "INVALID_TOOL_ARGUMENTS");

    fs::remove_dir(&second).unwrap();
    let deleted = runtime
        .execute(
            run("workspace-second", "deleted"),
            "bash",
            json!({"command": "pwd"}),
        )
        .await;
    assert!(deleted.is_error);
    assert_eq!(code(&deleted), "STALE_WORKSPACE_LEASE");
}

#[cfg(unix)]
#[tokio::test]
async fn bash_keeps_the_resolved_directory_fd_across_a_path_swap() {
    let root = TempDir::new();
    let first = root.path().join("first");
    let second = root.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    fs::write(first.join("marker"), "original").unwrap();
    fs::write(second.join("marker"), "replacement").unwrap();
    let registry = WorkspaceRegistry::open(root.path().join("data"), &first, Vec::new()).unwrap();
    let workspace_id = registry.list()[0].workspace_id.clone();
    registry.recognize_session("builtin-tools").unwrap();
    registry
        .attach_session(&workspace_id, "builtin-tools", None)
        .unwrap();
    let runtime = ToolRuntime::new();
    let _builtins = BuiltinTools::new(
        &runtime,
        BuiltinToolsConfig {
            enable_bash: true,
            cwd: root.path().to_path_buf(),
            resolver: Some(Arc::new(SessionResourceResolver::new(registry))),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        },
    )
    .unwrap();
    let context_root = ContextHandle::root();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let context = context(&context_root, "path-swap");
        async move {
            runtime
                .execute(
                    context,
                    "bash",
                    json!({"command": ": > started; sleep 1; cat marker"}),
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if first.join("started").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shell entered descriptor-backed cwd");
    fs::rename(&first, root.path().join("old-first")).unwrap();
    fs::rename(&second, &first).unwrap();
    let output = task.await.unwrap();
    assert!(!output.is_error);
    assert_eq!(text(&output), "original");
}

#[cfg(unix)]
#[tokio::test]
async fn enabled_bash_session_retains_cwd_environment_and_functions() {
    let directory = TempDir::new();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).unwrap();
    let runtime = ToolRuntime::new();
    let builtins = BuiltinTools::new(&runtime, bash_config(directory.path())).unwrap();
    let shells = builtins.persistent_shell_sessions();
    let root = ContextHandle::root();
    let session = SessionId::from("persistent-state");
    shells.enable(session.clone());

    let initialized = runtime
        .execute(
            context_for(&root, session.as_str(), "initialize"),
            "bash",
            json!({"command": "cd nested; export TESSIVUM_PERSISTENT=value; remembered() { printf function; }"}),
        )
        .await;
    assert!(!initialized.is_error);
    let background = runtime
        .execute(
            context_for(&root, session.as_str(), "background"),
            "bash",
            json!({"command": "printf never", "run_in_background": true}),
        )
        .await;
    assert_eq!(code(&background), "PERSISTENT_SHELL_BACKGROUND_UNSUPPORTED");
    let policy_change = runtime
        .execute(
            context_for(&root, session.as_str(), "policy-change"),
            "bash",
            json!({"command": "printf never", "sandbox_permissions": "read-only"}),
        )
        .await;
    assert_eq!(
        code(&policy_change),
        "PERSISTENT_SHELL_SANDBOX_POLICY_MISMATCH"
    );
    let output = runtime
        .execute(
            context_for(&root, session.as_str(), "observe"),
            "bash",
            json!({"command": "printf '%s|' \"$TESSIVUM_PERSISTENT\"; remembered; printf '|'; pwd"}),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(
        text(&output),
        format!(
            "value|function|{}\n",
            nested.canonicalize().unwrap().display()
        )
    );
    shells.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn disabled_bash_session_remains_one_shot() {
    let directory = TempDir::new();
    fs::create_dir(directory.path().join("nested")).unwrap();
    let runtime = ToolRuntime::new();
    let _builtins = BuiltinTools::new(&runtime, bash_config(directory.path())).unwrap();
    let root = ContextHandle::root();

    let initialized = runtime
        .execute(
            context(&root, "initialize"),
            "bash",
            json!({"command": "cd nested; export TESSIVUM_EPHEMERAL=value; remembered() { printf function; }"}),
        )
        .await;
    assert!(!initialized.is_error);
    let output = runtime
        .execute(
            context(&root, "observe"),
            "bash",
            json!({"command": "printf '%s|' \"${TESSIVUM_EPHEMERAL-unset}\"; command -v remembered || printf missing; printf '|'; pwd"}),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(
        text(&output),
        format!(
            "unset|missing|{}\n",
            directory.path().canonicalize().unwrap().display()
        )
    );
}

#[cfg(unix)]
#[tokio::test]
async fn enabled_bash_sessions_are_isolated() {
    let directory = TempDir::new();
    let runtime = ToolRuntime::new();
    let builtins = BuiltinTools::new(&runtime, bash_config(directory.path())).unwrap();
    let shells = builtins.persistent_shell_sessions();
    let root = ContextHandle::root();
    let first = SessionId::from("persistent-first");
    let second = SessionId::from("persistent-second");
    shells.enable(first.clone());
    shells.enable(second.clone());

    assert!(
        !runtime
            .execute(
                context_for(&root, first.as_str(), "first-write"),
                "bash",
                json!({"command": "export TESSIVUM_SESSION_VALUE=first"}),
            )
            .await
            .is_error
    );
    let second_output = runtime
        .execute(
            context_for(&root, second.as_str(), "second-read"),
            "bash",
            json!({"command": "printf '%s' \"${TESSIVUM_SESSION_VALUE-unset}\""}),
        )
        .await;
    assert_eq!(text(&second_output), "unset");
    let first_output = runtime
        .execute(
            context_for(&root, first.as_str(), "first-read"),
            "bash",
            json!({"command": "printf '%s' \"$TESSIVUM_SESSION_VALUE\""}),
        )
        .await;
    assert_eq!(text(&first_output), "first");
    shells.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn persistent_bash_cancellation_disable_and_shutdown_reap_process_groups() {
    let directory = TempDir::new();
    let runtime = ToolRuntime::new();
    let builtins = BuiltinTools::new(&runtime, bash_config(directory.path())).unwrap();
    let shells = builtins.persistent_shell_sessions();
    let root = ContextHandle::root();
    let cancelled = SessionId::from("persistent-cancelled");
    shells.enable(cancelled.clone());
    let child = directory.path().join("cancelled-child.pid");
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let context = context_for(&root, cancelled.as_str(), "cancel");
        let command = format!(
            "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
            child.display()
        );
        async move {
            runtime
                .execute(context, "bash", json!({"command": command}))
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !child.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("persistent shell starts its child");
    root.scope().dispose().await.unwrap();
    let output = task.await.unwrap();
    assert_eq!(code(&output), "CANCELLED");
    assert_reaped(&fs::read_to_string(&child).unwrap()).await;

    let disabled = SessionId::from("persistent-disabled");
    shells.enable(disabled.clone());
    let disabled_pid = directory.path().join("disabled.pid");
    assert!(
        !runtime
            .execute(
                context_for(&ContextHandle::root(), disabled.as_str(), "start-disabled"),
                "bash",
                json!({"command": format!("printf '%s' \"$$\" > '{}'", disabled_pid.display())}),
            )
            .await
            .is_error
    );
    shells.disable(&disabled).await;
    assert_reaped(&fs::read_to_string(&disabled_pid).unwrap()).await;

    let shutdown = SessionId::from("persistent-shutdown");
    shells.enable(shutdown.clone());
    let shutdown_pid = directory.path().join("shutdown.pid");
    assert!(
        !runtime
            .execute(
                context_for(&ContextHandle::root(), shutdown.as_str(), "start-shutdown"),
                "bash",
                json!({"command": format!("printf '%s' \"$$\" > '{}'", shutdown_pid.display())}),
            )
            .await
            .is_error
    );
    shells.shutdown().await;
    assert_reaped(&fs::read_to_string(&shutdown_pid).unwrap()).await;
}

#[cfg(unix)]
#[tokio::test]
async fn stale_workspace_retires_the_enabled_bash_shell() {
    let root = TempDir::new();
    let first = root.path().join("first");
    let replacement = root.path().join("replacement");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&replacement).unwrap();
    let registry = WorkspaceRegistry::open(root.path().join("data"), &first, Vec::new()).unwrap();
    let workspace = registry.list()[0].workspace_id.clone();
    let session = SessionId::from("persistent-stale");
    registry.recognize_session(&session).unwrap();
    registry.attach_session(&workspace, &session, None).unwrap();
    let runtime = ToolRuntime::new();
    let builtins = BuiltinTools::new(
        &runtime,
        BuiltinToolsConfig {
            enable_bash: true,
            cwd: root.path().to_path_buf(),
            resolver: Some(Arc::new(SessionResourceResolver::new(registry))),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        },
    )
    .unwrap();
    let shells = builtins.persistent_shell_sessions();
    shells.enable(session.clone());
    let context_root = ContextHandle::root();
    let pid_path = first.join("shell.pid");
    assert!(
        !runtime
            .execute(
                context_for(&context_root, session.as_str(), "start"),
                "bash",
                json!({"command": "printf '%s' \"$$\" > shell.pid"}),
            )
            .await
            .is_error
    );
    let pid = fs::read_to_string(&pid_path).unwrap();
    let old = root.path().join("old");
    fs::rename(&first, &old).unwrap();
    fs::rename(&replacement, &first).unwrap();
    let stale = runtime
        .execute(
            context_for(&context_root, session.as_str(), "stale"),
            "bash",
            json!({"command": "printf should-not-run"}),
        )
        .await;
    assert_eq!(code(&stale), "STALE_WORKSPACE_LEASE");
    assert_reaped(&pid).await;
    shells.shutdown().await;
}

#[cfg(not(unix))]
#[test]
fn bash_registration_is_explicitly_unsupported_off_unix() {
    let error = BuiltinTools::new(
        &ToolRuntime::new(),
        BuiltinToolsConfig {
            enable_bash: true,
            ..BuiltinToolsConfig::default()
        },
    )
    .expect_err("bash is unavailable off Unix");
    assert_eq!(error.code, "UNSUPPORTED_BUILTIN_BASH");
}

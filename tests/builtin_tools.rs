use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use serde_json::{json, Value};
use tessivum::{
    builtin_tools::{BuiltinTools, BuiltinToolsConfig, DEFAULT_MAX_OUTPUT_BYTES, MAX_OUTPUT_BYTES},
    tools::{ToolRunContext, ToolRuntime},
    workspace::{SessionResourceResolver, WorkspaceRegistry},
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

fn text(output: &tessivum::tools::ToolOutput) -> &str {
    match output.content.as_slice() {
        [ContentBlock::Text { text }] => text,
        _ => panic!("builtins return one text block"),
    }
}

fn code(output: &tessivum::tools::ToolOutput) -> &str {
    output.meta["code"]
        .as_str()
        .expect("error output has a stable code")
}

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
        BuiltinTools::new(&runtime, BuiltinToolsConfig::default()).expect("echo registers");
    let root = ContextHandle::root();

    let schemas = runtime.schemas();
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "echo");
    assert_eq!(schemas[0].parameters["required"], json!(["text"]));
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

#[tokio::test]
async fn bash_is_absent_by_default_and_opt_in_executes_the_fixture_once() {
    let runtime = ToolRuntime::new();
    let default_builtins =
        BuiltinTools::new(&runtime, BuiltinToolsConfig::default()).expect("echo registers");
    assert_eq!(
        runtime
            .schemas()
            .iter()
            .map(|schema| schema.name.as_str())
            .collect::<Vec<_>>(),
        ["echo"]
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
    assert_eq!(text(&output), "stdoutstderr");
    assert_eq!(output.meta["exitCode"], json!(7));
    assert_eq!(output.meta["truncated"], Value::Bool(false));
}

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

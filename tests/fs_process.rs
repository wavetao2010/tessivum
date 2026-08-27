#![cfg(unix)]

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use tessivum::{
    filesystem::{Filesystem, FsLiteralEdit, FsWriteGuard},
    sandbox::{
        runner_denied, RunnerRules, Sandbox, SandboxApproval, SandboxDenial, SandboxEnforcement,
        SandboxMode, SandboxPlan, SandboxProvider, SandboxReadPolicy, SandboxRequest,
    },
    subprocess::{
        CaptureOutput, PersistentShell, PersistentShellCommand, PersistentShellConfig, ProcessDone,
        ProcessOutput, ProcessStdin, SubprocessRequest, SubprocessRuntime,
    },
    TessivumError,
};

fn root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("tessivum-fs-process-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[tokio::test]
async fn filesystem_follows_symlinks_but_lstat_does_not() {
    let root = root();
    std::fs::write(root.join("real"), "one\r\ntwo\rthree").unwrap();
    std::os::unix::fs::symlink("real", root.join("link")).unwrap();
    let fs = Filesystem::new(&root);
    let link = fs.target("link").unwrap();
    assert_eq!(
        fs.lstat(&link).await.unwrap().kind,
        tessivum::filesystem::FsNodeKind::Symlink
    );
    assert_eq!(fs.read_text(&link, 64).await.unwrap(), "one\ntwo\nthree");
    assert!(fs.contains(root.join("real")).await);
    assert_eq!(
        fs.process_path(&link).await.unwrap(),
        std::fs::canonicalize(root.join("real")).unwrap()
    );
}

#[tokio::test]
async fn filesystem_version_guard_precedes_literal_matching_and_atomic_create_is_observed() {
    let root = root();
    std::fs::write(root.join("file"), "before").unwrap();
    let fs = Filesystem::new(&root);
    let target = fs.target("file").unwrap();
    let old = fs.observe(&target).await.unwrap();
    fs.write_text(&target, "changed", FsWriteGuard::default())
        .await
        .unwrap();
    let error = fs
        .edit_text(
            &target,
            FsLiteralEdit {
                expected: "missing".into(),
                replacement: "new".into(),
                replace_if_version: Some(old.version),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "FS_STALE_VERSION");
    assert_eq!(
        std::fs::read_to_string(root.join("file")).unwrap(),
        "changed"
    );

    let create = FsWriteGuard {
        create_if_absent: true,
        replace_if_version: None,
    };
    let error = fs
        .write_text(&target, "replacement", create)
        .await
        .unwrap_err();
    assert_eq!(error.code, "FS_NOT_OBSERVED");
    assert_eq!(
        std::fs::read_to_string(root.join("file")).unwrap(),
        "changed"
    );
}

#[tokio::test]
async fn filesystem_bounded_reads_and_literal_edits_are_observable() {
    let root = root();
    std::fs::write(root.join("file"), "one one").unwrap();
    let fs = Filesystem::new(&root);
    let target = fs.target("file").unwrap();
    assert_eq!(
        fs.read_bytes(&target, 2).await.unwrap_err().code,
        "FS_TOO_LARGE"
    );
    let error = fs
        .edit_text(&target, FsLiteralEdit::new("one", "two"))
        .await
        .unwrap_err();
    assert_eq!(error.code, "FS_AMBIGUOUS_EDIT");
    let error = fs
        .edit_text(&target, FsLiteralEdit::new("none", "two"))
        .await
        .unwrap_err();
    assert_eq!(error.code, "FS_EDIT_NOT_FOUND");
}

#[tokio::test]
async fn literal_argv_scrubs_dsh_and_retains_bounded_output_spill() {
    let root = root();
    let marker = root.join("marker");
    std::env::set_var("DSH_TEST_SECRET", "must-not-cross");
    let runtime = SubprocessRuntime::new();
    let mut request = SubprocessRequest::new(vec![
        "/bin/sh".into(),
        "-c".into(),
        "test -z \"${DSH_TEST_SECRET+x}\" && printf 'abcdef'".into(),
    ]);
    request.stdin = ProcessStdin::Null;
    request.stdout = ProcessOutput::Capture(CaptureOutput {
        tail_bytes: 3,
        spill_path: Some(root.join("spill")),
    });
    let process = runtime.spawn(request).await.unwrap();
    assert_eq!(process.wait().await.exit_code, Some(0));
    assert_eq!(process.stdout().unwrap().tail, b"def");
    assert_eq!(process.read_stdout(0, 6).await.unwrap().bytes, b"abcdef");

    let request = SubprocessRequest::new(vec![
        "/bin/echo".into(),
        format!("; touch {}", marker.display()),
    ]);
    assert_eq!(
        runtime.spawn(request).await.unwrap().wait().await.exit_code,
        Some(0)
    );
    assert!(!marker.exists());
    runtime.shutdown().await;
    std::env::remove_var("DSH_TEST_SECRET");
}

#[tokio::test]
async fn process_tree_termination_and_first_cause_complete() {
    let runtime = SubprocessRuntime::new();
    let mut request = SubprocessRequest::new(vec![
        "/bin/sh".into(),
        "-c".into(),
        "sleep 30 & wait".into(),
    ]);
    request.terminate_grace = Duration::from_millis(10);
    let process = runtime.spawn(request).await.unwrap();
    let done = process.terminate(Duration::from_millis(10)).await;
    assert_eq!(
        done.termination,
        Some(tessivum::subprocess::ProcessTermination::Terminated)
    );

    let process = runtime
        .spawn(SubprocessRequest::new(vec![
            "/bin/sh".into(),
            "-c".into(),
            "sleep 30".into(),
        ]))
        .await
        .unwrap();
    let first = process.clone();
    let second = process.clone();
    let (timeout, abort) = tokio::join!(
        async move { first.wait_timeout(Duration::from_millis(1)).await },
        async move { second.abort(Duration::from_millis(1)).await },
    );
    assert_eq!(timeout.termination, abort.termination);
    assert!(matches!(
        timeout.termination,
        Some(
            tessivum::subprocess::ProcessTermination::TimedOut
                | tessivum::subprocess::ProcessTermination::Aborted
        )
    ));
    runtime.shutdown().await;
}

#[derive(Clone)]
struct Provider(SandboxPlan);

impl SandboxProvider for Provider {
    fn confine(
        &self,
        _: &tessivum::sandbox::EffectiveSandboxRequest,
        _: &[String],
    ) -> Result<SandboxPlan, TessivumError> {
        Ok(self.0.clone())
    }
}

fn sandbox_request(workspace: &Path) -> SandboxRequest {
    SandboxRequest {
        mode: SandboxMode::WorkspaceWrite,
        workspace: workspace.to_path_buf(),
        read_policy: SandboxReadPolicy::Allow,
        read_roots: vec![workspace.to_path_buf()],
        write_roots: vec![workspace.to_path_buf()],
        approval: Some(SandboxApproval {
            mode: Some(SandboxMode::WorkspaceWrite),
            read_policy: Some(SandboxReadPolicy::Allow),
        }),
    }
}

#[tokio::test]
async fn sandbox_fails_closed_and_runner_denial_needs_fatal_stderr() {
    let workspace = root();
    let request = sandbox_request(&workspace);
    let runtime = SubprocessRuntime::new();
    let none = Sandbox::default();
    let process = SubprocessRequest::new(vec!["/bin/true".into()]);
    assert_eq!(
        none.spawn(&runtime, &request, process)
            .await
            .unwrap_err()
            .code,
        "SANDBOX_UNAVAILABLE"
    );

    let partial = Sandbox::new(Some(Arc::new(Provider(SandboxPlan {
        argv: vec!["/bin/true".into()],
        enforcement: SandboxEnforcement::Partial,
        denial: None,
        runner_rules: RunnerRules::default(),
    }))));
    assert_eq!(
        partial
            .spawn(
                &runtime,
                &request,
                SubprocessRequest::new(vec!["/bin/false".into()])
            )
            .await
            .unwrap_err()
            .code,
        "SANDBOX_UNAVAILABLE"
    );

    let rules = RunnerRules {
        denial_exit_codes: Some(BTreeSet::from([42])),
        informational_stderr: BTreeSet::from(["informational".into()]),
    };
    assert!(!runner_denied(
        &rules,
        &ProcessDone {
            exit_code: Some(42),
            signal: None,
            termination: None
        },
        b"informational\n"
    ));
    assert!(runner_denied(
        &rules,
        &ProcessDone {
            exit_code: Some(42),
            signal: None,
            termination: None
        },
        b"fatal\n"
    ));

    let sandbox = Sandbox::new(Some(Arc::new(Provider(SandboxPlan {
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo fatal >&2; exit 42".into(),
        ],
        enforcement: SandboxEnforcement::Full,
        denial: Some(SandboxDenial {
            code: "SANDBOX_DENIED".into(),
            message: "provider denied command".into(),
        }),
        runner_rules: rules,
    }))));
    assert_eq!(
        sandbox
            .spawn(
                &runtime,
                &request,
                SubprocessRequest::new(vec!["/bin/true".into()])
            )
            .await
            .unwrap()
            .wait()
            .await
            .unwrap_err()
            .code,
        "SANDBOX_DENIED"
    );
    runtime.shutdown().await;
}

async fn persistent_shell(workspace: &Path) -> PersistentShell {
    PersistentShell::start(PersistentShellConfig::new(workspace), || Ok(()))
        .await
        .unwrap()
}

#[tokio::test]
async fn persistent_shell_preserves_cwd_environment_and_functions() {
    let workspace = root();
    std::fs::create_dir(workspace.join("nested")).unwrap();
    let shell = persistent_shell(&workspace).await;
    shell
        .run(PersistentShellCommand::new(
            "cd nested; export TESSIVUM_PERSISTENT_VALUE=retained; remembered() { printf function; }",
        ))
        .await
        .unwrap();
    let result = shell
        .run(PersistentShellCommand::new(
            "printf '%s|' \"$TESSIVUM_PERSISTENT_VALUE\"; remembered; printf '|'; pwd",
        ))
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(result.stdout.tail).unwrap(),
        format!("retained|function|{}\n", workspace.join("nested").display())
    );
    shell.dispose().await;
}

#[tokio::test]
async fn persistent_shells_are_isolated() {
    let first_root = root();
    let second_root = root();
    let first = persistent_shell(&first_root).await;
    let second = persistent_shell(&second_root).await;
    first
        .run(PersistentShellCommand::new("export TESSIVUM_SHELL_ISOLATED=first"))
        .await
        .unwrap();
    assert_eq!(
        second
            .run(PersistentShellCommand::new("printf '%s' \"${TESSIVUM_SHELL_ISOLATED-unset}\""))
            .await
            .unwrap()
            .stdout
            .tail,
        b"unset"
    );
    second
        .run(PersistentShellCommand::new("export TESSIVUM_SHELL_ISOLATED=second"))
        .await
        .unwrap();
    assert_eq!(
        first
            .run(PersistentShellCommand::new("printf '%s' \"$TESSIVUM_SHELL_ISOLATED\""))
            .await
            .unwrap()
            .stdout
            .tail,
        b"first"
    );
    first.dispose().await;
    second.dispose().await;
}

#[tokio::test]
async fn persistent_shell_serializes_concurrent_commands() {
    let workspace = root();
    let shell = persistent_shell(&workspace).await;
    let first_shell = shell.clone();
    let first = tokio::spawn(async move {
        first_shell
            .run(PersistentShellCommand::new(
                "printf first-start; sleep 1; printf first-end",
            ))
            .await
            .unwrap()
    });
    tokio::task::yield_now().await;
    let second_shell = shell.clone();
    let second = tokio::spawn(async move {
        second_shell
            .run(PersistentShellCommand::new("printf second"))
            .await
            .unwrap()
    });
    assert_eq!(first.await.unwrap().stdout.tail, b"first-startfirst-end");
    assert_eq!(second.await.unwrap().stdout.tail, b"second");
    shell.dispose().await;
}

#[tokio::test]
async fn persistent_shell_bounds_each_output_stream_and_ignores_foreign_frames() {
    let workspace = root();
    let mut config = PersistentShellConfig::new(&workspace);
    config.max_output_bytes = 3;
    let shell = PersistentShell::start(config, || Ok(())).await.unwrap();
    let result = shell
        .run(PersistentShellCommand::new("printf abcdef; printf ghijkl >&2"))
        .await
        .unwrap();
    assert_eq!(result.stdout.total_bytes, 6);
    assert_eq!(result.stdout.tail, b"def");
    assert_eq!(result.stderr.total_bytes, 6);
    assert_eq!(result.stderr.tail, b"jkl");
    shell.dispose().await;

    let shell = persistent_shell(&workspace).await;
    let result = shell
        .run(PersistentShellCommand::new(
            "printf '\\036TESSIVUM-SHELL:00000000000000000000000000000000:O:0\\037\\nforeign'",
        ))
        .await
        .unwrap();
    assert_eq!(
        result.stdout.tail,
        b"\x1eTESSIVUM-SHELL:00000000000000000000000000000000:O:0\x1f\nforeign"
    );
    shell.dispose().await;
}

#[tokio::test]
async fn persistent_shell_cancellation_kills_descendants() {
    let workspace = root();
    let child_pid = workspace.join("child.pid");
    let shell = persistent_shell(&workspace).await;
    let cancellation = tessivum_core::CancellationToken::new();
    let command = PersistentShellCommand::new(format!(
        "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
        child_pid.display()
    ))
    .cancelled_by(cancellation.clone());
    let run = tokio::spawn({
        let shell = shell.clone();
        async move { shell.run(command).await.unwrap_err() }
    });
    for _ in 0..100 {
        if child_pid.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(child_pid.exists());
    let pid: i32 = std::fs::read_to_string(&child_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    cancellation.cancel();
    assert_eq!(run.await.unwrap().code, "PERSISTENT_SHELL_CANCELLED");
    for _ in 0..100 {
        if unsafe { libc::kill(pid, 0) } == -1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    shell.dispose().await;
}

#[tokio::test]
async fn persistent_shell_rejects_stale_lease_before_command() {
    let workspace = root();
    let valid = Arc::new(AtomicBool::new(true));
    let validator = {
        let valid = Arc::clone(&valid);
        move || {
            if valid.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(TessivumError::new(
                    "STALE_WORKSPACE_LEASE",
                    "workspace lease is stale",
                    "workspace",
                    serde_json::json!({}),
                ))
            }
        }
    };
    let shell = PersistentShell::start(PersistentShellConfig::new(&workspace), validator)
        .await
        .unwrap();
    valid.store(false, Ordering::Release);
    assert_eq!(
        shell
            .run(PersistentShellCommand::new("printf should-not-run"))
            .await
            .unwrap_err()
            .code,
        "STALE_WORKSPACE_LEASE"
    );
    shell.dispose().await;
}
#[tokio::test]
async fn persistent_shell_never_waits_for_closed_stdin() {
    let workspace = root();
    let shell = persistent_shell(&workspace).await;
    let closed = tokio::time::timeout(
        Duration::from_secs(2),
        shell.run(PersistentShellCommand::new("exec <&-")),
    )
    .await
    .unwrap();
    if closed.is_ok() {
        assert_eq!(
            shell
                .run(PersistentShellCommand::new("printf unavailable"))
                .await
                .unwrap_err()
                .code,
            "PERSISTENT_SHELL_CLOSED"
        );
    } else {
        assert_eq!(closed.unwrap_err().code, "PERSISTENT_SHELL_CLOSED");
    }
    shell.dispose().await;
}


#[tokio::test]
async fn persistent_shell_fails_closed_after_exec_and_dispose_is_idempotent() {
    let workspace = root();
    let shell = persistent_shell(&workspace).await;
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        shell.run(PersistentShellCommand::new("exec /bin/true")),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.code, "PERSISTENT_SHELL_CLOSED");
    shell.dispose().await;
    shell.dispose().await;
    assert_eq!(
        shell
            .run(PersistentShellCommand::new("printf unavailable"))
            .await
            .unwrap_err()
            .code,
        "PERSISTENT_SHELL_DISPOSED"
    );
}

#![cfg(windows)]

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use tessivum::{
    subprocess::{
        CaptureOutput, PersistentShell, PersistentShellCommand, PersistentShellConfig,
        ProcessOutput, SubprocessRequest, SubprocessRuntime,
    },
    TessivumError,
};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("tessivum-windows-process-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ps_literal(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

async fn persistent_shell(root: &Path) -> PersistentShell {
    PersistentShell::start(PersistentShellConfig::new(root), || Ok(()))
        .await
        .unwrap()
}

async fn read_pid(path: &Path) -> u32 {
    for _ in 0..1000 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("child pid was not written to {}", path.display());
}

fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, WaitForSingleObject},
    };

    const SYNCHRONIZE: u32 = 0x0010_0000;
    const WAIT_TIMEOUT: u32 = 0x0000_0102;
    let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return false;
    }
    let alive = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
    unsafe { CloseHandle(process) };
    alive
}

async fn assert_process_exits(pid: u32) {
    for _ in 0..200 {
        if !process_is_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process {pid} survived managed-tree termination");
}

fn descendant_script(pid_file: &Path) -> String {
    format!(
        "$child = Start-Process -FilePath $env:ComSpec -ArgumentList '/d','/c','ping -n 31 127.0.0.1 >nul' -PassThru; \
         Set-Content -LiteralPath {} -Value $child.Id -NoNewline; Wait-Process -Id $child.Id",
        ps_literal(pid_file)
    )
}

#[tokio::test]
async fn persistent_powershell_preserves_state_cwd_utf8_and_exit_semantics() {
    let root = TempRoot::new();
    std::fs::create_dir(root.0.join("nested")).unwrap();
    let shell = persistent_shell(&root.0).await;

    shell
        .run(PersistentShellCommand::new(
            "$TessivumValue = '保留'; function Remembered { '函数' }; Set-Location nested",
        ))
        .await
        .unwrap();
    let retained = shell
        .run(PersistentShellCommand::new(
            "[Console]::Out.Write(\"$TessivumValue|$(Remembered)|$((Get-Location).Path)\"); [Console]::Error.Write('错误')",
        ))
        .await
        .unwrap();
    let stdout = String::from_utf8(retained.stdout.tail).unwrap();
    assert!(stdout.starts_with("保留|函数|"), "{stdout:?}");
    assert!(stdout.ends_with("nested"), "{stdout:?}");
    assert_eq!(String::from_utf8(retained.stderr.tail).unwrap(), "错误");

    let parse = shell
        .run(PersistentShellCommand::new("if ("))
        .await
        .unwrap();
    assert_eq!(parse.exit_code, 1);
    assert!(!parse.stderr.tail.is_empty());

    let runtime = shell
        .run(PersistentShellCommand::new("throw 'runtime-boom'"))
        .await
        .unwrap();
    assert_eq!(runtime.exit_code, 1);
    assert!(String::from_utf8(runtime.stderr.tail)
        .unwrap()
        .contains("runtime-boom"));

    let native = shell
        .run(PersistentShellCommand::new(
            "& $env:ComSpec '/d' '/c' 'exit 23'",
        ))
        .await
        .unwrap();
    assert_eq!(native.exit_code, 23);

    let powershell_failure = shell
        .run(PersistentShellCommand::new("Write-Error 'failed'"))
        .await
        .unwrap();
    assert_eq!(powershell_failure.exit_code, 1);
    shell.dispose().await;
}

#[tokio::test]
async fn persistent_powershell_serializes_commands_and_isolates_siblings() {
    let first_root = TempRoot::new();
    let second_root = TempRoot::new();
    let gate = first_root.0.join("started");
    let first = persistent_shell(&first_root.0).await;
    let second = persistent_shell(&second_root.0).await;

    first
        .run(PersistentShellCommand::new("$SiblingValue = 'first'"))
        .await
        .unwrap();
    let first_command = tokio::spawn({
        let shell = first.clone();
        let gate = ps_literal(&gate);
        async move {
            shell
                .run(PersistentShellCommand::new(format!(
                    "$SerialValue = 1; Set-Content -LiteralPath {gate} -Value ready; Start-Sleep -Milliseconds 200; $SerialValue = 2"
                )))
                .await
                .unwrap()
        }
    });
    for _ in 0..200 {
        if gate.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gate.exists());
    let second_command = tokio::spawn({
        let shell = first.clone();
        async move {
            shell
                .run(PersistentShellCommand::new(
                    "[Console]::Out.Write($SerialValue)",
                ))
                .await
                .unwrap()
        }
    });
    first_command.await.unwrap();
    assert_eq!(second_command.await.unwrap().stdout.tail, b"2");

    assert_eq!(
        second
            .run(PersistentShellCommand::new(
                "if (Test-Path variable:SiblingValue) { [Console]::Out.Write($SiblingValue) } else { [Console]::Out.Write('unset') }",
            ))
            .await
            .unwrap()
            .stdout
            .tail,
        b"unset"
    );
    first.dispose().await;
    second.dispose().await;
}

#[tokio::test]
async fn persistent_powershell_bounds_streams_and_treats_stale_nonce_as_output() {
    let root = TempRoot::new();
    let mut config = PersistentShellConfig::new(&root.0);
    config.max_output_bytes = 4;
    let shell = PersistentShell::start(config, || Ok(())).await.unwrap();
    let bounded = shell
        .run(PersistentShellCommand::new(
            "[Console]::Out.Write('abcdef'); [Console]::Error.Write('ghijkl')",
        ))
        .await
        .unwrap();
    assert_eq!(bounded.stdout.total_bytes, 6);
    assert_eq!(bounded.stdout.tail, b"cdef");
    assert_eq!(bounded.stderr.total_bytes, 6);
    assert_eq!(bounded.stderr.tail, b"ijkl");
    shell.dispose().await;

    let shell = persistent_shell(&root.0).await;
    let stale = shell
        .run(PersistentShellCommand::new(
            "$old = [char]0x1e + 'TESSIVUM-SHELL:00000000000000000000000000000000:O:0' + [char]0x1f + \"`n\"; [Console]::Out.Write($old + 'after')",
        ))
        .await
        .unwrap();
    assert_eq!(
        stale.stdout.tail,
        b"\x1eTESSIVUM-SHELL:00000000000000000000000000000000:O:0\x1f\nafter"
    );
    shell.dispose().await;
}

#[tokio::test]
async fn persistent_powershell_rejects_an_invalidated_lease_before_execution() {
    let root = TempRoot::new();
    let marker = root.0.join("must-not-exist");
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
    let shell = PersistentShell::start(PersistentShellConfig::new(&root.0), validator)
        .await
        .unwrap();
    valid.store(false, Ordering::Release);
    let error = shell
        .run(PersistentShellCommand::new(format!(
            "Set-Content -LiteralPath {} -Value escaped",
            ps_literal(&marker)
        )))
        .await
        .unwrap_err();
    assert_eq!(error.code, "STALE_WORKSPACE_LEASE");
    assert!(!marker.exists());
    shell.dispose().await;
}

#[tokio::test]
async fn cancellation_timeout_and_dispose_kill_powershell_descendants() {
    let root = TempRoot::new();

    let cancellation_pid = root.0.join("cancel.pid");
    let shell = persistent_shell(&root.0).await;
    let cancellation = tessivum_core::Scope::root().cancellation();
    let cancelled = tokio::spawn({
        let shell = shell.clone();
        let cancellation = cancellation.clone();
        let script = descendant_script(&cancellation_pid);
        async move {
            shell
                .run(PersistentShellCommand::new(script).cancelled_by(cancellation))
                .await
                .unwrap_err()
        }
    });
    let pid = read_pid(&cancellation_pid).await;
    cancellation.cancel();
    assert_eq!(cancelled.await.unwrap().code, "PERSISTENT_SHELL_CANCELLED");
    assert_process_exits(pid).await;
    shell.dispose().await;

    let timeout_pid = root.0.join("timeout.pid");
    let shell = persistent_shell(&root.0).await;
    let mut command = PersistentShellCommand::new(descendant_script(&timeout_pid));
    command.timeout = Duration::from_secs(2);
    let timed_out = tokio::spawn({
        let shell = shell.clone();
        async move { shell.run(command).await.unwrap_err() }
    });
    let pid = read_pid(&timeout_pid).await;
    assert_eq!(timed_out.await.unwrap().code, "PERSISTENT_SHELL_TIMEOUT");
    assert_process_exits(pid).await;
    shell.dispose().await;

    let dispose_pid = root.0.join("dispose.pid");
    let shell = persistent_shell(&root.0).await;
    let running = tokio::spawn({
        let shell = shell.clone();
        let script = descendant_script(&dispose_pid);
        async move { shell.run(PersistentShellCommand::new(script)).await }
    });
    let pid = read_pid(&dispose_pid).await;
    shell.dispose().await;
    assert_eq!(
        running.await.unwrap().unwrap_err().code,
        "PERSISTENT_SHELL_DISPOSED"
    );
    assert_process_exits(pid).await;
}

#[tokio::test]
async fn dropping_the_last_persistent_shell_owner_kills_background_descendants() {
    let root = TempRoot::new();
    let pid_file = root.0.join("drop.pid");
    let shell = persistent_shell(&root.0).await;
    shell
        .run(PersistentShellCommand::new(format!(
            "$child = Start-Process -FilePath $env:ComSpec -ArgumentList '/d','/c','ping -n 31 127.0.0.1 >nul' -PassThru; Set-Content -LiteralPath {} -Value $child.Id -NoNewline",
            ps_literal(&pid_file)
        )))
        .await
        .unwrap();
    let pid = read_pid(&pid_file).await;
    assert!(process_is_alive(pid));
    drop(shell);
    assert_process_exits(pid).await;
}

#[tokio::test]
async fn immediate_parent_exit_cannot_orphan_descendants_or_hold_capture_pipes() {
    let root = TempRoot::new();
    let pid_file = root.0.join("orphan.pid");
    let script = format!(
        "$child = Start-Process -FilePath $env:ComSpec -ArgumentList '/d','/c','ping -n 31 127.0.0.1' -PassThru; \
         Set-Content -LiteralPath {} -Value $child.Id -NoNewline; exit 0",
        ps_literal(&pid_file)
    );
    let mut request = SubprocessRequest::new(vec![
        "powershell.exe".into(),
        "-NoLogo".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        script,
    ]);
    request.cwd = Some(root.0.clone());
    request.stdout = ProcessOutput::Capture(CaptureOutput {
        tail_bytes: 1024,
        spill_path: None,
    });
    let runtime = SubprocessRuntime::new();
    let process = runtime.spawn(request).await.unwrap();
    let pid = read_pid(&pid_file).await;
    let done = tokio::time::timeout(Duration::from_secs(5), process.wait())
        .await
        .expect("descendant inherited pipes must close after direct parent exit");
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(done.termination, None);
    assert_process_exits(pid).await;
    runtime.shutdown().await;
}

#[tokio::test]
async fn dropping_the_subprocess_runtime_terminates_owned_trees() {
    let root = TempRoot::new();
    let pid_file = root.0.join("runtime-drop.pid");
    let mut request = SubprocessRequest::new(vec![
        "powershell.exe".into(),
        "-NoLogo".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        descendant_script(&pid_file),
    ]);
    request.cwd = Some(root.0.clone());
    let runtime = SubprocessRuntime::new();
    let process = runtime.spawn(request).await.unwrap();
    let pid = read_pid(&pid_file).await;
    drop(runtime);
    let done = tokio::time::timeout(Duration::from_secs(5), process.wait())
        .await
        .expect("runtime drop must close the managed tree");
    assert_eq!(
        done.termination,
        Some(tessivum::subprocess::ProcessTermination::Shutdown)
    );
    assert_process_exits(pid).await;
}

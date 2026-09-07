#![cfg(windows)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use tessivum::sandbox::{Sandbox, SandboxApproval, SandboxMode, SandboxReadPolicy, SandboxRequest};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tessivum-windows-sandbox-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request(mode: SandboxMode, workspace: &Path, approved: bool) -> SandboxRequest {
    SandboxRequest {
        mode,
        workspace: workspace.to_path_buf(),
        read_policy: SandboxReadPolicy::Deny,
        read_roots: Vec::new(),
        write_roots: if mode == SandboxMode::WorkspaceWrite {
            vec![workspace.to_path_buf()]
        } else {
            Vec::new()
        },
        approval: approved.then_some(SandboxApproval {
            mode: Some(mode),
            read_policy: None,
        }),
    }
}

fn powershell(script: impl Into<String>) -> Vec<String> {
    vec![
        "powershell.exe".into(),
        "-NoLogo".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        format!(
            "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); [Console]::InputEncoding=[Text.UTF8Encoding]::new($false); {}",
            script.into()
        ),
    ]
}

fn plan(mode: SandboxMode, workspace: &Path, script: impl Into<String>) -> Vec<String> {
    let argv = powershell(script);
    let plan = Sandbox::local()
        .prepare(&request(mode, workspace, true), &argv)
        .unwrap();
    assert_eq!(
        Path::new(&plan.argv[0]).canonicalize().unwrap(),
        Path::new(env!("CARGO_BIN_EXE_tessivum"))
            .canonicalize()
            .unwrap()
    );
    plan.argv
}

fn output(mode: SandboxMode, workspace: &Path, script: impl Into<String>) -> Output {
    let argv = plan(mode, workspace, script);
    Command::new(&argv[0]).args(&argv[1..]).output().unwrap()
}

fn spawn(mode: SandboxMode, workspace: &Path, script: impl Into<String>) -> Child {
    let argv = plan(mode, workspace, script);
    Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn ps(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

fn ps_text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(path.exists(), "timed out waiting for {}", path.display());
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

fn assert_process_exits(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_is_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(!process_is_alive(pid), "descendant process {pid} survived");
}

#[test]
fn write_matrix_private_temp_isolated_and_stale_cleanup_is_safe() {
    let root = TestRoot::new("matrix");
    let first = root.0.join("first");
    let second = root.0.join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();
    let temp_note = first.join("temp-path.txt");

    let mut live = spawn(
        SandboxMode::WorkspaceWrite,
        &first,
        format!(
            "Set-Content -LiteralPath {} -Value $env:TEMP; Set-Content -LiteralPath (Join-Path $env:TEMP 'own.txt') -Value ok; Set-Content -LiteralPath {} -Value ok; Start-Sleep -Seconds 60",
            ps(&temp_note),
            ps(&first.join("allowed.txt")),
        ),
    );
    wait_for(&temp_note);
    let private_temp = PathBuf::from(fs::read_to_string(&temp_note).unwrap().trim());
    assert!(private_temp.exists());

    let user_probe = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap()
        .join(format!("tessivum-denied-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&user_probe).unwrap();
    let denied = |path: &Path| {
        format!(
            "try {{ Set-Content -LiteralPath {} -Value denied; exit 41 }} catch {{}}; ",
            ps(path)
        )
    };
    let probe = format!(
        "{}{}{}Set-Content -LiteralPath {} -Value ok; Set-Content -LiteralPath (Join-Path $env:TEMP 'own.txt') -Value ok; Write-Output '中文输出'; [Console]::Error.WriteLine('中文错误')",
        denied(&first.join("sibling-workspace.txt")),
        denied(&private_temp.join("sibling-temp.txt")),
        denied(&user_probe.join("ordinary.txt")),
        ps(&second.join("allowed.txt")),
    );
    let result = output(SandboxMode::WorkspaceWrite, &second, probe);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "中文输出");
    assert!(String::from_utf8(result.stderr)
        .unwrap()
        .contains("中文错误"));
    assert!(!first.join("sibling-workspace.txt").exists());
    assert!(!private_temp.join("sibling-temp.txt").exists());
    assert!(!user_probe.join("ordinary.txt").exists());
    assert!(private_temp.exists(), "a live run was collected as stale");

    live.kill().unwrap();
    live.wait().unwrap();
    wait_for(&private_temp);
    let cleanup = output(
        SandboxMode::WorkspaceWrite,
        &second,
        "Set-Content -LiteralPath (Join-Path $env:TEMP 'cleanup-probe.txt') -Value ok",
    );
    assert!(
        cleanup.status.success(),
        "{}",
        String::from_utf8_lossy(&cleanup.stderr)
    );
    assert!(
        !private_temp.exists(),
        "next startup did not collect owned stale temp"
    );
    fs::remove_dir_all(user_probe).unwrap();
}

#[test]
fn read_only_denies_workspace_and_private_temp_writes() {
    let root = TestRoot::new("readonly");
    let script = format!(
        "try {{ Set-Content -LiteralPath {} -Value denied; exit 51 }} catch {{}}; try {{ Set-Content -LiteralPath (Join-Path $env:TEMP 'denied.txt') -Value denied; exit 52 }} catch {{}}; Write-Output readonly-ok",
        ps(&root.0.join("denied.txt")),
    );
    let result = output(SandboxMode::ReadOnly, &root.0, script);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).unwrap().trim(),
        "readonly-ok"
    );
    assert!(!root.0.join("denied.txt").exists());
}

#[test]
fn each_launch_carries_only_current_write_root_capabilities() {
    let root = TestRoot::new("current-roots");
    let first = root.0.join("first");
    let second = root.0.join("second");
    fs::create_dir(&first).unwrap();
    fs::create_dir(&second).unwrap();

    let run = |roots: Vec<PathBuf>, script: String| {
        let mut sandbox_request = request(SandboxMode::WorkspaceWrite, &root.0, true);
        sandbox_request.write_roots = roots;
        let argv = powershell(script);
        let plan = Sandbox::local().prepare(&sandbox_request, &argv).unwrap();
        Command::new(&plan.argv[0])
            .args(&plan.argv[1..])
            .output()
            .unwrap()
    };
    let assert_success = |result: &Output| {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };

    let first_result = run(
        vec![first.clone()],
        format!(
            "Set-Content -LiteralPath {} -Value first",
            ps(&first.join("allowed.txt"))
        ),
    );
    assert_success(&first_result);

    let leaked_from_first = first.join("leaked-from-first.txt");
    let second_result = run(
        vec![second.clone()],
        format!(
            "try {{ Set-Content -LiteralPath {} -Value leaked; exit 71 }} catch {{}}; Set-Content -LiteralPath {} -Value second",
            ps(&leaked_from_first),
            ps(&second.join("allowed.txt")),
        ),
    );
    assert_success(&second_result);
    assert!(!leaked_from_first.exists());
    assert!(second.join("allowed.txt").exists());

    let whole_result = run(
        vec![root.0.clone()],
        format!(
            "Set-Content -LiteralPath {} -Value whole",
            ps(&root.0.join("whole-workspace.txt"))
        ),
    );
    assert_success(&whole_result);

    let leaked_from_whole = first.join("leaked-from-whole.txt");
    let leaked_to_workspace = root.0.join("leaked-to-workspace.txt");
    let narrowed_result = run(
        vec![second.clone()],
        format!(
            "try {{ Set-Content -LiteralPath {} -Value leaked; exit 72 }} catch {{}}; try {{ Set-Content -LiteralPath {} -Value leaked; exit 73 }} catch {{}}; Set-Content -LiteralPath {} -Value narrowed",
            ps(&leaked_from_whole),
            ps(&leaked_to_workspace),
            ps(&second.join("narrowed.txt")),
        ),
    );
    assert_success(&narrowed_result);
    assert!(!leaked_from_whole.exists());
    assert!(!leaked_to_workspace.exists());
    assert!(second.join("narrowed.txt").exists());
}

#[test]
fn danger_requires_exact_approval_before_raw_execution() {
    let root = TestRoot::new("danger");
    let target = root.0.join("approved.txt");
    let argv = powershell(format!(
        "Set-Content -LiteralPath {} -Value approved",
        ps(&target)
    ));
    let sandbox = Sandbox::default();
    let denied = sandbox
        .prepare(
            &request(SandboxMode::DangerFullAccess, &root.0, false),
            &argv,
        )
        .unwrap_err();
    assert_eq!(denied.code, "SANDBOX_DENIED");
    assert!(!target.exists());
    let mut wrong_approval = request(SandboxMode::DangerFullAccess, &root.0, false);
    wrong_approval.approval = Some(SandboxApproval {
        mode: Some(SandboxMode::WorkspaceWrite),
        read_policy: None,
    });
    assert_eq!(
        sandbox.prepare(&wrong_approval, &argv).unwrap_err().code,
        "SANDBOX_DENIED"
    );

    let approved = sandbox
        .prepare(
            &request(SandboxMode::DangerFullAccess, &root.0, true),
            &argv,
        )
        .unwrap();
    assert_eq!(approved.argv, argv);
    let result = Command::new(&approved.argv[0])
        .args(&approved.argv[1..])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(fs::read_to_string(target).unwrap().trim(), "approved");
}

#[test]
fn reparse_and_workspace_temp_intersection_refuse_before_target_launch() {
    let root = TestRoot::new("paths");
    let real = root.0.join("real");
    let junction = root.0.join("junction");
    fs::create_dir(&real).unwrap();
    let junction_script = format!(
        "New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
        ps(&junction),
        ps(&real),
    );
    assert!(Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &junction_script
        ])
        .status()
        .unwrap()
        .success());
    let marker = real.join("must-not-run.txt");
    let argv = powershell(format!(
        "Set-Content -LiteralPath {} -Value bad",
        ps(&marker)
    ));
    let error = Sandbox::local()
        .prepare(
            &request(SandboxMode::WorkspaceWrite, &junction, true),
            &argv,
        )
        .unwrap_err();
    assert_eq!(error.code, "SANDBOX_INVALID_PATH");
    assert!(!marker.exists());

    let temp_inside = real.join("temp");
    fs::create_dir(&temp_inside).unwrap();
    let wrapped = plan(
        SandboxMode::WorkspaceWrite,
        &real,
        format!("Set-Content -LiteralPath {} -Value bad", ps(&marker)),
    );
    let refused = Command::new(&wrapped[0])
        .args(&wrapped[1..])
        .env("TEMP", &temp_inside)
        .env("TMP", &temp_inside)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("windows-acl-run:"));
    assert!(!marker.exists());
}

#[test]
fn runner_job_kills_started_descendant_after_parent_exits() {
    let root = TestRoot::new("job");
    let pid_file = root.0.join("descendant.pid");
    let release = root.0.join("release-parent");
    let script = format!(
        "$child = Start-Process powershell.exe -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-Command',{}) -PassThru; if ($child.HasExited) {{ exit 61 }}; Set-Content -LiteralPath {} -Value $child.Id -NoNewline; while (-not (Test-Path -LiteralPath {})) {{ Start-Sleep -Milliseconds 10 }}",
        ps_text("Start-Sleep -Seconds 60"),
        ps(&pid_file),
        ps(&release),
    );
    let mut runner = spawn(SandboxMode::WorkspaceWrite, &root.0, script);
    wait_for(&pid_file);
    let pid = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert!(
        process_is_alive(pid),
        "descendant {pid} was not alive before its parent exited"
    );

    fs::write(&release, "exit").unwrap();
    let result = runner.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_process_exits(pid);
}

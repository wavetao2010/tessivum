#![cfg(windows)]

use std::{
    fs,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::Duration,
};

use parking_lot::Mutex;
use serde_json::{json, Value};
use tessivum::{
    agent_mode::AgentModeId,
    host::{HostApi, HostConfig, HostNotification, HostRuntime},
    jobs::{JobSnapshot, JobStatus},
    protocol::{ContentBlock, SessionEvent, SessionPromptParams},
    SessionId,
};
use tokio::sync::broadcast;
use uuid::Uuid;
use windows_sys::Win32::{
    Foundation::CloseHandle,
    Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW},
    System::Threading::{
        GetExitCodeProcess, OpenProcess, TerminateProcess, WaitForSingleObject,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, SYNCHRONIZE,
    },
};

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(60);
const STILL_ACTIVE: u32 = 259;

struct TempWorkspace {
    path: PathBuf,
    cleanup_pids: Mutex<Vec<u32>>,
}

impl TempWorkspace {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("tessivum-windows-runtime-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("temporary Windows workspace creates");
        let workspace = Self {
            path,
            cleanup_pids: Mutex::new(Vec::new()),
        };
        assert_ntfs(workspace.path());
        workspace
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn guard(&self, pids: [u32; 2]) {
        self.cleanup_pids.lock().extend(pids);
    }

    fn disarm(&self) {
        self.cleanup_pids.lock().clear();
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let pids = self.cleanup_pids.get_mut();
        for &pid in pids.iter() {
            terminate_process(pid);
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn assert_ntfs(path: &Path) {
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut volume = [0u16; 512];
    assert_ne!(
        unsafe { GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) },
        0,
        "temporary workspace volume resolves"
    );
    let mut filesystem = [0u16; 32];
    assert_ne!(
        unsafe {
            GetVolumeInformationW(
                volume.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        },
        0,
        "temporary workspace filesystem resolves"
    );
    let length = filesystem
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(filesystem.len());
    assert_eq!(
        String::from_utf16_lossy(&filesystem[..length]),
        "NTFS",
        "Windows runtime probe requires a local NTFS workspace"
    );
}

fn terminate_process(pid: u32) {
    let process = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return;
    }
    unsafe {
        TerminateProcess(process, 1);
        WaitForSingleObject(process, 5_000);
        CloseHandle(process);
    }
}

fn process_is_running(pid: u32) -> bool {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let mut exit_code = 0;
    let queried = unsafe { GetExitCodeProcess(process, &mut exit_code) };
    unsafe { CloseHandle(process) };
    queried != 0 && exit_code == STILL_ACTIVE
}

async fn assert_terminated(pid: u32, role: &str) {
    let stopped = tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            if !process_is_running(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(stopped.is_ok(), "{role} process {pid} remained alive");
}

async fn wait_for_pids(path: &Path) -> [u32; 2] {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                let values = text
                    .lines()
                    .map(str::parse::<u32>)
                    .collect::<Result<Vec<_>, _>>()
                    .expect("ready handshake contains numeric process identifiers");
                if let [parent, descendant] = values.as_slice() {
                    assert_ne!(parent, descendant);
                    return [*parent, *descendant];
                }
                panic!("ready handshake must contain exactly two process identifiers");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("PowerShell parent and descendant report their ready handshake")
}

fn recorded_bash(calls: &[(&str, bool)]) -> String {
    let mut frames = Vec::<Value>::new();
    for (index, (command, background)) in calls.iter().enumerate() {
        let request_id = format!("windows-runtime-{}", index + 1);
        let call_id = format!("windows-runtime-call-{}", index + 1);
        let mut arguments = json!({
            "command": command,
            "description": "Exercise Host-owned Windows process cleanup."
        });
        if *background {
            arguments["run_in_background"] = json!(true);
        }
        let arguments = serde_json::to_string(&arguments).unwrap();
        frames.extend([
            json!({"requestId":request_id,"chunk":{"type":"block-start","index":0,"blockType":"tool-call"}}),
            json!({"requestId":request_id,"chunk":{"type":"tool-call-delta","index":0,"id":call_id,"name":"bash","argumentsDelta":arguments.clone()}}),
            json!({"requestId":request_id,"chunk":{"type":"block-end","index":0,"block":{"type":"tool-call","id":call_id,"name":"bash","arguments":arguments}}}),
            json!({"requestId":request_id,"chunk":{"type":"usage","usage":{"inputTokens":8,"outputTokens":3}}}),
            json!({"requestId":request_id,"chunk":{"type":"finish","reason":{"kind":"tool-calls"}}}),
        ]);
    }
    let request_id = format!("windows-runtime-{}", calls.len() + 1);
    frames.extend([
        json!({"requestId":request_id,"chunk":{"type":"block-start","index":0,"blockType":"text"}}),
        json!({"requestId":request_id,"chunk":{"type":"text-delta","index":0,"text":"Windows runtime probe complete."}}),
        json!({"requestId":request_id,"chunk":{"type":"block-end","index":0,"block":{"type":"text","text":"Windows runtime probe complete."}}}),
        json!({"requestId":request_id,"chunk":{"type":"finish","reason":{"kind":"stop"}}}),
    ]);
    frames
        .into_iter()
        .map(|frame| frame.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn host_config(workspace: &TempWorkspace, replay: String) -> HostConfig {
    let mut config = HostConfig::new(workspace.path(), workspace.path().join("data"))
        .with_recorded_replay(replay);
    config.enable_trusted_bash = true;
    config
}

fn prompt(session_id: SessionId) -> SessionPromptParams {
    SessionPromptParams {
        session_id,
        content_blocks: vec![ContentBlock::Text {
            text: "Run the recorded Windows process probe.".into(),
        }],
        client_time_zone: None,
    }
}

async fn prepare_danger_session(host: &impl HostApi, session: &SessionId) {
    host.create_session(session.clone()).await.unwrap();
    let result = host
        .command_execute(session.clone(), "/permission danger-full-access".into())
        .await
        .unwrap()
        .expect("permission command is public");
    assert_eq!(
        serde_json::to_value(result.result).unwrap()["kind"],
        "success"
    );
}

async fn wait_for_tool_result(host: &impl HostApi, session: &SessionId) -> SessionEvent {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            if let Some(event) = host
                .events(session.clone(), 0)
                .await
                .unwrap()
                .into_iter()
                .find(|event| event.event_type == "tool/result")
            {
                return event;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("recorded bash tool returns through HostApi events")
}

async fn wait_for_job_status(
    notifications: &mut broadcast::Receiver<HostNotification>,
    session: &SessionId,
    job_id: &str,
    status: JobStatus,
) -> JobSnapshot {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            match notifications.recv().await {
                Ok(HostNotification::SessionJobs(update)) if &update.session_id == session => {
                    if let Some(job) = update
                        .jobs
                        .into_iter()
                        .find(|job| job.id.as_str() == job_id && job.status == status)
                    {
                        return job;
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    panic!("Host notification stream closed before job status {status:?}")
                }
            }
        }
    })
    .await
    .expect("Host publishes the expected background job status")
}

fn long_lived_process_command(ready_file: &str) -> String {
    format!(
        concat!(
            "$program = [Diagnostics.Process]::GetCurrentProcess().MainModule.FileName; ",
            "$child = Start-Process -FilePath $program -ArgumentList ",
            "'-NoLogo','-NoProfile','-NonInteractive','-Command',",
            "'Start-Sleep -Seconds 300' -PassThru; ",
            "$ready = [string]$PID + \"`n\" + [string]$child.Id + \"`n\"; ",
            "[IO.File]::WriteAllText('{ready_file}.tmp', $ready); ",
            "Move-Item -LiteralPath '{ready_file}.tmp' -Destination '{ready_file}'; ",
            "Wait-Process -Id $child.Id"
        ),
        ready_file = ready_file
    )
}

#[tokio::test]
async fn host_shutdown_kills_background_powershell_and_its_descendant() {
    let workspace = TempWorkspace::new();
    let command = long_lived_process_command("background.ready");
    let runtime = HostRuntime::boot(host_config(
        &workspace,
        recorded_bash(&[(command.as_str(), true)]),
    ))
    .await
    .unwrap();
    let host = runtime.handle();
    let session = SessionId::from("windows-background-shutdown");
    prepare_danger_session(&host, &session).await;
    let mut notifications = host.subscribe();

    host.prompt(prompt(session.clone())).await.unwrap();
    let result = wait_for_tool_result(&host, &session).await;
    assert_eq!(result.data["message"]["content"][0]["isError"], false);
    let pids = wait_for_pids(&workspace.path().join("background.ready")).await;
    workspace.guard(pids);
    let job_id = result.data["meta"]["jobId"]
        .as_str()
        .expect("background tool result carries its real job identifier");
    assert!(job_id.starts_with("bash-"));
    assert!(result.data["message"]["content"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains(job_id));
    let running =
        wait_for_job_status(&mut notifications, &session, job_id, JobStatus::Running).await;
    assert_eq!(running.kind, "bash");

    assert!(
        process_is_running(pids[0]),
        "PowerShell parent did not stay live"
    );
    assert!(
        process_is_running(pids[1]),
        "PowerShell descendant did not stay live"
    );

    tokio::time::timeout(SHUTDOWN_TIMEOUT, runtime.shutdown())
        .await
        .expect("HostRuntime shutdown remains bounded")
        .unwrap();
    let killed = wait_for_job_status(&mut notifications, &session, job_id, JobStatus::Killed).await;
    assert!(killed.finished_at.is_some());
    assert!(killed.result.is_none());
    assert_terminated(pids[0], "background PowerShell parent").await;
    assert_terminated(pids[1], "background PowerShell descendant").await;
    workspace.disarm();
}

#[tokio::test]
async fn host_shutdown_cancels_minimal_persistent_powershell_process_tree() {
    let workspace = TempWorkspace::new();
    let setup = concat!(
        "[IO.File]::WriteAllText('persistent.parent.tmp', [string]$PID); ",
        "Move-Item -LiteralPath 'persistent.parent.tmp' -Destination 'persistent.parent'"
    );
    let command = long_lived_process_command("persistent.ready");
    let config = host_config(
        &workspace,
        recorded_bash(&[(setup, false), (command.as_str(), false)]),
    )
    .with_default_agent_mode(AgentModeId::minimal());
    let runtime = HostRuntime::boot(config).await.unwrap();
    let host = runtime.handle();
    let session = SessionId::from("windows-minimal-persistent-shutdown");
    prepare_danger_session(&host, &session).await;

    host.prompt(prompt(session.clone())).await.unwrap();
    let pids = wait_for_pids(&workspace.path().join("persistent.ready")).await;
    workspace.guard(pids);
    assert_eq!(
        fs::read_to_string(workspace.path().join("persistent.parent"))
            .unwrap()
            .parse::<u32>()
            .unwrap(),
        pids[0],
        "both calls execute in the same Minimal persistent PowerShell"
    );
    assert!(
        process_is_running(pids[0]),
        "persistent PowerShell did not stay live"
    );
    assert!(
        process_is_running(pids[1]),
        "persistent descendant did not stay live"
    );

    tokio::time::timeout(SHUTDOWN_TIMEOUT, runtime.shutdown())
        .await
        .expect("HostRuntime persistent-session shutdown remains bounded")
        .unwrap();
    let events = host.events(session, 0).await.unwrap();
    let result = events
        .iter()
        .filter(|event| event.event_type == "tool/result")
        .find(|event| {
            matches!(
                event.data["meta"]["code"].as_str(),
                Some("PERSISTENT_SHELL_CANCELLED" | "PERSISTENT_SHELL_DISPOSED")
            )
        })
        .expect("Host records cancellation of the active persistent bash call");
    assert_eq!(result.data["message"]["content"][0]["isError"], true);
    assert!(events.iter().any(|event| {
        event.event_type == "turn/end"
            && event.data["reason"]["kind"] == "aborted"
            && event.data["reason"]["reason"]
                == json!({
                    "kind": "hook",
                    "reason": "host shutdown"
                })
    }));
    assert_terminated(pids[0], "persistent PowerShell parent").await;
    assert_terminated(pids[1], "persistent PowerShell descendant").await;
    workspace.disarm();
}

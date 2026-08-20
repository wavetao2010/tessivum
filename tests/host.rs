use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::json;
use tessivum::{
    credentials::{CredentialError, CredentialRef},
    host::{HostApi, HostConfig, HostNotification, HostRuntime, HostSettingsMutation},
    persistence_jsonl::JsonlSessionPersistence,
    protocol::{
        AgentCancelCause, ContentBlock, SessionHeader, SessionModelSelection, SessionPromptParams,
        SessionStatus, ToolCallId, SESSION_FORMAT_VERSION,
    },
    session::SessionPersistence,
    settings::{
        SettingsError, SettingsEventKind, SettingsRegistration, AGENT_DEFAULT_MODEL_NAMESPACE,
        LLM_OPENAI_RESPONSES_NAMESPACE,
    },
    SessionId,
};
use tessivum_core::ContextHandle;
use uuid::Uuid;

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes
}

const REPLAY: &str = include_str!("../fixtures/headless/recorded-replay.jsonl");
const RESUME_REPLAY: &str = concat!(
    "{\"requestId\":\"one\",\"chunk\":{\"type\":\"block-start\",\"index\":0,\"blockType\":\"text\"}}\n",
    "{\"requestId\":\"one\",\"chunk\":{\"type\":\"text-delta\",\"index\":0,\"text\":\"first\"}}\n",
    "{\"requestId\":\"one\",\"chunk\":{\"type\":\"block-end\",\"index\":0,\"block\":{\"type\":\"text\",\"text\":\"first\"}}}\n",
    "{\"requestId\":\"one\",\"chunk\":{\"type\":\"finish\",\"reason\":{\"kind\":\"stop\"}}}\n",
    "{\"requestId\":\"two\",\"chunk\":{\"type\":\"block-start\",\"index\":0,\"blockType\":\"text\"}}\n",
    "{\"requestId\":\"two\",\"chunk\":{\"type\":\"text-delta\",\"index\":0,\"text\":\"second\"}}\n",
    "{\"requestId\":\"two\",\"chunk\":{\"type\":\"block-end\",\"index\":0,\"block\":{\"type\":\"text\",\"text\":\"second\"}}}\n",
    "{\"requestId\":\"two\",\"chunk\":{\"type\":\"finish\",\"reason\":{\"kind\":\"stop\"}}}\n",
);

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tessivum-host-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
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

fn config(root: &TempDir) -> HostConfig {
    let mut config =
        HostConfig::new(root.path(), root.path().join("data")).with_recorded_replay(REPLAY);
    config.provider = "cli-mock".into();
    config.model = "cli-mock".into();
    config.enable_trusted_bash = true;
    config
}

fn dynamic_config(root: &TempDir) -> HostConfig {
    let mut config = HostConfig::new(root.path(), root.path().join("data"));
    config.provider = "openai-responses".into();
    config.model = "alpha".into();
    config.profile_patch = json!({
        "llm-openai-responses": {
            "providers": {
                "openai-responses": {
                    "displayName": "Test relay",
                    "baseURL": "http://127.0.0.1:1/v1",
                    "apiKeyEnv": "TESSIVUM_DYNAMIC_TEST_KEY",
                    "models": [
                        {"id": "alpha", "input": ["text"]},
                        {"id": "beta", "input": ["text", "image"]}
                    ]
                }
            }
        }
    });
    config
}

async fn wait_for_models_changed(
    notifications: &mut tokio::sync::broadcast::Receiver<HostNotification>,
) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                notifications.recv().await.unwrap(),
                HostNotification::ModelsChanged
            ) {
                return;
            }
        }
    })
    .await
    .expect("models change notification arrives");
}

fn prompt(session: &str) -> SessionPromptParams {
    SessionPromptParams {
        session_id: SessionId::from(session),
        content_blocks: vec![ContentBlock::Text {
            text: "exercise host receipt".into(),
        }],
    }
}

async fn wait_for_event(host: &impl HostApi, session: SessionId) {
    for _ in 0..100 {
        if !host.events(session.clone(), 0).await.unwrap().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("recorded prompt did not commit an event");
}

#[test]
fn profile_patches_have_fixed_precedence() {
    let root = TempDir::new();
    let mut config = HostConfig::new(root.path(), root.path().join("data"));
    config.bundle_patch = json!({"value":"bundle","nested":{"bundle":true,"winner":"bundle"}});
    config.profile_patch = json!({"value":"profile","nested":{"profile":true,"winner":"profile"}});
    config.home_patch = json!({"value":"home","nested":{"home":true,"winner":"home"}});
    config.cli_patches = vec![json!({"value":"cli","nested":{"cli":true,"winner":"cli"}})];
    config.telemetry_patch =
        json!({"value":"telemetry","nested":{"telemetry":true,"winner":"telemetry"}});
    assert_eq!(
        config.compose_profile().unwrap(),
        json!({
            "value":"telemetry",
            "nested":{"bundle":true,"profile":true,"home":true,"cli":true,"telemetry":true,"winner":"telemetry"}
        })
    );
}

#[tokio::test]
async fn prompt_receipt_relays_committed_events_and_flushes_on_shutdown() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root.path().join("data"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
    let handle = runtime.handle();
    let mut notifications = handle.subscribe();
    let receipt = handle.prompt(prompt("host-prompt")).await.unwrap();
    assert!(!receipt.message_id.as_str().is_empty());
    let events = handle
        .events(SessionId::from("host-prompt"), 0)
        .await
        .unwrap();
    assert!(
        events.iter().any(|event| event.event_type == "user/message"
            && event.data.get("id").and_then(serde_json::Value::as_str)
                == Some(receipt.message_id.as_str())),
        "a returned receipt must survive an immediate process crash/read race"
    );
    let notification = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let tessivum::host::HostNotification::SessionEvent(notification) =
                notifications.recv().await.unwrap()
            {
                return notification;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(notification.session_id, SessionId::from("host-prompt"));
    runtime.shutdown().await.unwrap();
    let reopened = HostRuntime::boot(config(&root)).await.unwrap();
    assert!(!reopened
        .handle()
        .events(SessionId::from("host-prompt"), 0)
        .await
        .unwrap()
        .is_empty());
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_relay_replays_startup_asked_without_durable_tool_details() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root).with_approval_required_tool("bash"))
        .await
        .unwrap();
    let handle = runtime.handle();
    let mut notifications = handle.subscribe();
    handle.prompt(prompt("approval-relay")).await.unwrap();

    let requested = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let tessivum::host::HostNotification::ApprovalRequested(requested) =
                notifications.recv().await.unwrap()
            {
                return requested;
            }
        }
    })
    .await
    .unwrap();
    let asked = handle
        .events(SessionId::from("approval-relay"), 0)
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == "approval/asked")
        .unwrap()
        .data;
    assert_eq!(asked["approvalId"], json!(requested.approval_id.as_str()));
    assert_eq!(asked["sessionId"], json!("approval-relay"));
    assert_eq!(asked["toolName"], json!("bash"));
    assert_eq!(asked["callId"], json!("cli-smoke-call"));
    assert_eq!(asked["request"], json!({"action": "bash"}));
    assert!(!asked.to_string().contains("CLI_TOOL_ROUND_TRIP"));

    assert!(
        handle
            .approval_registry()
            .unwrap()
            .respond(
                &requested.rpc_id,
                &requested.session_id,
                &requested.approval_id,
                tessivum::approval::ApprovalOutcome::Rejected,
            )
            .accepted
    );
    let resolved = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let tessivum::host::HostNotification::ApprovalResolved(resolved) =
                notifications.recv().await.unwrap()
            {
                return resolved;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(resolved.approval_id, requested.approval_id);
    assert_eq!(
        resolved.outcome,
        tessivum::approval::ApprovalOutcome::Rejected
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn runtime_resumes_a_durable_session_without_replacing_it() {
    let root = TempDir::new();
    let first = HostRuntime::boot(config(&root)).await.unwrap();
    first.handle().prompt(prompt("host-resume")).await.unwrap();
    wait_for_event(&first.handle(), SessionId::from("host-resume")).await;
    let first_len = first
        .handle()
        .events(SessionId::from("host-resume"), 0)
        .await
        .unwrap()
        .len();
    first.shutdown().await.unwrap();
    let second = HostRuntime::boot(config(&root)).await.unwrap();
    second.handle().prompt(prompt("host-resume")).await.unwrap();
    for _ in 0..100 {
        if second
            .handle()
            .events(SessionId::from("host-resume"), 0)
            .await
            .unwrap()
            .len()
            > first_len
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        second
            .handle()
            .events(SessionId::from("host-resume"), 0)
            .await
            .unwrap()
            .len()
            > first_len
    );
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_session_resumes_with_a_fresh_agent_generation() {
    let root = TempDir::new();
    let config =
        HostConfig::new(root.path(), root.path().join("data")).with_recorded_replay(RESUME_REPLAY);
    let runtime = HostRuntime::boot(config).await.unwrap();
    let handle = runtime.handle();
    let session = SessionId::from("cancel-resume");

    handle.prompt(prompt(session.as_str())).await.unwrap();
    for _ in 0..100 {
        if handle.status(session.clone()).await.unwrap() == Some(SessionStatus::Idle)
            && handle
                .events(session.clone(), 0)
                .await
                .unwrap()
                .iter()
                .any(|event| {
                    event.event_type == "assistant/message"
                        && event.data.to_string().contains("first")
                })
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(handle
        .cancel(session.clone(), AgentCancelCause::User)
        .await
        .unwrap());
    handle.prompt(prompt(session.as_str())).await.unwrap();
    for _ in 0..100 {
        if handle
            .events(session.clone(), 0)
            .await
            .unwrap()
            .iter()
            .any(|event| {
                event.event_type == "assistant/message" && event.data.to_string().contains("second")
            })
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(handle
        .events(session, 0)
        .await
        .unwrap()
        .iter()
        .any(|event| {
            event.event_type == "assistant/message" && event.data.to_string().contains("second")
        }));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_fences_new_admission_and_leaves_no_owned_processes() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = runtime.handle();
    handle.prompt(prompt("host-fence")).await.unwrap();
    let shutting = handle.clone();
    let task = tokio::spawn(async move { shutting.shutdown().await });
    for _ in 0..100 {
        if handle.is_shutting_down() {
            break;
        }
        tokio::task::yield_now().await;
    }
    let error = handle
        .prompt(prompt("rejected-after-shutdown"))
        .await
        .unwrap_err();
    assert_eq!(error.code, "HOST_SHUTTING_DOWN");
    task.await.unwrap().unwrap();
    assert_eq!(handle.in_flight(), 0);
    assert!(
        fs::remove_dir_all(root.path()).is_ok(),
        "all host-owned file/process resources release before shutdown returns"
    );
}

#[tokio::test]
async fn shutdown_drains_racing_settings_writes_before_relays_close() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = runtime.handle();
    let settings = handle.settings().unwrap();
    let credentials = handle.credentials().unwrap();
    let namespace = "ui-theme";
    let reference =
        CredentialRef::new(format!("TESSIVUM_RACE_{}", Uuid::new_v4().simple())).unwrap();
    settings
        .register(SettingsRegistration::new(
            namespace,
            json!({}),
            json!({}),
            json!({}),
        ))
        .await
        .unwrap();
    let mut notifications = handle.subscribe();

    let (settings_result, credentials_result, shutdown_result) = tokio::join!(
        settings.update(namespace, json!({"saved": true}), None),
        credentials.set(reference.clone(), "racing-credential".into()),
        runtime.shutdown(),
    );
    settings_result.unwrap();
    credentials_result.unwrap();
    shutdown_result.unwrap();

    let mut settings_changed = false;
    let mut credentials_changed = false;
    for _ in 0..8 {
        if settings_changed && credentials_changed {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(1), notifications.recv())
            .await
            .unwrap()
            .unwrap()
        {
            HostNotification::SettingsChanged(event)
                if event.namespace == namespace && event.kind == SettingsEventKind::Updated =>
            {
                settings_changed = true;
            }
            HostNotification::CredentialsChanged(event) if event.reference == reference => {
                credentials_changed = true;
            }
            _ => {}
        }
    }
    assert!(settings_changed && credentials_changed);
}

#[tokio::test]
async fn host_approval_registry_tracks_owned_agent_generations() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = runtime.handle();
    let approvals = handle.approval_registry().unwrap();
    let session = SessionId::from("host-approval-lifetime");
    assert!(approvals.lookup(&session).is_none());

    handle.prompt(prompt(session.as_str())).await.unwrap();
    assert!(approvals.lookup(&session).is_some());
    assert!(handle
        .cancel(session.clone(), AgentCancelCause::User)
        .await
        .unwrap());
    assert!(approvals.lookup(&session).is_none());

    handle.prompt(prompt(session.as_str())).await.unwrap();
    assert!(approvals.lookup(&session).is_some());
    runtime.shutdown().await.unwrap();
    assert!(approvals.lookup(&session).is_none());
}

#[tokio::test]
async fn host_services_use_default_paths_persist_and_drain_on_shutdown() {
    let root = TempDir::new();
    let namespace = format!("host-{}", Uuid::new_v4().simple());
    let reference =
        CredentialRef::new(format!("TESSIVUM_HOST_{}", Uuid::new_v4().simple())).unwrap();
    let value = "host-credential-value";
    let first = HostRuntime::boot(config(&root)).await.unwrap();
    let handle = first.handle();
    let settings = handle.settings().unwrap();
    let credentials = handle.credentials().unwrap();
    settings
        .register(SettingsRegistration::new(
            namespace.clone(),
            json!({}),
            json!({}),
            json!({}),
        ))
        .await
        .unwrap();
    settings
        .update(&namespace, json!({"saved": true}), None)
        .await
        .unwrap();
    let mut credential_events = credentials.subscribe();
    credentials
        .set(reference.clone(), value.into())
        .await
        .unwrap();
    assert!(!format!("{credentials:?}").contains(value));
    assert!(
        !serde_json::to_string(&credential_events.recv().await.unwrap())
            .unwrap()
            .contains(value)
    );
    let shadow_reference =
        CredentialRef::new(format!("TESSIVUM_SHADOW_{}", Uuid::new_v4().simple())).unwrap();
    let shadow_value = "host-environment-secret";
    std::env::set_var(shadow_reference.as_str(), shadow_value);
    let shadowed_set = credentials
        .set(shadow_reference.clone(), "host-file-secret".into())
        .await;
    let shadowed_unset = credentials.unset(&shadow_reference).await;
    std::env::remove_var(shadow_reference.as_str());
    let shadowed_set = shadowed_set.unwrap_err();
    assert!(matches!(&shadowed_set, CredentialError::Shadowed(_)));
    assert!(
        !shadowed_set.to_string().contains(shadow_value)
            && !shadowed_set.to_string().contains("host-file-secret")
    );
    assert!(matches!(shadowed_unset, Err(CredentialError::Shadowed(_))));
    assert!(root.path().join("data/settings.yaml").is_file());
    assert!(root.path().join("data/credentials.yaml").is_file());

    first.shutdown().await.unwrap();
    assert!(matches!(
        settings
            .update(&namespace, json!({"after": true}), None)
            .await,
        Err(SettingsError::Closed)
    ));
    assert!(matches!(
        credentials.set(reference.clone(), value.into()).await,
        Err(CredentialError::Closed)
    ));

    let second = HostRuntime::boot(config(&root)).await.unwrap();
    let settings = second.handle().settings().unwrap();
    let credentials = second.handle().credentials().unwrap();
    settings
        .register(SettingsRegistration::new(
            namespace.clone(),
            json!({}),
            json!({}),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(
        settings.get(&namespace).unwrap().value,
        json!({"saved": true})
    );
    assert_eq!(
        credentials.resolve(&reference).await.unwrap(),
        Some(value.into())
    );
    credentials.unset(&reference).await.unwrap();
    second.shutdown().await.unwrap();

    let third = HostRuntime::boot(config(&root)).await.unwrap();
    assert_eq!(
        third
            .handle()
            .credentials()
            .unwrap()
            .resolve(&reference)
            .await
            .unwrap(),
        None
    );
    third.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_uses_selected_storage_files_and_rejects_directories() {
    let root = TempDir::new();
    let settings_path = root.path().join("selected/settings.yaml");
    let credentials_path = root.path().join("selected/credentials.yaml");
    let runtime = HostRuntime::boot(
        config(&root)
            .with_settings_path(&settings_path)
            .with_credentials_path(&credentials_path),
    )
    .await
    .unwrap();
    let settings = runtime.handle().settings().unwrap();
    settings
        .register(SettingsRegistration::new(
            "selected",
            json!({}),
            json!({}),
            json!({}),
        ))
        .await
        .unwrap();
    settings
        .update("selected", json!({"on": true}), None)
        .await
        .unwrap();
    runtime
        .handle()
        .credentials()
        .unwrap()
        .set(
            CredentialRef::new("TESSIVUM_SELECTED").unwrap(),
            "value".into(),
        )
        .await
        .unwrap();
    assert!(settings_path.is_file());
    assert!(credentials_path.is_file());
    assert!(!root.path().join("data/settings.yaml").exists());
    assert!(!root.path().join("data/credentials.yaml").exists());
    runtime.shutdown().await.unwrap();

    let rejected = HostRuntime::boot(config(&root).with_settings_path(root.path()))
        .await
        .err()
        .unwrap();
    assert_eq!(rejected.code(), "INVALID_HOST_CONFIG");
}

#[tokio::test]
async fn host_persists_workspace_session_attachment_and_retries_ungrouped_blanks() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let registry = runtime.workspace_registry().unwrap();
    let default_workspace = registry.list().into_iter().next().unwrap().workspace_id;
    let direct = runtime
        .create_session(SessionId::from("workspace-direct"))
        .await
        .unwrap();
    assert_eq!(direct.workspace_id, Some(default_workspace.clone()));

    let other_dir = root.path().join("other-workspace");
    fs::create_dir(&other_dir).unwrap();
    let other_workspace = registry
        .create(&other_dir, None)
        .unwrap()
        .workspace
        .workspace_id;
    let attached = runtime
        .create_session_in(SessionId::from("workspace-other"), other_workspace.clone())
        .await
        .unwrap();
    assert_eq!(attached.workspace_id, Some(other_workspace));
    assert_eq!(
        runtime
            .create_session_in(
                SessionId::from("workspace-other"),
                default_workspace.clone()
            )
            .await
            .unwrap_err()
            .code,
        "SESSION_CONFLICT"
    );

    let registry_path = root.path().join("data/workspaces.json");
    fs::remove_file(&registry_path).unwrap();
    fs::create_dir(&registry_path).unwrap();
    let failed = runtime
        .create_session_in(
            SessionId::from("workspace-retry"),
            default_workspace.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(failed.code, "WORKSPACE_ATTACH_FAILED");
    let ungrouped = runtime
        .list_sessions()
        .await
        .unwrap()
        .into_iter()
        .find(|session| session.session_id == SessionId::from("workspace-retry"))
        .unwrap();
    assert_eq!(ungrouped.workspace_id, None);
    assert_eq!(
        runtime
            .prompt(prompt("workspace-retry"))
            .await
            .unwrap_err()
            .code,
        "SESSION_UNGROUPED"
    );
    fs::remove_dir(&registry_path).unwrap();
    let retried = runtime
        .create_session_in(
            SessionId::from("workspace-retry"),
            default_workspace.clone(),
        )
        .await
        .unwrap();
    assert_eq!(retried.workspace_id, Some(default_workspace.clone()));
    runtime.shutdown().await.unwrap();

    let restarted = HostRuntime::boot(config(&root)).await.unwrap();
    let reopened = restarted.workspace_registry().unwrap();
    assert!(reopened
        .list()
        .into_iter()
        .any(|workspace| workspace.workspace_id == default_workspace));
    assert_eq!(
        restarted
            .list_sessions()
            .await
            .unwrap()
            .into_iter()
            .find(|session| session.session_id == SessionId::from("workspace-retry"))
            .unwrap()
            .workspace_id,
        Some(default_workspace)
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_reads_only_durable_session_attachments_without_resuming() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let session = SessionId::from("attachment-read");
    let reference = runtime
        .upload_attachment(png(1, 1), Some("nested.png".into()))
        .await
        .unwrap();
    runtime
        .prompt(SessionPromptParams {
            session_id: session.clone(),
            content_blocks: vec![ContentBlock::ToolResult {
                tool_call_id: ToolCallId::from("attachment-tool"),
                content: vec![ContentBlock::Image {
                    attachment: serde_json::to_value(&reference).unwrap(),
                }],
                is_error: None,
            }],
        })
        .await
        .unwrap();

    let live = runtime
        .read_attachment(session.clone(), reference.attachment_id.clone())
        .await
        .unwrap();
    assert_eq!(live.reference, reference);
    assert_eq!(live.data, png(1, 1));

    let unreferenced = runtime.upload_attachment(png(2, 2), None).await.unwrap();
    let unreferenced_path = root.path().join("data/attachments/v1").join(
        unreferenced
            .attachment_id
            .as_str()
            .strip_prefix("sha256:")
            .unwrap(),
    );
    assert_eq!(
        runtime
            .read_attachment(session.clone(), unreferenced.attachment_id.clone())
            .await
            .unwrap_err()
            .code,
        "ATTACHMENT_NOT_REFERENCED"
    );
    assert_eq!(
        runtime
            .read_attachment(SessionId::from(""), reference.attachment_id.clone())
            .await
            .unwrap_err()
            .code,
        "INVALID_SESSION_ID"
    );

    runtime.shutdown().await.unwrap();
    assert!(
        !unreferenced_path.exists(),
        "shutdown removes unattached blobs"
    );
    drop(runtime);

    let restarted = HostRuntime::boot(config(&root)).await.unwrap();
    let before = restarted.events(session.clone(), 0).await.unwrap();
    let persisted = restarted
        .read_attachment(session.clone(), reference.attachment_id)
        .await
        .unwrap();
    assert_eq!(persisted.reference, live.reference);
    assert_eq!(persisted.data, live.data);
    assert_eq!(restarted.events(session, 0).await.unwrap(), before);
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_boot_migrates_durable_session_cwds_once() {
    let root = TempDir::new();
    let legacy_workspace = root.path().join("legacy-workspace");
    fs::create_dir(&legacy_workspace).unwrap();
    let persistence = JsonlSessionPersistence::new(root.path().join("data"));
    let context = ContextHandle::root();
    persistence
        .create(
            &SessionHeader {
                version: SESSION_FORMAT_VERSION,
                id: SessionId::from("legacy-workspace-session"),
                created_at: 1,
                cwd: Some(
                    legacy_workspace
                        .canonicalize()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                ),
                parent_session: None,
                seed_length: None,
                origin: None,
                delegation_depth: Some(0),
                agent_preset: None,
            },
            context.scope().cancellation(),
        )
        .await
        .unwrap();

    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let first = runtime
        .workspace_registry()
        .unwrap()
        .workspace_for_session("legacy-workspace-session")
        .unwrap();
    assert_eq!(
        first.path,
        legacy_workspace.canonicalize().unwrap().to_string_lossy()
    );
    let workspace_id = first.workspace_id;
    runtime.shutdown().await.unwrap();

    let restarted = HostRuntime::boot(config(&root)).await.unwrap();
    assert_eq!(
        restarted
            .workspace_registry()
            .unwrap()
            .workspace_for_session("legacy-workspace-session")
            .unwrap()
            .workspace_id,
        workspace_id
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn deleting_workspace_disposes_agents_and_denies_default_sessions() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let registry = runtime.workspace_registry().unwrap();
    let default_workspace = registry.list().into_iter().next().unwrap().workspace_id;
    let other_dir = root.path().join("other-workspace");
    fs::create_dir(&other_dir).unwrap();
    let other_workspace = registry
        .create(&other_dir, None)
        .unwrap()
        .workspace
        .workspace_id;
    let deleted_session = SessionId::from("deleted-live");

    runtime
        .create_session_in(deleted_session.clone(), default_workspace.clone())
        .await
        .unwrap();
    runtime.prompt(prompt("deleted-live")).await.unwrap();
    let approvals = runtime.approval_registry().unwrap();
    assert!(approvals.lookup(&deleted_session).is_some());

    assert!(runtime
        .delete_workspace(default_workspace.clone())
        .await
        .unwrap());
    assert!(registry.workspace_for_session(&deleted_session).is_none());
    assert!(approvals.lookup(&deleted_session).is_none());
    let after_delete = runtime.events(deleted_session.clone(), 0).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        runtime.events(deleted_session.clone(), 0).await.unwrap(),
        after_delete
    );
    assert_eq!(
        runtime
            .prompt(prompt("deleted-live"))
            .await
            .unwrap_err()
            .code,
        "SESSION_UNGROUPED"
    );
    assert_eq!(
        runtime
            .create_session(SessionId::from("default-denied"))
            .await
            .unwrap_err()
            .code,
        "WORKSPACE_NOT_FOUND"
    );
    assert_eq!(
        runtime
            .create_session_in(SessionId::from("explicit-other"), other_workspace.clone())
            .await
            .unwrap()
            .workspace_id,
        Some(other_workspace)
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_creation_is_serial_idempotent_and_conflict_safe() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(config(&root)).await.unwrap();
    let registry = runtime.workspace_registry().unwrap();
    let first_dir = root.path().join("first-workspace");
    let second_dir = root.path().join("second-workspace");
    fs::create_dir(&first_dir).unwrap();
    fs::create_dir(&second_dir).unwrap();
    let first_workspace = registry
        .create(&first_dir, None)
        .unwrap()
        .workspace
        .workspace_id;
    let second_workspace = registry
        .create(&second_dir, None)
        .unwrap()
        .workspace
        .workspace_id;

    let automatic = runtime.handle();
    tokio::time::timeout(
        Duration::from_secs(1),
        automatic.prompt(prompt("automatic-session")),
    )
    .await
    .expect("automatic session creation must not deadlock")
    .unwrap();

    let left_handle = runtime.handle();
    let right_handle = runtime.handle();
    let same_session = SessionId::from("concurrent-same");
    let left_session = same_session.clone();
    let right_session = same_session.clone();
    let left_workspace = first_workspace.clone();
    let right_workspace = first_workspace.clone();
    let (left, right) = tokio::time::timeout(Duration::from_secs(1), async move {
        tokio::join!(
            left_handle.create_session_in(left_session, left_workspace),
            right_handle.create_session_in(right_session, right_workspace),
        )
    })
    .await
    .expect("same-workspace creation must not deadlock");
    assert_eq!(left.unwrap().workspace_id, Some(first_workspace.clone()));
    assert_eq!(right.unwrap().workspace_id, Some(first_workspace.clone()));

    let first_handle = runtime.handle();
    let second_handle = runtime.handle();
    let conflict_session = SessionId::from("concurrent-conflict");
    let first_session = conflict_session.clone();
    let second_session = conflict_session;
    let first_workspace_id = first_workspace;
    let second_workspace_id = second_workspace;
    let (first, second) = tokio::time::timeout(Duration::from_secs(1), async move {
        tokio::join!(
            first_handle.create_session_in(first_session, first_workspace_id),
            second_handle.create_session_in(second_session, second_workspace_id),
        )
    })
    .await
    .expect("conflicting creation must not deadlock");
    match (first, second) {
        (Ok(_), Err(error)) | (Err(error), Ok(_)) => assert_eq!(error.code, "SESSION_CONFLICT"),
        result => panic!("expected one successful create and one conflict, got {result:?}"),
    }
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn dynamic_models_report_defaults_without_eager_legacy_migration() {
    let root = TempDir::new();
    let persistence = JsonlSessionPersistence::new(root.path().join("data"));
    let context = ContextHandle::root();
    persistence
        .create(
            &SessionHeader {
                version: SESSION_FORMAT_VERSION,
                id: SessionId::from("legacy-model"),
                created_at: 1,
                cwd: Some(
                    root.path()
                        .canonicalize()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                ),
                parent_session: None,
                seed_length: None,
                origin: None,
                delegation_depth: Some(0),
                agent_preset: None,
            },
            context.scope().cancellation(),
        )
        .await
        .unwrap();

    let runtime = HostRuntime::boot(dynamic_config(&root)).await.unwrap();
    let handle = runtime.handle();
    assert_eq!(
        handle
            .session_models(SessionId::from("legacy-model"))
            .await
            .unwrap()
            .current,
        Some(SessionModelSelection {
            provider: "openai-responses".into(),
            model: "alpha".into(),
            reasoning_effort: None,
        })
    );
    assert!(handle
        .events(SessionId::from("legacy-model"), 0)
        .await
        .unwrap()
        .is_empty());

    let mut notifications = handle.subscribe();
    handle
        .mutate_settings(
            AGENT_DEFAULT_MODEL_NAMESPACE.into(),
            HostSettingsMutation::Update {
                patch: json!({"provider": "openai-responses", "model": "beta"}),
                expected_revision: None,
            },
        )
        .await
        .unwrap();
    wait_for_models_changed(&mut notifications).await;
    assert_eq!(
        handle
            .session_models(SessionId::from("legacy-model"))
            .await
            .unwrap()
            .current
            .unwrap()
            .model,
        "beta"
    );
    assert!(
        handle
            .events(SessionId::from("legacy-model"), 0)
            .await
            .unwrap()
            .is_empty(),
        "catalog reads do not migrate legacy sessions"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn dynamic_route_notification_follows_committed_registration() {
    let root = TempDir::new();
    let runtime = HostRuntime::boot(dynamic_config(&root)).await.unwrap();
    let handle = runtime.handle();
    let mut notifications = handle.subscribe();
    handle
        .mutate_settings(
            LLM_OPENAI_RESPONSES_NAMESPACE.into(),
            HostSettingsMutation::Update {
                patch: json!({
                    "providers": {
                        "openai-responses": {
                            "displayName": "Updated relay",
                            "baseURL": "http://127.0.0.1:2/v1",
                            "apiKeyEnv": "TESSIVUM_UPDATED_TEST_KEY",
                            "models": [{"id": "beta", "input": ["text", "image"]}]
                        }
                    }
                }),
                expected_revision: None,
            },
        )
        .await
        .unwrap();
    wait_for_models_changed(&mut notifications).await;
    assert!(handle.provider_directory()[0].active);
    assert_eq!(
        handle.model_groups("openai-responses")[0].models[0].id,
        "beta"
    );
    runtime.shutdown().await.unwrap();
}

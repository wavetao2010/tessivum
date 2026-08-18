use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::json;
use tessivum::{
    host::{HostApi, HostConfig, HostRuntime},
    protocol::{AgentCancelCause, ContentBlock, SessionPromptParams, SessionStatus},
    credentials::{CredentialError, CredentialRef},
    settings::{SettingsError, SettingsRegistration},
    SessionId,
};
use uuid::Uuid;

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
    let reference = CredentialRef::new(format!("TESSIVUM_HOST_{}", Uuid::new_v4().simple()))
        .unwrap();
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
    credentials.set(reference.clone(), value.into()).await.unwrap();
    assert!(!format!("{credentials:?}").contains(value));
    assert!(!serde_json::to_string(&credential_events.recv().await.unwrap())
        .unwrap()
        .contains(value));
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
        settings.update(&namespace, json!({"after": true}), None).await,
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
    assert_eq!(settings.get(&namespace).unwrap().value, json!({"saved": true}));
    assert_eq!(credentials.resolve(&reference).await.unwrap(), Some(value.into()));
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
    settings.update("selected", json!({"on": true}), None).await.unwrap();
    runtime
        .handle()
        .credentials()
        .unwrap()
        .set(CredentialRef::new("TESSIVUM_SELECTED").unwrap(), "value".into())
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

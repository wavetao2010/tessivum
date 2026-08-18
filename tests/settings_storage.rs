use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, Mutex},
};

use serde_json::json;
use tessivum::{
    attachments::{AttachmentInput, AttachmentStore},
    credentials::{
        CredentialEnvironment, CredentialError, CredentialRef, Credentials, YamlCredentialFile,
    },
    settings::{
        MemorySettingsProvider, Settings, SettingsError, SettingsRegistration, YamlSettingsProvider,
    },
    storage::{MemoryStorageBackend, StorageError, StorageRegistry},
};
use uuid::Uuid;

fn root(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tessivum-{label}-{}", Uuid::new_v4()))
}
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes
}

#[tokio::test]
async fn settings_precedence_reset_conflict_redaction_and_last_good_yaml() {
    let provider = Arc::new(MemorySettingsProvider::new());
    provider
        .insert(
            "demo",
            json!({"nested": {"user": true}, "array": [3], "secret": "saved"}),
        )
        .unwrap();
    let settings = Settings::new(provider.clone());
    settings
        .register(
            SettingsRegistration::new(
                "demo",
                json!({"type": "object"}),
                json!({"nested": {"default": true}, "array": [1], "secret": "default"}),
                json!({"nested": {"base": true}, "array": [2]}),
            )
            .with_secret_paths(vec![vec!["secret".into()]]),
        )
        .await
        .unwrap();
    assert_eq!(
        settings.get("demo").unwrap().value,
        json!({"nested":{"default":true,"base":true,"user":true},"array":[3],"secret":"saved"})
    );
    let changed = settings
        .set_path(
            "demo",
            vec!["nested".into(), "user".into()],
            json!(false),
            Some(0),
        )
        .await
        .unwrap();
    assert_eq!(changed.revision, 1);
    assert_eq!(
        settings
            .update("demo", json!({"nested": {"more": 1}}), Some(0))
            .await
            .unwrap_err()
            .code(),
        "SETTINGS_CONFLICT"
    );
    let redacted = serde_json::to_string(&settings.describe("demo").unwrap()).unwrap();
    assert!(!redacted.contains("saved"));
    settings
        .remove_path("demo", vec!["nested".into(), "user".into()], Some(1))
        .await
        .unwrap();
    assert_eq!(
        settings.get("demo").unwrap().value["nested"],
        json!({"default":true,"base":true})
    );
    provider.set_writable(false);
    assert!(matches!(
        settings.update("demo", json!({"nope": true}), None).await,
        Err(SettingsError::ReadOnly)
    ));
    assert!(settings.get("demo").unwrap().value.get("nope").is_none());

    let path = root("settings").join("settings.yaml");
    let yaml_provider = Arc::new(YamlSettingsProvider::new(&path));
    let yaml = Settings::new(yaml_provider);
    yaml.register(SettingsRegistration::new(
        "demo",
        json!({}),
        json!({"value": 1}),
        json!({}),
    ))
    .await
    .unwrap();
    yaml.update("demo", json!({"value": 2}), None)
        .await
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    fs::write(&path, "demo: [not-a-document\n").unwrap();
    assert_eq!(
        yaml.reload("demo").await.unwrap().value,
        json!({"value": 2})
    );
    assert!(
        !path.parent().unwrap().read_dir().unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp"))
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[derive(Default)]
struct TestEnvironment(Mutex<BTreeMap<String, String>>);
impl CredentialEnvironment for TestEnvironment {
    fn get(&self, reference: &CredentialRef) -> Option<String> {
        self.0.lock().unwrap().get(reference.as_str()).cloned()
    }
}

#[tokio::test]
async fn credentials_shadow_live_environment_and_never_describe_values() {
    let root = root("credentials");
    let environment = Arc::new(TestEnvironment::default());
    environment
        .0
        .lock()
        .unwrap()
        .insert("TOKEN".into(), "environment-secret".into());
    let credentials = Credentials::with_environment(
        environment.clone(),
        Arc::new(YamlCredentialFile::new(root.join("credentials.yaml"))),
    );
    let mut events = credentials.subscribe();
    let token = CredentialRef::new("TOKEN").unwrap();
    assert_eq!(
        credentials.resolve(&token).await.unwrap(),
        Some("environment-secret".into())
    );
    let shadowed = credentials
        .set(token.clone(), "file-secret".into())
        .await
        .unwrap_err();
    assert!(matches!(&shadowed, CredentialError::Shadowed(_)));
    assert!(
        !shadowed.to_string().contains("environment-secret")
            && !shadowed.to_string().contains("file-secret")
    );
    let descriptor = serde_json::to_string(&credentials.describe(&token).await.unwrap()).unwrap();
    assert!(!descriptor.contains("environment-secret") && !descriptor.contains("file-secret"));
    environment.0.lock().unwrap().remove("TOKEN");
    credentials
        .set(token.clone(), "file-secret".into())
        .await
        .unwrap();
    assert!(!format!("{credentials:?}").contains("file-secret"));
    assert!(!serde_json::to_string(&events.recv().await.unwrap())
        .unwrap()
        .contains("file-secret"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root.join("credentials.yaml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert_eq!(
        credentials.resolve(&token).await.unwrap(),
        Some("file-secret".into())
    );
    credentials.unset(&token).await.unwrap();
    assert_eq!(credentials.resolve(&token).await.unwrap(), None);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn storage_rolls_back_failed_writes_orders_events_and_closes() {
    let backend = Arc::new(MemoryStorageBackend::new());
    let registry = StorageRegistry::new();
    registry
        .register_backend("memory", backend.clone())
        .unwrap();
    let domain = registry.open("memory", "prefs", 1).await.unwrap();
    backend.set_writable(false);
    assert!(matches!(
        domain.put("table", "unit", json!(1)).await,
        Err(StorageError::Persistence(_))
    ));
    assert_eq!(domain.get("table", "unit").unwrap(), None);
    backend.set_writable(true);
    let mut updates = domain.subscribe();
    domain.put("table", "one", json!(1)).await.unwrap();
    domain.put("table", "two", json!(2)).await.unwrap();
    assert_eq!(updates.recv().await.unwrap().revision, 1);
    assert_eq!(updates.recv().await.unwrap().revision, 2);
    domain.close().await.unwrap();
    assert!(matches!(
        domain.put("table", "three", json!(3)).await,
        Err(StorageError::Closed)
    ));
}

#[tokio::test]
async fn attachments_validate_batch_atomically_and_verify_reads() {
    let root = root("attachments");
    let store = AttachmentStore::new(&root, Default::default()).unwrap();
    assert!(store
        .save_batch(vec![
            AttachmentInput::new(png(1, 1), Some("ok.png".into())),
            AttachmentInput::new(b"not-image".to_vec(), None)
        ])
        .await
        .is_err());
    assert!(!root.join("v1").exists());
    let reference = store
        .save(AttachmentInput::new(
            png(2, 3),
            Some("../../unsafe.png".into()),
        ))
        .await
        .unwrap();
    assert_eq!(reference.name, None);
    assert_eq!(store.read_ref(&reference).await.unwrap(), png(2, 3));
    let filename = &reference.attachment_id.as_str()[7..];
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(root.join("v1").join(filename))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fs::write(root.join("v1").join(filename), png(4, 5)).unwrap();
    assert!(store.read(&reference.attachment_id).await.is_err());
    fs::remove_dir_all(root).unwrap();
}

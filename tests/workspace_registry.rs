use std::{
    fs,
    path::{Path, PathBuf},
    thread,
};

use tessivum::{
    protocol::{SessionHeader, SessionId, SESSION_FORMAT_VERSION},
    session::SessionInspection,
    workspace::{WorkspaceDiagnostic, WorkspaceRegistry, WorkspaceSnapshot},
};
use uuid::Uuid;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tessivum-workspace-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn inspection(id: &str, cwd: Option<&Path>) -> SessionInspection {
    SessionInspection {
        header: SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: SessionId::from(id),
            created_at: 0,
            cwd: cwd.map(|path| path.to_string_lossy().into_owned()),
            parent_session: None,
            seed_length: None,
            origin: None,
            delegation_depth: None,
            agent_preset: None,
        },
        event_count: 0,
        next_seq: 0,
        flush_count: 0,
    }
}

#[test]
fn snapshot_fixture_is_exact_and_unknown_fields_are_rejected() {
    let fixture = include_str!("../fixtures/workspaces/registry-v1.json");
    let snapshot: WorkspaceSnapshot = serde_json::from_str(fixture).unwrap();
    assert_eq!(
        serde_json::to_value(snapshot).unwrap(),
        serde_json::from_str::<serde_json::Value>(fixture).unwrap()
    );
    assert!(serde_json::from_str::<WorkspaceSnapshot>(include_str!(
        "../fixtures/workspaces/unknown-field-v1.json"
    ))
    .is_err());
}

#[test]
fn migration_groups_canonical_cwds_and_keeps_invalid_sessions_ungrouped() {
    let root = TempDir::new("migration");
    let host = root.dir("host");
    let other = root.dir("other");
    let missing = root.path().join("missing");
    #[cfg(unix)]
    let alias = {
        let alias = root.path().join("other-alias");
        std::os::unix::fs::symlink(&other, &alias).unwrap();
        alias
    };
    #[cfg(not(unix))]
    let alias = other.clone();
    let sessions = vec![
        inspection("host-session", Some(&host)),
        inspection("other-session", Some(&other)),
        inspection("alias-session", Some(&alias)),
        inspection("missing-session", Some(&missing)),
        inspection("no-cwd-session", None),
    ];
    let registry = WorkspaceRegistry::open(root.path().join("data"), &host, sessions).unwrap();
    let workspaces = registry.list();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(
        workspaces[0].session_ids,
        vec![SessionId::from("host-session")]
    );
    assert_eq!(
        workspaces[1].session_ids,
        vec![
            SessionId::from("other-session"),
            SessionId::from("alias-session")
        ]
    );
    assert_eq!(
        registry.diagnostics(),
        vec![
            WorkspaceDiagnostic::InvalidSessionCwd {
                session_id: SessionId::from("missing-session")
            },
            WorkspaceDiagnostic::MissingSessionCwd {
                session_id: SessionId::from("no-cwd-session")
            },
        ]
    );

    let ids = registry
        .list()
        .into_iter()
        .map(|workspace| workspace.workspace_id)
        .collect::<Vec<_>>();
    let reopened = WorkspaceRegistry::open(root.path().join("data"), &host, Vec::new()).unwrap();
    assert_eq!(
        reopened
            .list()
            .into_iter()
            .map(|workspace| workspace.workspace_id)
            .collect::<Vec<_>>(),
        ids
    );
}

#[test]
fn mutations_are_ordered_idempotent_and_preserve_global_archive() {
    let root = TempDir::new("mutations");
    let host = root.dir("host");
    let one = root.dir("one");
    let two = root.dir("two");
    let three = root.dir("three");
    let registry = WorkspaceRegistry::open(
        root.path().join("data"),
        &host,
        vec![
            inspection("a", None),
            inspection("b", None),
            inspection("c", None),
        ],
    )
    .unwrap();
    let first = registry.create(&one, None).unwrap();
    assert!(first.created);
    let unchanged = registry.revision();
    assert!(!registry.create(&one, None).unwrap().created);
    assert_eq!(registry.revision(), unchanged);
    let second = registry.create(&two, None).unwrap();
    let third = registry.create(&three, None).unwrap();
    let renamed = registry
        .rename(&first.workspace.workspace_id, "  renamed  ", None)
        .unwrap();
    assert_eq!(renamed.title, "renamed");
    assert_eq!(
        registry
            .rename(&renamed.workspace_id, "renamed", None)
            .unwrap(),
        renamed
    );
    assert_eq!(
        registry
            .rename(&second.workspace.workspace_id, "renamed", None)
            .unwrap_err()
            .code(),
        "WORKSPACE_TITLE_CONFLICT"
    );

    registry
        .insert_before(
            &third.workspace.workspace_id,
            Some(renamed.workspace_id.as_str()),
            None,
        )
        .unwrap();
    let ordered = registry.list();
    let position = ordered
        .iter()
        .position(|workspace| workspace.workspace_id == third.workspace.workspace_id)
        .unwrap();
    assert_eq!(ordered[position + 1].workspace_id, renamed.workspace_id);
    let revision = registry.revision();
    registry
        .insert_before(
            &third.workspace.workspace_id,
            Some(renamed.workspace_id.as_str()),
            None,
        )
        .unwrap();
    assert_eq!(registry.revision(), revision);

    registry
        .attach_session(&renamed.workspace_id, "a", None)
        .unwrap();
    registry
        .attach_session(&renamed.workspace_id, "b", None)
        .unwrap();
    registry
        .attach_session(&renamed.workspace_id, "b", None)
        .unwrap();
    registry
        .attach_session(&renamed.workspace_id, "c", None)
        .unwrap();
    registry
        .insert_session_before(&renamed.workspace_id, "c", Some("b"), None)
        .unwrap();
    assert_eq!(
        registry.workspace_for_session("a").unwrap().workspace_id,
        renamed.workspace_id
    );
    assert_eq!(
        registry.workspace_for_session("c").unwrap().session_ids,
        vec![
            SessionId::from("c"),
            SessionId::from("b"),
            SessionId::from("a")
        ]
    );
    registry.archive_session("a", None).unwrap();
    let revision = registry.revision();
    registry.archive_session("a", None).unwrap();
    assert_eq!(registry.revision(), revision);

    let lease = registry.resolve(&renamed.workspace_id).unwrap();
    assert!(registry.delete(&renamed.workspace_id, None).unwrap());
    assert_eq!(
        registry
            .delete(&renamed.workspace_id, None)
            .unwrap_err()
            .code(),
        "WORKSPACE_NOT_FOUND"
    );
    assert_eq!(
        registry.snapshot().archived_session_ids,
        vec![SessionId::from("a")]
    );
    assert_eq!(
        lease.validate_current().unwrap_err().code(),
        "STALE_WORKSPACE_LEASE"
    );
}

#[test]
fn compare_and_swap_and_concurrent_mutations_are_serialized() {
    let root = TempDir::new("cas");
    let host = root.dir("host");
    let one = root.dir("one");
    let two = root.dir("two");
    let three = root.dir("three");
    let registry = WorkspaceRegistry::open(root.path().join("data"), &host, Vec::new()).unwrap();
    let revision = registry.revision();
    registry.create(&one, Some(revision)).unwrap();
    assert_eq!(
        registry.create(&two, Some(revision)).unwrap_err().code(),
        "WORKSPACE_REVISION_CONFLICT"
    );

    let left = registry.clone();
    let right = registry.clone();
    let left_thread = thread::spawn(move || left.create(&two, None));
    let right_thread = thread::spawn(move || right.create(&three, None));
    left_thread.join().unwrap().unwrap();
    right_thread.join().unwrap().unwrap();
    assert_eq!(registry.list().len(), 4);
}

#[test]
fn corrupt_input_stale_temp_and_secure_file_are_handled() {
    let root = TempDir::new("durability");
    let host = root.dir("host");
    let data = root.dir("data");
    fs::write(data.join("workspaces.json"), b"{\"schemaVersion\":").unwrap();
    assert_eq!(
        WorkspaceRegistry::open(&data, &host, Vec::new())
            .unwrap_err()
            .code(),
        "WORKSPACE_REGISTRY_CORRUPT"
    );
    fs::remove_file(data.join("workspaces.json")).unwrap();
    let stale = data.join(format!(".workspaces.json-{}.tmp", Uuid::new_v4()));
    fs::write(&stale, b"torn").unwrap();
    WorkspaceRegistry::open(&data, &host, Vec::new()).unwrap();
    assert!(!stale.exists());
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(data.join("workspaces.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
fn lists_one_hundred_workspaces_and_one_thousand_sessions() {
    let root = TempDir::new("scale");
    let host = root.dir("host");
    let sessions = (0..1000)
        .map(|index| inspection(&format!("session-{index}"), None))
        .collect::<Vec<_>>();
    let registry = WorkspaceRegistry::open(root.path().join("data"), &host, sessions).unwrap();
    let mut workspaces = vec![registry.list()[0].workspace_id.clone()];
    for index in 1..100 {
        let directory = root.dir(&format!("workspace-{index}"));
        workspaces.push(
            registry
                .create(directory, None)
                .unwrap()
                .workspace
                .workspace_id,
        );
    }
    for (index, session) in (0..1000)
        .map(|index| format!("session-{index}"))
        .enumerate()
    {
        registry
            .attach_session(&workspaces[index % workspaces.len()], &session, None)
            .unwrap();
    }
    assert_eq!(registry.list().len(), 100);
    assert_eq!(
        registry
            .list()
            .iter()
            .map(|workspace| workspace.session_ids.len())
            .sum::<usize>(),
        1000
    );
}

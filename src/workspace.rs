//! Durable multi-workspace registry and local workspace authority.
//!
//! The registry owns the on-disk workspace snapshot.  It deliberately has no
//! connection to Host or the browser protocol: callers must resolve a private
//! [`WorkspaceLease`] before using a workspace root.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{protocol::SessionId, session::SessionInspection};

pub const WORKSPACE_SCHEMA_VERSION: &str = "tessivum.workspaces/v1";
const REGISTRY_FILE: &str = "workspaces.json";

/// Opaque workspace identity.  The UUID is an implementation detail and is
/// never derived from a path.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct WorkspaceId(pub String);

impl WorkspaceId {
    pub fn random() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for WorkspaceId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<String> for WorkspaceId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for WorkspaceId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// The wire-stable workspace item.  Lease generation is intentionally absent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workspace {
    pub workspace_id: WorkspaceId,
    pub path: String,
    pub title: String,
    pub session_ids: Vec<SessionId>,
    pub created_at: String,
    pub updated_at: String,
}

/// The complete persisted registry document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub schema_version: String,
    pub revision: u64,
    pub items: Vec<Workspace>,
    pub archived_session_ids: Vec<SessionId>,
}

impl Default for WorkspaceSnapshot {
    fn default() -> Self {
        Self {
            schema_version: WORKSPACE_SCHEMA_VERSION.to_owned(),
            revision: 0,
            items: Vec::new(),
            archived_session_ids: Vec::new(),
        }
    }
}

/// Migration diagnostics intentionally contain no broken cwd.  Paths can be
/// secret, while a session id and a stable code are enough for local repair.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "code", deny_unknown_fields)]
pub enum WorkspaceDiagnostic {
    #[serde(rename = "missingSessionCwd")]
    MissingSessionCwd { session_id: SessionId },
    #[serde(rename = "invalidSessionCwd")]
    InvalidSessionCwd { session_id: SessionId },
    #[serde(rename = "unaccountedSession")]
    UnaccountedSession { session_id: SessionId },
}

impl WorkspaceDiagnostic {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingSessionCwd { .. } => "MISSING_SESSION_CWD",
            Self::InvalidSessionCwd { .. } => "INVALID_SESSION_CWD",
            Self::UnaccountedSession { .. } => "UNACCOUNTED_SESSION",
        }
    }

    pub fn session_id(&self) -> &SessionId {
        match self {
            Self::MissingSessionCwd { session_id }
            | Self::InvalidSessionCwd { session_id }
            | Self::UnaccountedSession { session_id } => session_id,
        }
    }
}

/// Result of create.  Existing canonical paths are returned without changing
/// the document and report `created == false`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceCreateResult {
    pub workspace: Workspace,
    pub created: bool,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum WorkspaceError {
    #[error("workspace registry I/O failed: {0}")]
    Io(String),
    #[error("workspace registry is corrupt: {0}")]
    Corrupt(String),
    #[error("unsupported workspace registry schema: {0}")]
    UnsupportedSchema(String),
    #[error("workspace path is not an existing readable directory: {0}")]
    InvalidPath(String),
    #[error("workspace id is not an opaque UUID: {0}")]
    InvalidId(String),
    #[error("workspace title must not be blank")]
    InvalidTitle,
    #[error("workspace {0} was not found")]
    NotFound(WorkspaceId),
    #[error("workspace path is already registered")]
    PathConflict,
    #[error("workspace title is already used by another workspace")]
    TitleConflict,
    #[error("workspace position reference is invalid")]
    InvalidPosition,
    #[error("session is not known to durable persistence: {0}")]
    UnknownSession(SessionId),
    #[error("session is not accounted by a workspace: {0}")]
    UnaccountedSession(SessionId),
    #[error("session belongs to another workspace: {0}")]
    SessionBelongsElsewhere(SessionId),
    #[error("workspace revision conflict: expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("workspace lease is stale")]
    StaleLease,
    #[error("workspace persistence failed: {0}")]
    Persistence(String),
}

impl WorkspaceError {
    pub fn code(&self) -> &str {
        match self {
            Self::Io(_) => "WORKSPACE_REGISTRY_IO",
            Self::Corrupt(_) => "WORKSPACE_REGISTRY_CORRUPT",
            Self::UnsupportedSchema(_) => "UNSUPPORTED_WORKSPACE_SCHEMA",
            Self::InvalidPath(_) => "INVALID_WORKSPACE_PATH",
            Self::InvalidId(_) => "INVALID_WORKSPACE_ID",
            Self::InvalidTitle => "INVALID_WORKSPACE_TITLE",
            Self::NotFound(_) => "WORKSPACE_NOT_FOUND",
            Self::PathConflict => "WORKSPACE_PATH_CONFLICT",
            Self::TitleConflict => "WORKSPACE_TITLE_CONFLICT",
            Self::InvalidPosition => "INVALID_WORKSPACE_POSITION",
            Self::UnknownSession(_) => "UNKNOWN_SESSION",
            Self::UnaccountedSession(_) => "UNACCOUNTED_SESSION",
            Self::SessionBelongsElsewhere(_) => "SESSION_BELONGS_TO_OTHER_WORKSPACE",
            Self::RevisionConflict { .. } => "WORKSPACE_REVISION_CONFLICT",
            Self::StaleLease => "STALE_WORKSPACE_LEASE",
            Self::Persistence(_) => "WORKSPACE_PERSISTENCE_FAILED",
        }
    }
}

struct RegistryState {
    snapshot: WorkspaceSnapshot,
    known_sessions: BTreeSet<SessionId>,
    generations: BTreeMap<WorkspaceId, u64>,
    next_generation: u64,
    diagnostics: Vec<WorkspaceDiagnostic>,
}

struct RegistryInner {
    file: PathBuf,
    state: Mutex<RegistryState>,
}

/// Durable registry handle.  A single process may clone this handle and race
/// mutations safely; each mutation holds the one small state lock through its
/// atomic file commit.
#[derive(Clone)]
pub struct WorkspaceRegistry {
    inner: Arc<RegistryInner>,
}

impl fmt::Debug for WorkspaceRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceRegistry")
            .field("file", &self.inner.file)
            .finish_non_exhaustive()
    }
}

/// Private authority handed to Host/resource code after an explicit resolve.
/// It is not serializable and has no wire representation.
pub struct WorkspaceLease {
    registry: Weak<RegistryInner>,
    workspace_id: WorkspaceId,
    generation: u64,
    canonical_root: PathBuf,
}

impl fmt::Debug for WorkspaceLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceLease")
            .field("workspace_id", &self.workspace_id)
            .field("canonical_root", &self.canonical_root)
            .finish_non_exhaustive()
    }
}

impl WorkspaceLease {
    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    /// Verifies registration, generation, and the filesystem root immediately before use.
    pub fn validate_current(&self) -> Result<PathBuf, WorkspaceError> {
        let Some(registry) = self.registry.upgrade() else {
            return Err(WorkspaceError::StaleLease);
        };
        let state = registry
            .state
            .lock()
            .map_err(|_| WorkspaceError::StaleLease)?;
        let Some(generation) = state.generations.get(&self.workspace_id) else {
            return Err(WorkspaceError::StaleLease);
        };
        let Some(workspace) = state
            .snapshot
            .items
            .iter()
            .find(|item| item.workspace_id == self.workspace_id)
        else {
            return Err(WorkspaceError::StaleLease);
        };
        if *generation != self.generation || workspace.path != path_string(&self.canonical_root) {
            return Err(WorkspaceError::StaleLease);
        }
        let current =
            canonical_directory(&self.canonical_root).map_err(|_| WorkspaceError::StaleLease)?;
        if current != self.canonical_root {
            return Err(WorkspaceError::StaleLease);
        }
        Ok(current)
    }
}

/// Resolves a durable session membership into a generation-checked resource root.
#[derive(Clone, Debug)]
pub struct SessionResourceResolver {
    registry: WorkspaceRegistry,
}

impl SessionResourceResolver {
    pub fn new(registry: WorkspaceRegistry) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &WorkspaceRegistry {
        &self.registry
    }

    /// Returns a lease after checking the current canonical directory.
    pub fn resolve(&self, session_id: impl AsRef<str>) -> Result<WorkspaceLease, WorkspaceError> {
        let session_id = SessionId::from(session_id.as_ref());
        let workspace = self
            .registry
            .workspace_for_session(&session_id)
            .ok_or_else(|| WorkspaceError::UnaccountedSession(session_id.clone()))?;
        let lease = self.registry.resolve(&workspace.workspace_id)?;
        lease.validate_current()?;
        Ok(lease)
    }

    pub fn resolve_root(&self, session_id: impl AsRef<str>) -> Result<PathBuf, WorkspaceError> {
        self.resolve(session_id)?.validate_current()
    }
}

impl WorkspaceRegistry {
    /// Opens the durable registry.  An absent file is migrated exactly once
    /// from durable session headers; a present file is parsed and validated,
    /// never regenerated.
    pub fn open<I>(
        data_dir: impl AsRef<Path>,
        host_cwd: impl AsRef<Path>,
        sessions: I,
    ) -> Result<Self, WorkspaceError>
    where
        I: IntoIterator<Item = SessionInspection>,
    {
        let data_dir = data_dir.as_ref();
        fs::create_dir_all(data_dir).map_err(|e| io_error("create data directory", e))?;
        cleanup_stale_temps(data_dir)?;
        let file = data_dir.join(REGISTRY_FILE);
        let file_present = match fs::symlink_metadata(&file) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    return Err(WorkspaceError::Corrupt(
                        "workspace registry is not a regular file".into(),
                    ));
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(io_error("inspect workspace registry", error)),
        };
        let host_cwd = canonical_directory(host_cwd.as_ref())?;
        let sessions: Vec<SessionInspection> = sessions.into_iter().collect();
        let known_sessions = sessions.iter().map(|s| s.header.id.clone()).collect();

        let snapshot = if file_present {
            let bytes = fs::read(&file).map_err(|e| io_error("read workspace registry", e))?;
            let snapshot: WorkspaceSnapshot = serde_json::from_slice(&bytes)
                .map_err(|e| WorkspaceError::Corrupt(e.to_string()))?;
            validate_snapshot(&snapshot).map_err(|error| match error {
                WorkspaceError::UnsupportedSchema(_) => error,
                other => WorkspaceError::Corrupt(other.to_string()),
            })?;
            snapshot
        } else {
            migrate(&host_cwd, &sessions)?
        };

        let mut generations = BTreeMap::new();
        let mut next_generation: u64 = 1;
        for workspace in &snapshot.items {
            generations.insert(workspace.workspace_id.clone(), next_generation);
            next_generation = next_generation.saturating_add(1).max(1);
        }
        let diagnostics = diagnostics_for(&snapshot, &sessions);
        let registry = Self {
            inner: Arc::new(RegistryInner {
                file,
                state: Mutex::new(RegistryState {
                    snapshot,
                    known_sessions,
                    generations,
                    next_generation,
                    diagnostics,
                }),
            }),
        };
        if !file_present {
            let state = registry
                .inner
                .state
                .lock()
                .map_err(|_| WorkspaceError::StaleLease)?;
            atomic_write(&registry.inner.file, &state.snapshot)?;
        }
        Ok(registry)
    }

    pub fn snapshot(&self) -> WorkspaceSnapshot {
        self.inner
            .state
            .lock()
            .expect("workspace registry lock poisoned")
            .snapshot
            .clone()
    }

    pub fn list(&self) -> Vec<Workspace> {
        self.snapshot().items
    }
    /// Adds a freshly persisted session to the in-memory known-session set.
    /// Membership remains ungrouped until `attach_session` commits it.
    pub fn recognize_session(&self, session_id: impl AsRef<str>) -> Result<(), WorkspaceError> {
        let session_id = SessionId::from(session_id.as_ref());
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| WorkspaceError::StaleLease)?;
        state.known_sessions.insert(session_id);
        Ok(())
    }

    pub fn revision(&self) -> u64 {
        self.inner
            .state
            .lock()
            .expect("workspace registry lock poisoned")
            .snapshot
            .revision
    }

    pub fn diagnostics(&self) -> Vec<WorkspaceDiagnostic> {
        self.inner
            .state
            .lock()
            .expect("workspace registry lock poisoned")
            .diagnostics
            .clone()
    }

    pub fn create(
        &self,
        path: impl AsRef<Path>,
        expected_revision: Option<u64>,
    ) -> Result<WorkspaceCreateResult, WorkspaceError> {
        let canonical = canonical_directory(path.as_ref())?;
        let path_string = path_string(&canonical);
        self.mutate(expected_revision, |snapshot, runtime| {
            if let Some(existing) = snapshot.items.iter().find(|w| w.path == path_string) {
                return Ok((
                    false,
                    WorkspaceCreateResult {
                        workspace: existing.clone(),
                        created: false,
                    },
                ));
            }
            let title = workspace_title(&canonical);
            let now = timestamp(None);
            let workspace = Workspace {
                workspace_id: WorkspaceId::random(),
                path: path_string,
                title,
                session_ids: Vec::new(),
                created_at: now.clone(),
                updated_at: now,
            };
            runtime
                .generations
                .insert(workspace.workspace_id.clone(), runtime.next_generation);
            runtime.next_generation = runtime.next_generation.saturating_add(1).max(1);
            snapshot.items.push(workspace.clone());
            Ok((
                true,
                WorkspaceCreateResult {
                    workspace,
                    created: true,
                },
            ))
        })
    }

    pub fn rename(
        &self,
        workspace_id: impl AsRef<str>,
        title: impl AsRef<str>,
        expected_revision: Option<u64>,
    ) -> Result<Workspace, WorkspaceError> {
        let id = WorkspaceId::from(workspace_id.as_ref());
        let title = title.as_ref().trim().to_owned();
        if title.is_empty() {
            return Err(WorkspaceError::InvalidTitle);
        }
        self.mutate(expected_revision, |snapshot, _| {
            let index = workspace_index(snapshot, &id)?;
            if snapshot.items[index].title == title {
                return Ok((false, snapshot.items[index].clone()));
            }
            ensure_unique_title(snapshot, &title, Some(&id))?;
            snapshot.items[index].title = title;
            snapshot.items[index].updated_at = timestamp(Some(&snapshot.items[index].updated_at));
            Ok((true, snapshot.items[index].clone()))
        })
    }

    pub fn delete(
        &self,
        workspace_id: impl AsRef<str>,
        expected_revision: Option<u64>,
    ) -> Result<bool, WorkspaceError> {
        let id = WorkspaceId::from(workspace_id.as_ref());
        self.mutate(expected_revision, |snapshot, runtime| {
            let index = snapshot
                .items
                .iter()
                .position(|workspace| workspace.workspace_id == id)
                .ok_or_else(|| WorkspaceError::NotFound(id.clone()))?;
            snapshot.items.remove(index);
            runtime.generations.remove(&id);
            Ok((true, true))
        })
    }

    /// DOM `insertBefore`: a null reference appends, and an item already in
    /// the requested position is a no-op.
    pub fn insert_before(
        &self,
        workspace_id: impl AsRef<str>,
        before_workspace_id: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<(), WorkspaceError> {
        let id = WorkspaceId::from(workspace_id.as_ref());
        let before = before_workspace_id.map(WorkspaceId::from);
        self.mutate(expected_revision, |snapshot, _| {
            let from = workspace_index(snapshot, &id)?;
            match before.as_ref() {
                None if from + 1 == snapshot.items.len() => Ok((false, ())),
                None => {
                    let item = snapshot.items.remove(from);
                    snapshot.items.push(item);
                    Ok((true, ()))
                }
                Some(before) if *before == id => Ok((false, ())),
                Some(before) => {
                    let target = snapshot
                        .items
                        .iter()
                        .position(|item| item.workspace_id == *before)
                        .ok_or(WorkspaceError::InvalidPosition)?;
                    if from + 1 == target {
                        return Ok((false, ()));
                    }
                    let item = snapshot.items.remove(from);
                    let target = snapshot
                        .items
                        .iter()
                        .position(|item| item.workspace_id == *before)
                        .ok_or(WorkspaceError::InvalidPosition)?;
                    snapshot.items.insert(target, item);
                    Ok((true, ()))
                }
            }
        })
    }

    pub fn attach_session(
        &self,
        workspace_id: impl AsRef<str>,
        session_id: impl AsRef<str>,
        expected_revision: Option<u64>,
    ) -> Result<(), WorkspaceError> {
        let id = WorkspaceId::from(workspace_id.as_ref());
        let session = SessionId::from(session_id.as_ref());
        self.mutate(expected_revision, |snapshot, runtime| {
            let index = workspace_index(snapshot, &id)?;
            ensure_known(runtime, &session)?;
            if let Some((owner, _)) = session_location(snapshot, &session) {
                if owner == id {
                    return Ok((false, ()));
                }
                return Err(WorkspaceError::SessionBelongsElsewhere(session.clone()));
            }
            snapshot.items[index].session_ids.insert(0, session);
            snapshot.items[index].updated_at = timestamp(Some(&snapshot.items[index].updated_at));
            Ok((true, ()))
        })
    }

    /// DOM `insertBefore` for manual session order.  A known ungrouped
    /// session is attached as part of insertion; a null reference appends.
    pub fn insert_session_before(
        &self,
        workspace_id: impl AsRef<str>,
        session_id: impl AsRef<str>,
        before_session_id: Option<&str>,
        expected_revision: Option<u64>,
    ) -> Result<(), WorkspaceError> {
        let id = WorkspaceId::from(workspace_id.as_ref());
        let session = SessionId::from(session_id.as_ref());
        let before = before_session_id.map(SessionId::from);
        self.mutate(expected_revision, |snapshot, runtime| {
            let index = workspace_index(snapshot, &id)?;
            ensure_known(runtime, &session)?;
            let Some((owner, _)) = session_location(snapshot, &session) else {
                return Err(WorkspaceError::UnaccountedSession(session.clone()));
            };
            if owner != id {
                return Err(WorkspaceError::SessionBelongsElsewhere(session.clone()));
            }
            let current = snapshot.items[index]
                .session_ids
                .iter()
                .position(|candidate| candidate == &session);
            match before.as_ref() {
                Some(before) if *before == session => return Ok((false, ())),
                Some(before) => {
                    let target = snapshot.items[index]
                        .session_ids
                        .iter()
                        .position(|candidate| candidate == before)
                        .ok_or(WorkspaceError::InvalidPosition)?;
                    if current.is_some_and(|from| from + 1 == target) {
                        return Ok((false, ()));
                    }
                    if let Some(from) = current {
                        snapshot.items[index].session_ids.remove(from);
                    }
                    let target = snapshot.items[index]
                        .session_ids
                        .iter()
                        .position(|candidate| candidate == before)
                        .ok_or(WorkspaceError::InvalidPosition)?;
                    snapshot.items[index].session_ids.insert(target, session);
                }
                None if current
                    .is_some_and(|from| from + 1 == snapshot.items[index].session_ids.len()) =>
                {
                    return Ok((false, ()));
                }
                None => {
                    if let Some(from) = current {
                        snapshot.items[index].session_ids.remove(from);
                    }
                    snapshot.items[index].session_ids.push(session);
                }
            }
            snapshot.items[index].updated_at = timestamp(Some(&snapshot.items[index].updated_at));
            Ok((true, ()))
        })
    }

    pub fn archive_session(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: Option<u64>,
    ) -> Result<(), WorkspaceError> {
        let session = SessionId::from(session_id.as_ref());
        self.mutate(expected_revision, |snapshot, runtime| {
            ensure_known(runtime, &session)?;
            if snapshot.archived_session_ids.contains(&session) {
                return Ok((false, ()));
            }
            if session_location(snapshot, &session).is_none() {
                return Err(WorkspaceError::UnaccountedSession(session.clone()));
            }
            snapshot.archived_session_ids.push(session);
            Ok((true, ()))
        })
    }

    pub fn workspace_for_session(&self, session_id: impl AsRef<str>) -> Option<Workspace> {
        let session = SessionId::from(session_id.as_ref());
        self.inner.state.lock().ok().and_then(|state| {
            session_location(&state.snapshot, &session)
                .and_then(|(_, index)| state.snapshot.items.get(index).cloned())
        })
    }

    pub fn resolve(&self, workspace_id: impl AsRef<str>) -> Result<WorkspaceLease, WorkspaceError> {
        let id = WorkspaceId::from(workspace_id.as_ref());
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| WorkspaceError::StaleLease)?;
        let workspace = state
            .snapshot
            .items
            .iter()
            .find(|item| item.workspace_id == id)
            .ok_or_else(|| WorkspaceError::NotFound(id.clone()))?;
        let generation = *state
            .generations
            .get(&id)
            .ok_or(WorkspaceError::StaleLease)?;
        Ok(WorkspaceLease {
            registry: Arc::downgrade(&self.inner),
            workspace_id: id,
            generation,
            canonical_root: PathBuf::from(&workspace.path),
        })
    }

    fn mutate<T, F>(&self, expected_revision: Option<u64>, change: F) -> Result<T, WorkspaceError>
    where
        F: FnOnce(&mut WorkspaceSnapshot, &mut RegistryState) -> Result<(bool, T), WorkspaceError>,
    {
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| WorkspaceError::StaleLease)?;
        let actual = state.snapshot.revision;
        if let Some(expected) = expected_revision {
            if expected != actual {
                return Err(WorkspaceError::RevisionConflict { expected, actual });
            }
        }
        let mut candidate = state.snapshot.clone();
        let mut candidate_runtime = RegistryState {
            snapshot: candidate.clone(),
            known_sessions: state.known_sessions.clone(),
            generations: state.generations.clone(),
            next_generation: state.next_generation,
            diagnostics: state.diagnostics.clone(),
        };
        let (changed, result) = change(&mut candidate, &mut candidate_runtime)?;
        if !changed {
            return Ok(result);
        }
        candidate.revision = actual
            .checked_add(1)
            .ok_or_else(|| WorkspaceError::Persistence("revision exhausted".into()))?;
        validate_snapshot(&candidate)?;
        atomic_write(&self.inner.file, &candidate)?;
        state.snapshot = candidate;
        state.generations = candidate_runtime.generations;
        state.next_generation = candidate_runtime.next_generation;
        Ok(result)
    }
}

fn ensure_known(runtime: &RegistryState, session: &SessionId) -> Result<(), WorkspaceError> {
    if runtime.known_sessions.contains(session) {
        Ok(())
    } else {
        Err(WorkspaceError::UnknownSession(session.clone()))
    }
}

fn workspace_index(
    snapshot: &WorkspaceSnapshot,
    id: &WorkspaceId,
) -> Result<usize, WorkspaceError> {
    snapshot
        .items
        .iter()
        .position(|item| &item.workspace_id == id)
        .ok_or_else(|| WorkspaceError::NotFound(id.clone()))
}

fn session_location(
    snapshot: &WorkspaceSnapshot,
    session: &SessionId,
) -> Option<(WorkspaceId, usize)> {
    snapshot
        .items
        .iter()
        .enumerate()
        .find_map(|(index, workspace)| {
            workspace
                .session_ids
                .iter()
                .any(|candidate| candidate == session)
                .then(|| (workspace.workspace_id.clone(), index))
        })
}

fn ensure_unique_title(
    snapshot: &WorkspaceSnapshot,
    title: &str,
    except: Option<&WorkspaceId>,
) -> Result<(), WorkspaceError> {
    if snapshot
        .items
        .iter()
        .any(|item| item.title == title && except.is_none_or(|id| item.workspace_id != *id))
    {
        Err(WorkspaceError::TitleConflict)
    } else {
        Ok(())
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf, WorkspaceError> {
    let canonical = fs::canonicalize(path)
        .map_err(|e| WorkspaceError::InvalidPath(format!("{}: {e}", path.display())))?;
    let metadata = fs::metadata(&canonical)
        .map_err(|e| WorkspaceError::InvalidPath(format!("{}: {e}", canonical.display())))?;
    if !metadata.is_dir() || fs::read_dir(&canonical).is_err() {
        return Err(WorkspaceError::InvalidPath(canonical.display().to_string()));
    }
    Ok(canonical)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn workspace_title(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path_string(path))
}

fn timestamp(previous: Option<&str>) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let now = format_unix_millis(millis);
    previous
        .filter(|value| *value >= now.as_str())
        .map_or(now, |value| (*value).to_owned())
}

fn format_unix_millis(millis: i64) -> String {
    let seconds = millis / 1_000;
    let milliseconds = millis % 1_000;
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}Z")
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn migrate(
    host_cwd: &Path,
    sessions: &[SessionInspection],
) -> Result<WorkspaceSnapshot, WorkspaceError> {
    let mut snapshot = WorkspaceSnapshot {
        schema_version: WORKSPACE_SCHEMA_VERSION.to_owned(),
        revision: 0,
        items: Vec::new(),
        archived_session_ids: Vec::new(),
    };
    let now = timestamp(None);
    snapshot.items.push(Workspace {
        workspace_id: WorkspaceId::random(),
        path: path_string(host_cwd),
        title: workspace_title(host_cwd),
        session_ids: Vec::new(),
        created_at: now.clone(),
        updated_at: now,
    });
    for inspection in sessions {
        let session = inspection.header.id.clone();
        let Some(cwd) = inspection.header.cwd.as_deref() else {
            continue;
        };
        let Ok(canonical) = canonical_directory(Path::new(cwd)) else {
            continue;
        };
        let path = path_string(&canonical);
        let index = if let Some(index) = snapshot.items.iter().position(|item| item.path == path) {
            index
        } else {
            let now = timestamp(None);
            snapshot.items.push(Workspace {
                workspace_id: WorkspaceId::random(),
                path,
                title: workspace_title(&canonical),
                session_ids: Vec::new(),
                created_at: now.clone(),
                updated_at: now,
            });
            snapshot.items.len() - 1
        };
        if !snapshot.items[index].session_ids.contains(&session) {
            snapshot.items[index].session_ids.push(session);
        }
    }
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn diagnostics_for(
    snapshot: &WorkspaceSnapshot,
    sessions: &[SessionInspection],
) -> Vec<WorkspaceDiagnostic> {
    let mut diagnostics = Vec::new();
    for inspection in sessions {
        let session = inspection.header.id.clone();
        match inspection.header.cwd.as_deref() {
            None => diagnostics.push(WorkspaceDiagnostic::MissingSessionCwd {
                session_id: session,
            }),
            Some(cwd) => {
                let valid = canonical_directory(Path::new(cwd)).ok();
                if valid.is_none() {
                    diagnostics.push(WorkspaceDiagnostic::InvalidSessionCwd {
                        session_id: session.clone(),
                    });
                } else if !snapshot
                    .items
                    .iter()
                    .any(|item| item.session_ids.contains(&session))
                {
                    diagnostics.push(WorkspaceDiagnostic::UnaccountedSession {
                        session_id: session,
                    });
                }
            }
        }
    }
    diagnostics
}

fn validate_snapshot(snapshot: &WorkspaceSnapshot) -> Result<(), WorkspaceError> {
    if snapshot.schema_version != WORKSPACE_SCHEMA_VERSION {
        return Err(WorkspaceError::UnsupportedSchema(
            snapshot.schema_version.clone(),
        ));
    }
    let mut paths = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut sessions = BTreeSet::new();
    for item in &snapshot.items {
        if Uuid::parse_str(item.workspace_id.as_str()).is_err() {
            return Err(WorkspaceError::InvalidId(item.workspace_id.to_string()));
        }
        if !ids.insert(item.workspace_id.clone()) {
            return Err(WorkspaceError::Corrupt("duplicate workspace id".into()));
        }
        let canonical = canonical_directory(Path::new(&item.path))?;
        if path_string(&canonical) != item.path || !paths.insert(item.path.clone()) {
            return Err(WorkspaceError::Corrupt(
                "workspace paths must be unique canonical directories".into(),
            ));
        }
        if item.title.trim().is_empty() {
            return Err(WorkspaceError::InvalidTitle);
        }
        if item
            .session_ids
            .iter()
            .any(|session| session.as_str().trim().is_empty())
        {
            return Err(WorkspaceError::Corrupt(
                "session id must not be blank".into(),
            ));
        }
        for session in &item.session_ids {
            if !sessions.insert(session.clone()) {
                return Err(WorkspaceError::Corrupt(
                    "session accounting is not unique".into(),
                ));
            }
        }
    }
    let mut archived = BTreeSet::new();
    for session in &snapshot.archived_session_ids {
        if session.as_str().trim().is_empty() {
            return Err(WorkspaceError::Corrupt(
                "archived session id must not be blank".into(),
            ));
        }
        if !archived.insert(session.clone()) {
            return Err(WorkspaceError::Corrupt(
                "archived session accounting is not unique".into(),
            ));
        }
    }
    Ok(())
}

fn cleanup_stale_temps(data_dir: &Path) -> Result<(), WorkspaceError> {
    let entries = fs::read_dir(data_dir).map_err(|e| io_error("scan workspace directory", e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_error("scan workspace temporary", e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(".workspaces.json-") || !name.ends_with(".tmp") {
            continue;
        }
        let token = &name[".workspaces.json-".len()..name.len() - ".tmp".len()];
        if Uuid::parse_str(token).is_err() {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|e| io_error("inspect workspace temporary", e))?;
        if metadata.file_type().is_file() {
            fs::remove_file(entry.path()).map_err(|e| io_error("remove workspace temporary", e))?;
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, snapshot: &WorkspaceSnapshot) -> Result<(), WorkspaceError> {
    let bytes = serde_json::to_vec_pretty(snapshot)
        .map_err(|e| WorkspaceError::Persistence(e.to_string()))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| io_error("create workspace directory", e))?;
    let temporary = parent.join(format!(".workspaces.json-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|e| io_error("create workspace temporary", e))?;
        file.write_all(&bytes)
            .map_err(|e| io_error("write workspace temporary", e))?;
        file.sync_all()
            .map_err(|e| io_error("sync workspace temporary", e))?;
        fs::rename(&temporary, path).map_err(|e| io_error("rename workspace temporary", e))?;
        let directory = File::open(parent).map_err(|e| io_error("open workspace directory", e))?;
        directory
            .sync_all()
            .map_err(|e| io_error("sync workspace directory", e))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn io_error(operation: &str, error: std::io::Error) -> WorkspaceError {
    WorkspaceError::Io(format!("{operation}: {error}"))
}

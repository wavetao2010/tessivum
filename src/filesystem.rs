//! Confined, versioned filesystem effects with durable atomic publication.

use std::{
    collections::HashMap,
    fmt,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tessivum_core::{ContextHandle, CoreError, ServiceHandle, ServiceKey};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::Mutex as AsyncMutex,
};
use uuid::Uuid;

use crate::TessivumError;

/// Stable key for the filesystem capability.
pub fn filesystem_service_key() -> ServiceKey {
    ServiceKey::new("harness.filesystem", "1")
}

/// A filesystem location created by a [`Filesystem`] capability.
///
/// Its path is deliberately private: callers cannot turn an arbitrary native
/// path into a target accepted by this capability.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FsTarget {
    path: PathBuf,
}

impl FsTarget {
    /// Returns the capability-local spelling of this target for diagnostics.
    pub fn display(&self) -> String {
        self.path.display().to_string()
    }
}

/// An opaque observation token used for optimistic replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsVersion([u8; 32]);

impl FsVersion {
    /// Renders the opaque token for persistence or transport.
    pub fn as_str(&self) -> String {
        let mut rendered = String::with_capacity(self.0.len() * 2);
        for byte in self.0 {
            use std::fmt::Write;
            let _ = write!(rendered, "{byte:02x}");
        }
        rendered
    }
}

/// The no-follow node class returned by [`Filesystem::lstat`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FsNodeKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// A no-follow filesystem observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsMetadata {
    pub kind: FsNodeKind,
    pub len: u64,
    pub version: FsVersion,
}

/// A canonical, regular-file observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsObservation {
    pub target: FsTarget,
    pub len: u64,
    pub version: FsVersion,
}

/// Guard applied to an atomic text write.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FsWriteGuard {
    /// Publish only when no file is observed at the target.
    pub create_if_absent: bool,
    /// Publish only if the current opaque token is exactly this one.
    pub replace_if_version: Option<FsVersion>,
}

/// A literal, exactly-once text edit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsLiteralEdit {
    pub expected: String,
    pub replacement: String,
    pub replace_if_version: Option<FsVersion>,
}

impl FsLiteralEdit {
    pub fn new(expected: impl Into<String>, replacement: impl Into<String>) -> Self {
        Self {
            expected: expected.into(),
            replacement: replacement.into(),
            replace_if_version: None,
        }
    }
}

/// A root-confined filesystem capability.
#[derive(Clone)]
pub struct Filesystem {
    root: PathBuf,
    gates: Arc<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>>,
}

impl fmt::Debug for Filesystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Filesystem")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Filesystem {
    /// Creates a capability confined to `root`. The root must exist before an
    /// effect is performed; this avoids silently creating a caller-controlled
    /// sandbox root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(root)
        };
        Self {
            root,
            gates: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Publishes this capability under [`filesystem_service_key`].
    pub fn publish(&self, context: &ContextHandle) -> Result<ServiceHandle<Filesystem>, CoreError> {
        context.provide(filesystem_service_key(), self.clone())
    }

    /// Returns the unnormalized configured root for diagnostics.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Converts an untrusted root-relative path to an opaque target.
    pub fn target(&self, path: impl AsRef<Path>) -> Result<FsTarget, TessivumError> {
        let path = path.as_ref();
        if path.as_os_str().is_empty()
            || path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(fs_error(
                "FS_SANDBOX_DENIED",
                "filesystem target must be a non-empty root-relative path",
                json!({"path": path.display().to_string()}),
            ));
        }
        Ok(FsTarget {
            path: self.root.join(path),
        })
    }

    /// Resolves a target through all symlinks and returns its canonical target.
    pub async fn resolve(&self, target: &FsTarget) -> Result<FsTarget, TessivumError> {
        let path = fs::canonicalize(&target.path)
            .await
            .map_err(|error| io_error("resolve", &target.path, error))?;
        self.ensure_contained(&path).await?;
        Ok(FsTarget { path })
    }

    /// Returns whether `path` canonically resides in this capability's root.
    /// Nonexistent and inaccessible paths are not contained.
    pub async fn contains(&self, path: impl AsRef<Path>) -> bool {
        let Ok(path) = fs::canonicalize(path.as_ref()).await else {
            return false;
        };
        self.ensure_contained(&path).await.is_ok()
    }

    /// Returns a validated native path for a subprocess `cwd` or executable
    /// argument. Existing symlinks are followed before publication.
    pub async fn process_path(&self, target: &FsTarget) -> Result<PathBuf, TessivumError> {
        Ok(self.resolve(target).await?.path)
    }

    /// Observes the target itself without following its final symlink.
    pub async fn lstat(&self, target: &FsTarget) -> Result<FsMetadata, TessivumError> {
        self.write_path(target).await?;
        let metadata = fs::symlink_metadata(&target.path)
            .await
            .map_err(|error| io_error("lstat", &target.path, error))?;
        let kind = if metadata.file_type().is_symlink() {
            FsNodeKind::Symlink
        } else if metadata.is_file() {
            FsNodeKind::File
        } else if metadata.is_dir() {
            FsNodeKind::Directory
        } else {
            FsNodeKind::Other
        };
        Ok(FsMetadata {
            kind,
            len: metadata.len(),
            version: version_for_metadata(&metadata, None),
        })
    }

    /// Follows symlinks and observes a regular file.
    pub async fn observe(&self, target: &FsTarget) -> Result<FsObservation, TessivumError> {
        let target = self.resolve(target).await?;
        let metadata = fs::metadata(&target.path)
            .await
            .map_err(|error| io_error("observe", &target.path, error))?;
        if !metadata.is_file() {
            return Err(fs_error(
                "FS_NOT_REGULAR_FILE",
                "filesystem target is not a regular file",
                json!({"path": target.display()}),
            ));
        }
        let bytes = fs::read(&target.path)
            .await
            .map_err(|error| io_error("read observed file", &target.path, error))?;
        Ok(FsObservation {
            target,
            len: metadata.len(),
            version: version_for_metadata(&metadata, Some(&bytes)),
        })
    }

    /// Reads at most `max_bytes` bytes from a regular file after following its
    /// final symlink.
    pub async fn read_bytes(
        &self,
        target: &FsTarget,
        max_bytes: usize,
    ) -> Result<Vec<u8>, TessivumError> {
        let target = self.resolve(target).await?;
        let metadata = fs::metadata(&target.path)
            .await
            .map_err(|error| io_error("stat before read", &target.path, error))?;
        ensure_regular(&target, &metadata)?;
        ensure_bound(metadata.len(), max_bytes, &target)?;
        let bytes = fs::read(&target.path)
            .await
            .map_err(|error| io_error("read", &target.path, error))?;
        ensure_bound(bytes.len() as u64, max_bytes, &target)?;
        Ok(bytes)
    }

    /// Reads a bounded UTF-8 text file and normalizes line endings to LF.
    pub async fn read_text(
        &self,
        target: &FsTarget,
        max_bytes: usize,
    ) -> Result<String, TessivumError> {
        let bytes = self.read_bytes(target, max_bytes).await?;
        let text = String::from_utf8(bytes).map_err(|_| {
            fs_error(
                "FS_NOT_TEXT",
                "filesystem target does not contain UTF-8 text",
                json!({"path": target.display()}),
            )
        })?;
        Ok(normalize_lf(&text))
    }

    /// Lists no more than `max_entries` direct children in deterministic name
    /// order. Entries are returned as opaque targets and are not resolved.
    pub async fn list(
        &self,
        target: &FsTarget,
        max_entries: usize,
    ) -> Result<Vec<FsTarget>, TessivumError> {
        let target = self.resolve(target).await?;
        let metadata = fs::metadata(&target.path)
            .await
            .map_err(|error| io_error("stat before list", &target.path, error))?;
        if !metadata.is_dir() {
            return Err(fs_error(
                "FS_NOT_DIRECTORY",
                "filesystem target is not a directory",
                json!({"path": target.display()}),
            ));
        }
        let mut entries = fs::read_dir(&target.path)
            .await
            .map_err(|error| io_error("list", &target.path, error))?;
        let mut paths = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| io_error("read directory entry", &target.path, error))?
        {
            if paths.len() == max_entries {
                return Err(fs_error(
                    "FS_TOO_LARGE",
                    "directory exceeds the requested entry limit",
                    json!({"path": target.display(), "maxEntries": max_entries}),
                ));
            }
            paths.push(FsTarget { path: entry.path() });
        }
        paths.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(paths)
    }

    /// Atomically writes LF-normalized text in the target's directory.
    pub async fn write_text(
        &self,
        target: &FsTarget,
        text: impl AsRef<str>,
        guard: FsWriteGuard,
    ) -> Result<FsObservation, TessivumError> {
        if guard.create_if_absent && guard.replace_if_version.is_some() {
            return Err(fs_error(
                "FS_IO_ERROR",
                "create_if_absent and replace_if_version cannot be combined",
                json!({"path": target.display()}),
            ));
        }
        let path = self.write_path(target).await?;
        let gate = self.gate(&path);
        let _guard = gate.lock().await;
        let existing = self.current_version(&path).await?;
        match (&guard.replace_if_version, existing.as_ref()) {
            (Some(_), None) => {
                return Err(fs_error(
                    "FS_STALE_VERSION",
                    "filesystem target is no longer observed",
                    json!({"path": path.display().to_string()}),
                ));
            }
            (Some(expected), Some(actual)) if expected != actual => {
                return Err(stale_error(&path, expected, actual));
            }
            _ => {}
        }
        if guard.create_if_absent && existing.is_some() {
            return Err(fs_error(
                "FS_NOT_OBSERVED",
                "filesystem target already exists",
                json!({"path": path.display().to_string()}),
            ));
        }
        let bytes = normalize_lf(text.as_ref()).into_bytes();
        atomic_publish(&path, &bytes, guard.create_if_absent).await?;
        self.observe_path(path).await
    }

    /// Applies one literal, exactly-once edit while holding the same target
    /// gate used by writes. Version staleness is intentionally checked before
    /// searching for the literal.
    pub async fn edit_text(
        &self,
        target: &FsTarget,
        edit: FsLiteralEdit,
    ) -> Result<FsObservation, TessivumError> {
        let path = self.write_path(target).await?;
        let gate = self.gate(&path);
        let _guard = gate.lock().await;
        let Some(version) = self.current_version(&path).await? else {
            return Err(fs_error(
                "FS_STALE_VERSION",
                "filesystem target is no longer observed",
                json!({"path": path.display().to_string()}),
            ));
        };
        if let Some(expected) = &edit.replace_if_version {
            if expected != &version {
                return Err(stale_error(&path, expected, &version));
            }
        }
        let bytes = fs::read(&path)
            .await
            .map_err(|error| io_error("read before edit", &path, error))?;
        let text = String::from_utf8(bytes).map_err(|_| {
            fs_error(
                "FS_NOT_TEXT",
                "filesystem target does not contain UTF-8 text",
                json!({"path": path.display().to_string()}),
            )
        })?;
        let text = normalize_lf(&text);
        let expected = normalize_lf(&edit.expected);
        let replacement = normalize_lf(&edit.replacement);
        if expected.is_empty() {
            return Err(fs_error(
                "FS_AMBIGUOUS_EDIT",
                "literal edit text must not be empty",
                json!({"path": path.display().to_string()}),
            ));
        }
        let matches = text.match_indices(&expected).count();
        if matches == 0 {
            return Err(fs_error(
                "FS_EDIT_NOT_FOUND",
                "literal edit text was not found",
                json!({"path": path.display().to_string()}),
            ));
        }
        if matches != 1 {
            return Err(fs_error(
                "FS_AMBIGUOUS_EDIT",
                "literal edit text matched more than once",
                json!({"path": path.display().to_string(), "matches": matches}),
            ));
        }
        let edited = text.replacen(&expected, &replacement, 1);
        atomic_publish(&path, edited.as_bytes(), false).await?;
        self.observe_path(path).await
    }

    fn gate(&self, path: &Path) -> Arc<AsyncMutex<()>> {
        let mut gates = lock(&self.gates);
        gates
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    async fn current_version(&self, path: &Path) -> Result<Option<FsVersion>, TessivumError> {
        let metadata = match fs::metadata(path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("stat write target", path, error)),
        };
        if !metadata.is_file() {
            return Err(fs_error(
                "FS_NOT_REGULAR_FILE",
                "filesystem target is not a regular file",
                json!({"path": path.display().to_string()}),
            ));
        }
        let bytes = fs::read(path)
            .await
            .map_err(|error| io_error("read write target", path, error))?;
        Ok(Some(version_for_metadata(&metadata, Some(&bytes))))
    }

    async fn observe_path(&self, path: PathBuf) -> Result<FsObservation, TessivumError> {
        let metadata = fs::metadata(&path)
            .await
            .map_err(|error| io_error("observe written file", &path, error))?;
        let bytes = fs::read(&path)
            .await
            .map_err(|error| io_error("read written file", &path, error))?;
        Ok(FsObservation {
            target: FsTarget { path },
            len: metadata.len(),
            version: version_for_metadata(&metadata, Some(&bytes)),
        })
    }

    /// Gives a write target an existing canonical parent and follows an
    /// existing final symlink. This blocks escapes through either route.
    async fn write_path(&self, target: &FsTarget) -> Result<PathBuf, TessivumError> {
        if !target.path.starts_with(&self.root) {
            return Err(fs_error(
                "FS_SANDBOX_DENIED",
                "filesystem target belongs to another capability root",
                json!({"path": target.display()}),
            ));
        }
        match fs::canonicalize(&target.path).await {
            Ok(path) => {
                self.ensure_contained(&path).await?;
                Ok(path)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = target.path.parent().ok_or_else(|| {
                    fs_error(
                        "FS_SANDBOX_DENIED",
                        "filesystem target has no parent directory",
                        json!({"path": target.display()}),
                    )
                })?;
                let parent = fs::canonicalize(parent)
                    .await
                    .map_err(|error| io_error("resolve write parent", parent, error))?;
                self.ensure_contained(&parent).await?;
                let name = target.path.file_name().ok_or_else(|| {
                    fs_error(
                        "FS_SANDBOX_DENIED",
                        "filesystem target has no file name",
                        json!({"path": target.display()}),
                    )
                })?;
                Ok(parent.join(name))
            }
            Err(error) => Err(io_error("resolve write target", &target.path, error)),
        }
    }

    async fn ensure_contained(&self, path: &Path) -> Result<(), TessivumError> {
        let root = fs::canonicalize(&self.root)
            .await
            .map_err(|error| io_error("resolve filesystem root", &self.root, error))?;
        if path.starts_with(&root) {
            Ok(())
        } else {
            Err(fs_error(
                "FS_SANDBOX_DENIED",
                "filesystem target resolves outside the capability root",
                json!({"path": path.display().to_string(), "root": root.display().to_string()}),
            ))
        }
    }
}

fn ensure_regular(target: &FsTarget, metadata: &std::fs::Metadata) -> Result<(), TessivumError> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(fs_error(
            "FS_NOT_REGULAR_FILE",
            "filesystem target is not a regular file",
            json!({"path": target.display()}),
        ))
    }
}

fn ensure_bound(len: u64, max: usize, target: &FsTarget) -> Result<(), TessivumError> {
    if len <= max as u64 {
        Ok(())
    } else {
        Err(fs_error(
            "FS_TOO_LARGE",
            "filesystem target exceeds the requested byte limit",
            json!({"path": target.display(), "maxBytes": max, "actualBytes": len}),
        ))
    }
}

fn version_for_metadata(metadata: &std::fs::Metadata, bytes: Option<&[u8]>) -> FsVersion {
    let mut hash = Sha256::new();
    hash.update(metadata.len().to_le_bytes());
    if let Ok(modified) = metadata.modified() {
        if let Ok(duration) = modified.duration_since(UNIX_EPOCH) {
            hash.update(duration.as_secs().to_le_bytes());
            hash.update(duration.subsec_nanos().to_le_bytes());
        }
    }
    if let Some(bytes) = bytes {
        hash.update(bytes);
    }
    let mut version = [0; 32];
    version.copy_from_slice(&hash.finalize());
    FsVersion(version)
}

async fn atomic_publish(
    path: &Path,
    bytes: &[u8],
    create_if_absent: bool,
) -> Result<(), TessivumError> {
    let parent = path.parent().ok_or_else(|| {
        fs_error(
            "FS_IO_ERROR",
            "filesystem target has no parent directory",
            json!({"path": path.display().to_string()}),
        )
    })?;
    let temp = parent.join(format!(
        ".tessivum-{}-{}.tmp",
        path.file_name().and_then(|v| v.to_str()).unwrap_or("file"),
        Uuid::new_v4()
    ));
    let result = async {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
            .map_err(|error| io_error("create atomic temporary file", &temp, error))?;
        file.write_all(bytes)
            .await
            .map_err(|error| io_error("write atomic temporary file", &temp, error))?;
        file.sync_data()
            .await
            .map_err(|error| io_error("sync atomic temporary file", &temp, error))?;
        drop(file);
        if create_if_absent {
            fs::hard_link(&temp, path).await.map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    fs_error(
                        "FS_NOT_OBSERVED",
                        "filesystem target already exists",
                        json!({"path": path.display().to_string()}),
                    )
                } else {
                    io_error("publish newly observed file", path, error)
                }
            })?;
            fs::remove_file(&temp)
                .await
                .map_err(|error| io_error("remove atomic temporary file", &temp, error))?;
        } else {
            fs::rename(&temp, path)
                .await
                .map_err(|error| io_error("publish atomic file", path, error))?;
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temp).await;
    }
    result
}

fn normalize_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn stale_error(path: &Path, expected: &FsVersion, actual: &FsVersion) -> TessivumError {
    fs_error(
        "FS_STALE_VERSION",
        "filesystem target version changed",
        json!({
            "path": path.display().to_string(),
            "expected": expected.as_str(),
            "actual": actual.as_str(),
        }),
    )
}

fn io_error(operation: &str, path: &Path, error: std::io::Error) -> TessivumError {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => "FS_NOT_FOUND",
        std::io::ErrorKind::PermissionDenied => "FS_PERMISSION_DENIED",
        _ => "FS_IO_ERROR",
    };
    fs_error(
        code,
        "filesystem operation failed",
        json!({"operation": operation, "path": path.display().to_string(), "error": error.to_string()}),
    )
}

fn fs_error(code: &str, message: &str, details: serde_json::Value) -> TessivumError {
    TessivumError::new(code, message, "filesystem", details)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

use axum::{
    body::Body,
    http::{header, Method, Response, StatusCode},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};
use thiserror::Error;
use tokio::sync::broadcast;

const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_BUNDLE_BYTES: usize = 32 * 1024 * 1024;
const MAX_STATIC_BYTES: usize = 32 * 1024 * 1024;
const MAX_PACKAGE_ENTRIES: usize = 4096;
const MAX_PACKAGE_DEPTH: usize = 32;
const MAX_PACKAGES: usize = 1024;
const MAX_GRAPH_BYTES: usize = 1024 * 1024;
const MAX_STRING_BYTES: usize = 1024;
const MAX_INJECT: usize = 128;
const MAX_HMR_QUEUE: usize = 64;

/// One browser plugin published by the host. `url` is always host-owned.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebBootEntry {
    pub id: String,
    pub url: String,
    pub rev: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub immediately: Option<bool>,
}

/// The complete, deterministic browser boot plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebBootGraph {
    pub rev: String,
    pub entries: Vec<WebBootEntry>,
}

impl Default for WebBootGraph {
    fn default() -> Self {
        Self {
            rev: graph_rev(&[]),
            entries: Vec::new(),
        }
    }
}

/// A bounded development-only graph revision notification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebHmrUpdate {
    pub graph: WebBootGraph,
}

/// A deterministic HTML contribution. Lower orders run first; IDs break ties.
pub struct FrontendHtmlTap {
    pub id: String,
    pub order: i64,
    render: Arc<dyn Fn(String) -> String + Send + Sync + 'static>,
}

impl FrontendHtmlTap {
    pub fn new(
        id: impl Into<String>,
        order: i64,
        render: impl Fn(String) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            order,
            render: Arc::new(render),
        }
    }
}

impl std::fmt::Debug for FrontendHtmlTap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrontendHtmlTap")
            .field("id", &self.id)
            .field("order", &self.order)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontendManifestError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum FrontendError {
    #[error("frontend {kind} root {path} is not a readable directory: {reason}")]
    InvalidRoot {
        kind: &'static str,
        path: PathBuf,
        reason: String,
    },
    #[error("frontend index {path} is not a regular file inside the distribution root")]
    InvalidIndex { path: PathBuf },
    #[error("frontend activation found invalid package manifests: {errors:?}")]
    InvalidPackageManifests { errors: Vec<FrontendManifestError> },
    #[error("frontend boot graph exceeds its {MAX_GRAPH_BYTES}-byte bound")]
    GraphTooLarge,
    #[error("frontend HMR queue capacity must be in 1..={MAX_HMR_QUEUE}")]
    InvalidHmrQueue,
    #[error("frontend HTML tap id is invalid: {0:?}")]
    InvalidTapId(String),
    #[error("frontend HTML tap is already registered: {0:?}")]
    DuplicateTapId(String),
    #[error("frontend index is missing a <head> element")]
    MissingHead,
    #[error("could not render frontend index: {0}")]
    RenderIndex(String),
}

#[derive(Clone)]
pub struct FrontendStatic {
    inner: Arc<FrontendInner>,
}

impl std::fmt::Debug for FrontendStatic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrontendStatic")
            .field("dist_root", &self.inner.dist_root)
            .field("graph", &self.graph())
            .finish_non_exhaustive()
    }
}

struct FrontendInner {
    dist_root: PathBuf,
    index_path: PathBuf,
    state: Mutex<FrontendState>,
    taps: Mutex<BTreeMap<String, RegisteredTap>>,
    hmr: Option<broadcast::Sender<WebHmrUpdate>>,
}

#[derive(Default)]
struct FrontendState {
    package_roots: Vec<PathBuf>,
    graph: WebBootGraph,
    bundles: BTreeMap<String, Bundle>,
    next_tap_token: u64,
}

#[derive(Clone)]
struct Bundle {
    root: PathBuf,
    path: PathBuf,
    rev: String,
    map: Option<(PathBuf, String)>,
}

struct RegisteredTap {
    token: u64,
    tap: FrontendHtmlTap,
}

/// An owned HTML tap registration. Dropping it removes only this registration.
pub struct FrontendTapRegistration {
    inner: Weak<FrontendInner>,
    id: String,
    token: u64,
}

impl std::fmt::Debug for FrontendTapRegistration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrontendTapRegistration")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl FrontendTapRegistration {
    pub fn remove(&self) -> bool {
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let mut taps = lock(&inner.taps);
        if taps
            .get(&self.id)
            .is_some_and(|registered| registered.token == self.token)
        {
            taps.remove(&self.id);
            true
        } else {
            false
        }
    }
}

impl Drop for FrontendTapRegistration {
    fn drop(&mut self) {
        self.remove();
    }
}

impl FrontendStatic {
    /// Opens the canonical distribution root. A missing root or `index.html` is
    /// an activation failure rather than an implicit development fallback.
    pub fn new(dist_root: impl AsRef<Path>) -> Result<Self, FrontendError> {
        Self::from_parts(dist_root.as_ref(), None)
    }

    /// Enables a bounded development HMR notification channel. Production
    /// callers use [`Self::new`], which exposes no HMR channel.
    pub fn new_with_hmr(
        dist_root: impl AsRef<Path>,
        queue_capacity: usize,
    ) -> Result<Self, FrontendError> {
        if !(1..=MAX_HMR_QUEUE).contains(&queue_capacity) {
            return Err(FrontendError::InvalidHmrQueue);
        }
        let (sender, _) = broadcast::channel(queue_capacity);
        Self::from_parts(dist_root.as_ref(), Some(sender))
    }

    fn from_parts(
        dist_root: &Path,
        hmr: Option<broadcast::Sender<WebHmrUpdate>>,
    ) -> Result<Self, FrontendError> {
        let dist_root = canonical_directory(dist_root, "distribution")?;
        let index_path = canonical_regular_file(&dist_root, &dist_root.join("index.html"))
            .map_err(|_| FrontendError::InvalidIndex {
                path: dist_root.join("index.html"),
            })?;
        Ok(Self {
            inner: Arc::new(FrontendInner {
                dist_root,
                index_path,
                state: Mutex::new(FrontendState::default()),
                taps: Mutex::new(BTreeMap::new()),
                hmr,
            }),
        })
    }

    /// Reads every supplied plugin root and atomically installs a new graph.
    /// Bad rows are accumulated and leave the old graph active.
    pub fn scan_packages<I, P>(&self, package_roots: I) -> Result<WebBootGraph, FrontendError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let roots = canonical_package_roots(package_roots)?;
        let scanned = scan_roots(&roots)?;
        Ok(self.install(roots, scanned))
    }

    /// Re-scans the roots from the most recent successful activation.
    pub fn rebuild(&self) -> Result<WebBootGraph, FrontendError> {
        let roots = lock(&self.inner.state).package_roots.clone();
        let scanned = scan_roots(&roots)?;
        Ok(self.install(roots, scanned))
    }

    /// Returns a snapshot that is safe to serialize directly into browser boot state.
    pub fn graph(&self) -> WebBootGraph {
        lock(&self.inner.state).graph.clone()
    }

    /// Receives bounded graph updates only when HMR was explicitly enabled.
    pub fn subscribe_hmr(&self) -> Option<broadcast::Receiver<WebHmrUpdate>> {
        self.inner.hmr.as_ref().map(broadcast::Sender::subscribe)
    }

    /// Registers an ordered transformation for every rendered application index.
    pub fn register_tap(
        &self,
        tap: FrontendHtmlTap,
    ) -> Result<FrontendTapRegistration, FrontendError> {
        validate_tap_id(&tap.id)?;
        let id = tap.id.clone();
        let mut state = lock(&self.inner.state);
        let mut taps = lock(&self.inner.taps);
        if taps.contains_key(&id) {
            return Err(FrontendError::DuplicateTapId(id));
        }
        state.next_tap_token = state.next_tap_token.wrapping_add(1);
        let token = state.next_tap_token;
        taps.insert(id.clone(), RegisteredTap { token, tap });
        Ok(FrontendTapRegistration {
            inner: Arc::downgrade(&self.inner),
            id,
            token,
        })
    }

    /// Applies ordered taps, then inserts boot data as the first child of `<head>`.
    pub fn render_index(&self) -> Result<String, FrontendError> {
        let index = canonical_regular_file(&self.inner.dist_root, &self.inner.index_path).map_err(
            |_| {
                FrontendError::RenderIndex("index is no longer inside the distribution root".into())
            },
        )?;
        let html = read_regular_limited(&index, MAX_STATIC_BYTES)
            .map_err(|error| FrontendError::RenderIndex(error.to_string()))?;
        let mut html = String::from_utf8(html)
            .map_err(|error| FrontendError::RenderIndex(format!("index is not UTF-8: {error}")))?;
        let taps = {
            let taps = lock(&self.inner.taps);
            let mut taps = taps.values().collect::<Vec<_>>();
            taps.sort_by(|left, right| {
                left.tap
                    .order
                    .cmp(&right.tap.order)
                    .then_with(|| left.tap.id.cmp(&right.tap.id))
            });
            taps.into_iter()
                .map(|registered| Arc::clone(&registered.tap.render))
                .collect::<Vec<_>>()
        };
        for tap in taps {
            html = tap(html);
        }
        let graph = self.graph();
        let graph = serde_json::to_string(&graph)
            .map_err(|error| FrontendError::RenderIndex(error.to_string()))?
            .replace('<', "\\u003c");
        inject_first_head_script(
            html,
            &format!("<script>window.__DSH_BOOT__={graph};</script>"),
        )
    }

    /// Serves host-owned files without interpreting application routes. The API
    /// integration passes its method and URI path here as its static fallback.
    pub fn serve(&self, method: Method, path: &str) -> Response<Body> {
        if method != Method::GET && method != Method::HEAD {
            return empty_response(StatusCode::METHOD_NOT_ALLOWED);
        }
        let head = method == Method::HEAD;
        let relative = match decode_relative_path(path) {
            Ok(path) => path,
            Err(()) => return empty_response(StatusCode::FORBIDDEN),
        };
        if path.starts_with("/plugins/") {
            return self.serve_plugin(path, head);
        }
        if relative.as_os_str().is_empty() || relative == Path::new("index.html") {
            return self.serve_index(head);
        }
        let candidate = self.inner.dist_root.join(relative);
        match canonical_regular_file(&self.inner.dist_root, &candidate) {
            Ok(file) if file == self.inner.index_path => self.serve_index(head),
            Ok(file) => match read_regular_limited(&file, MAX_STATIC_BYTES) {
                Ok(bytes) => file_response(StatusCode::OK, bytes, mime_for(&file), None, head),
                Err(_) => empty_response(StatusCode::NOT_FOUND),
            },
            Err(PathAccess::Missing) => self.serve_index(head),
            Err(PathAccess::Outside) => empty_response(StatusCode::FORBIDDEN),
            Err(PathAccess::Unreadable) => empty_response(StatusCode::NOT_FOUND),
        }
    }

    fn install(&self, roots: Vec<PathBuf>, scanned: ScannedPackages) -> WebBootGraph {
        let (graph, changed) = {
            let mut state = lock(&self.inner.state);
            let changed = state.graph.rev != scanned.graph.rev;
            state.package_roots = roots;
            state.bundles = scanned.bundles;
            state.graph = scanned.graph;
            (state.graph.clone(), changed)
        };
        if changed {
            if let Some(sender) = &self.inner.hmr {
                let _ = sender.send(WebHmrUpdate {
                    graph: graph.clone(),
                });
            }
        }
        graph
    }

    fn serve_index(&self, head: bool) -> Response<Body> {
        match self.render_index() {
            Ok(html) => file_response(
                StatusCode::OK,
                html.into_bytes(),
                "text/html",
                Some(("no-cache", None)),
                head,
            ),
            Err(_) => empty_response(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }

    fn serve_plugin(&self, path: &str, head: bool) -> Response<Body> {
        let Some((id, asset)) = plugin_asset(path) else {
            return empty_response(StatusCode::NOT_FOUND);
        };
        let bundle = lock(&self.inner.state).bundles.get(id).cloned();
        let Some(Bundle {
            root,
            path,
            rev,
            map,
        }) = bundle
        else {
            return empty_response(StatusCode::NOT_FOUND);
        };
        let (path, rev, mime) = match asset {
            PluginAsset::Client => (path, rev, "text/javascript"),
            PluginAsset::SourceMap => match map {
                Some((path, rev)) => (path, rev, "application/json"),
                None => return empty_response(StatusCode::NOT_FOUND),
            },
        };
        match canonical_regular_file(&root, &path).and_then(|path| {
            read_regular_limited(&path, MAX_BUNDLE_BYTES).map_err(|_| PathAccess::Unreadable)
        }) {
            Ok(bytes) if digest(&bytes) == rev => file_response(
                StatusCode::OK,
                bytes,
                mime,
                Some(("no-cache", Some(rev.as_str()))),
                head,
            ),
            _ => empty_response(StatusCode::NOT_FOUND),
        }
    }
}

struct ScannedPackages {
    graph: WebBootGraph,
    bundles: BTreeMap<String, Bundle>,
}

fn scan_roots(roots: &[PathBuf]) -> Result<ScannedPackages, FrontendError> {
    let mut manifests = Vec::new();
    let mut errors = Vec::new();
    let mut inspected_entries = 0;
    for root in roots {
        discover_manifests(root, 0, &mut inspected_entries, &mut manifests, &mut errors);
    }
    manifests.sort();
    manifests.dedup();

    let mut rows = Vec::new();
    for manifest in manifests {
        match scan_manifest(&manifest) {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => {}
            Err(error) => errors.push(error),
        }
    }
    rows.sort_by(|left, right| {
        left.entry
            .id
            .cmp(&right.entry.id)
            .then_with(|| left.manifest.cmp(&right.manifest))
    });
    // pnpm may materialize one byte-identical package per peer set.
    rows.dedup_by(|left, right| {
        left.entry == right.entry
            && left.map.as_ref().map(|(_, rev)| rev) == right.map.as_ref().map(|(_, rev)| rev)
    });
    for pair in rows.windows(2) {
        if pair[0].entry.id == pair[1].entry.id {
            errors.push(manifest_error(
                &pair[0].manifest,
                format!("duplicate web client id {:?}", pair[0].entry.id),
            ));
            errors.push(manifest_error(
                &pair[1].manifest,
                format!("duplicate web client id {:?}", pair[1].entry.id),
            ));
        }
    }
    errors.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.message.cmp(&right.message))
    });
    errors.dedup();
    if !errors.is_empty() {
        return Err(FrontendError::InvalidPackageManifests { errors });
    }
    if rows.len() > MAX_PACKAGES {
        return Err(FrontendError::InvalidPackageManifests {
            errors: vec![manifest_error(
                roots
                    .first()
                    .map_or_else(|| Path::new("."), PathBuf::as_path),
                format!("web package count exceeds {MAX_PACKAGES}"),
            )],
        });
    }

    let entries = rows.iter().map(|row| row.entry.clone()).collect::<Vec<_>>();
    let graph = WebBootGraph {
        rev: graph_rev(&entries),
        entries,
    };
    if serde_json::to_vec(&graph)
        .map_err(|_| FrontendError::GraphTooLarge)?
        .len()
        > MAX_GRAPH_BYTES
    {
        return Err(FrontendError::GraphTooLarge);
    }
    let bundles = rows
        .into_iter()
        .map(|row| {
            (
                row.entry.id,
                Bundle {
                    root: row.bundle_root,
                    path: row.bundle_path,
                    rev: row.entry.rev,
                    map: row.map,
                },
            )
        })
        .collect();
    Ok(ScannedPackages { graph, bundles })
}

fn discover_manifests(
    directory: &Path,
    depth: usize,
    entries: &mut usize,
    manifests: &mut Vec<PathBuf>,
    errors: &mut Vec<FrontendManifestError>,
) {
    if depth > MAX_PACKAGE_DEPTH {
        errors.push(manifest_error(
            directory,
            format!("package directory depth exceeds {MAX_PACKAGE_DEPTH}"),
        ));
        return;
    }
    let read_dir = match fs::read_dir(directory) {
        Ok(read_dir) => read_dir,
        Err(error) => {
            errors.push(manifest_error(directory, error.to_string()));
            return;
        }
    };
    for entry in read_dir {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(manifest_error(directory, error.to_string()));
                continue;
            }
        };
        *entries += 1;
        if *entries > MAX_PACKAGE_ENTRIES {
            errors.push(manifest_error(
                directory,
                format!("package directory entry count exceeds {MAX_PACKAGE_ENTRIES}"),
            ));
            return;
        }
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                errors.push(manifest_error(&path, error.to_string()));
                continue;
            }
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_file() && entry.file_name() == "package.json" {
            manifests.push(path);
        } else if file_type.is_dir() {
            let name = entry.file_name();
            let is_pnpm_modules = name.to_str() == Some("node_modules")
                && directory
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|parent| parent.to_str())
                    == Some(".pnpm");
            if matches!(name.to_str(), Some(".git" | "target" | "node_modules")) && !is_pnpm_modules
            {
                continue;
            }
            discover_manifests(&path, depth + 1, entries, manifests, errors);
        }
    }
}

struct ScannedRow {
    manifest: PathBuf,
    entry: WebBootEntry,
    bundle_root: PathBuf,
    bundle_path: PathBuf,
    map: Option<(PathBuf, String)>,
}

fn scan_manifest(manifest: &Path) -> Result<Option<ScannedRow>, FrontendManifestError> {
    let bytes = read_regular_limited(manifest, MAX_MANIFEST_BYTES)
        .map_err(|error| manifest_error(manifest, error.to_string()))?;
    let package: Value = serde_json::from_slice(&bytes)
        .map_err(|error| manifest_error(manifest, format!("invalid JSON: {error}")))?;
    let package = package
        .as_object()
        .ok_or_else(|| manifest_error(manifest, "package manifest must be an object"))?;
    let Some(dsh) = package.get("dsh") else {
        return Ok(None);
    };
    let dsh = dsh
        .as_object()
        .ok_or_else(|| manifest_error(manifest, "dsh must be an object"))?;
    let Some(client) = dsh.get("client") else {
        return Ok(None);
    };
    let client = client
        .as_object()
        .ok_or_else(|| manifest_error(manifest, "dsh.client must be an object"))?;
    let platform = required_string(manifest, client.get("platform"), "dsh.client.platform")?;
    if platform != "web" {
        return Ok(None);
    }
    let package_name = required_string(manifest, package.get("name"), "name")?;
    if !valid_package_id(&package_name) {
        return Err(manifest_error(
            manifest,
            "name must be an unscoped package or @scope/package using safe ASCII characters",
        ));
    }
    let id = package_name;
    let inject = optional_inject(manifest, client.get("inject"))?;
    let immediately = match client.get("immediately") {
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => {
            return Err(manifest_error(
                manifest,
                "dsh.client.immediately must be a boolean",
            ))
        }
        None => None,
    };
    let exports = package
        .get("exports")
        .and_then(Value::as_object)
        .ok_or_else(|| manifest_error(manifest, "exports must be an object with ./client"))?;
    let client_export = select_client_export(manifest, exports.get("./client"))?;
    let package_root = manifest
        .parent()
        .ok_or_else(|| manifest_error(manifest, "package manifest has no parent directory"))?;
    let package_root = fs::canonicalize(package_root).map_err(|error| {
        manifest_error(manifest, format!("package root is unreadable: {error}"))
    })?;
    let bundle_path = safe_export_path(&package_root, &client_export)
        .map_err(|message| manifest_error(manifest, message))?;
    let bytes = read_regular_limited(&bundle_path, MAX_BUNDLE_BYTES)
        .map_err(|error| manifest_error(manifest, format!("./client is unreadable: {error}")))?;
    let rev = digest(&bytes);
    let map = optional_source_map(&package_root, &bundle_path);
    Ok(Some(ScannedRow {
        manifest: manifest.to_path_buf(),
        entry: WebBootEntry {
            url: format!("/plugins/{id}/client.js?rev={rev}"),
            id,
            rev,
            inject,
            immediately,
        },
        bundle_root: package_root,
        bundle_path,
        map,
    }))
}

fn canonical_package_roots<I, P>(roots: I) -> Result<Vec<PathBuf>, FrontendError>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut canonical = BTreeSet::new();
    for root in roots {
        canonical.insert(canonical_directory(root.as_ref(), "plugin")?);
    }
    Ok(canonical.into_iter().collect())
}

fn canonical_directory(path: &Path, kind: &'static str) -> Result<PathBuf, FrontendError> {
    let canonical = fs::canonicalize(path).map_err(|error| FrontendError::InvalidRoot {
        kind,
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    if !canonical.is_dir() {
        return Err(FrontendError::InvalidRoot {
            kind,
            path: path.to_path_buf(),
            reason: "not a directory".into(),
        });
    }
    Ok(canonical)
}

enum PathAccess {
    Missing,
    Outside,
    Unreadable,
}

fn canonical_regular_file(root: &Path, path: &Path) -> Result<PathBuf, PathAccess> {
    let canonical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PathAccess::Missing)
        }
        Err(_) => return Err(PathAccess::Unreadable),
    };
    if !canonical.starts_with(root) {
        return Err(PathAccess::Outside);
    }
    if !canonical.is_file() {
        return Err(PathAccess::Missing);
    }
    Ok(canonical)
}

fn safe_export_path(root: &Path, export: &str) -> Result<PathBuf, String> {
    let export = export.strip_prefix("./").unwrap_or(export);
    if export.is_empty() || export.starts_with('/') {
        return Err("./client export is not a safe relative path".into());
    }
    let relative = decode_relative_path(&format!("/{export}"))
        .map_err(|_| "./client export is not a safe relative path")?;
    if relative.as_os_str().is_empty() {
        return Err("./client export must name a regular file".into());
    }
    canonical_regular_file(root, &root.join(relative)).map_err(|access| match access {
        PathAccess::Missing => {
            "./client export does not name a regular file inside its package".into()
        }
        PathAccess::Outside => "./client export escapes its package".into(),
        PathAccess::Unreadable => "./client export is unreadable".into(),
    })
}

fn optional_source_map(root: &Path, bundle_path: &Path) -> Option<(PathBuf, String)> {
    let mut map_path = bundle_path.as_os_str().to_os_string();
    map_path.push(".map");
    let map_path = canonical_regular_file(root, Path::new(&map_path)).ok()?;
    let bytes = read_regular_limited(&map_path, MAX_BUNDLE_BYTES).ok()?;
    Some((map_path, digest(&bytes)))
}

fn required_string(
    manifest: &Path,
    value: Option<&Value>,
    field: &str,
) -> Result<String, FrontendManifestError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| manifest_error(manifest, format!("{field} must be a string")))?;
    if value.trim().is_empty()
        || value.len() > MAX_STRING_BYTES
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(manifest_error(
            manifest,
            format!("{field} must be a non-empty bounded printable string"),
        ));
    }
    Ok(value.into())
}

fn select_client_export(
    manifest: &Path,
    export: Option<&Value>,
) -> Result<String, FrontendManifestError> {
    let export =
        export.ok_or_else(|| manifest_error(manifest, "exports[\"./client\"] is required"))?;
    let Value::Object(conditions) = export else {
        return required_string(manifest, Some(export), "exports[\"./client\"]");
    };
    for (condition, target) in conditions {
        if !matches!(
            condition.as_str(),
            "browser" | "import" | "default" | "types"
        ) {
            return Err(manifest_error(
                manifest,
                format!("exports[\"./client\"] has unsupported condition {condition:?}"),
            ));
        }
        required_string(
            manifest,
            Some(target),
            &format!("exports[\"./client\"].{condition}"),
        )?;
    }
    for condition in ["browser", "import", "default"] {
        if let Some(target) = conditions.get(condition) {
            return required_string(
                manifest,
                Some(target),
                &format!("exports[\"./client\"].{condition}"),
            );
        }
    }
    Err(manifest_error(
        manifest,
        "exports[\"./client\"] must declare browser, import, or default",
    ))
}

fn optional_inject(
    manifest: &Path,
    value: Option<&Value>,
) -> Result<Option<Vec<String>>, FrontendManifestError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| manifest_error(manifest, "dsh.client.inject must be an array of strings"))?;
    if values.len() > MAX_INJECT {
        return Err(manifest_error(
            manifest,
            format!("dsh.client.inject exceeds {MAX_INJECT} entries"),
        ));
    }
    let mut inject = Vec::with_capacity(values.len());
    for value in values {
        inject.push(required_string(
            manifest,
            Some(value),
            "dsh.client.inject[]",
        )?);
    }
    Ok(Some(inject))
}

fn valid_package_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_STRING_BYTES {
        return false;
    }
    let valid_segment = |segment: &str| {
        !segment.is_empty()
            && segment.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'~')
            })
    };
    if let Some(scoped) = id.strip_prefix('@') {
        let mut parts = scoped.split('/');
        matches!((parts.next(), parts.next(), parts.next()), (Some(scope), Some(package), None) if valid_segment(scope) && valid_segment(package))
    } else {
        !id.contains('/') && valid_segment(id)
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'~'))
}

fn manifest_error(path: &Path, message: impl Into<String>) -> FrontendManifestError {
    FrontendManifestError {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn graph_rev(entries: &[WebBootEntry]) -> String {
    digest(&serde_json::to_vec(entries).expect("WebBootEntry is serializable"))
}

fn digest(bytes: &[u8]) -> String {
    let digest = Sha1::digest(bytes);
    let mut output = String::with_capacity(12);
    for byte in &digest[..6] {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn read_regular_limited(path: &Path, maximum: usize) -> std::io::Result<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    if metadata.len() > maximum as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "file exceeds bounded read",
        ));
    }
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take((maximum + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "file exceeds bounded read",
        ));
    }
    Ok(bytes)
}

fn decode_relative_path(path: &str) -> Result<PathBuf, ()> {
    if !path.starts_with('/') {
        return Err(());
    }
    let bytes = percent_decode(path.as_bytes())?;
    let decoded = std::str::from_utf8(&bytes).map_err(|_| ())?;
    if decoded.contains('\0') {
        return Err(());
    }
    let mut relative = PathBuf::new();
    for component in Path::new(decoded.trim_start_matches('/')).components() {
        match component {
            Component::Normal(component) => relative.push(component),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return Err(()),
        }
    }
    Ok(relative)
}

fn percent_decode(input: &[u8]) -> Result<Vec<u8>, ()> {
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'%' {
            decoded.push(input[index]);
            index += 1;
            continue;
        }
        if index + 2 >= input.len() {
            return Err(());
        }
        let high = hex(input[index + 1]).ok_or(())?;
        let low = hex(input[index + 2]).ok_or(())?;
        decoded.push((high << 4) | low);
        index += 3;
    }
    Ok(decoded)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

enum PluginAsset {
    Client,
    SourceMap,
}

fn plugin_asset(path: &str) -> Option<(&str, PluginAsset)> {
    let path = path.strip_prefix("/plugins/")?;
    let (id, name) = path.rsplit_once('/')?;
    let asset = match name {
        "client.js" => PluginAsset::Client,
        "client.js.map" => PluginAsset::SourceMap,
        _ => return None,
    };
    valid_package_id(id).then_some((id, asset))
}

fn inject_first_head_script(mut html: String, script: &str) -> Result<String, FrontendError> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<head").ok_or(FrontendError::MissingHead)?;
    let end = lower[start..]
        .find('>')
        .map(|offset| start + offset + 1)
        .ok_or(FrontendError::MissingHead)?;
    html.insert_str(end, script);
    Ok(html)
}

fn mime_for(path: &Path) -> &'static str {
    mime_guess::from_path(path)
        .first_raw()
        .unwrap_or("application/octet-stream")
}

fn file_response(
    status: StatusCode,
    bytes: Vec<u8>,
    mime: &str,
    cache: Option<(&str, Option<&str>)>,
    head: bool,
) -> Response<Body> {
    let length = bytes.len().to_string();
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, length)
        .header("x-content-type-options", "nosniff");
    if let Some((cache_control, etag)) = cache {
        builder = builder.header(header::CACHE_CONTROL, cache_control);
        if let Some(rev) = etag {
            builder = builder.header(header::ETAG, format!("\"{rev}\""));
        }
    }
    builder
        .body(if head {
            Body::empty()
        } else {
            Body::from(bytes)
        })
        .expect("fixed headers are valid")
}

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, "0")
        .body(Body::empty())
        .expect("fixed response is valid")
}

fn validate_tap_id(id: &str) -> Result<(), FrontendError> {
    if !valid_id(id) {
        return Err(FrontendError::InvalidTapId(id.into()));
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

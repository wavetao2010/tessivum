use std::{
    collections::BTreeSet,
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tessivum_core::loader::RuntimeKind;
use tessivum_extism::Capability;
use thiserror::Error;

const PLUGIN_SCHEMA: &str = "cordis.plugin/v1";
const WASM_ABI: &str = "cordis.plugin/v1";
const SERVICE_CATALOG: &[(&str, &[&str])] = &[
    ("credentials@1", &["describe"]),
    ("logger@1", &["log"]),
    ("settings@1", &["describe"]),
    ("tools@1", &["schemas"]),
];

/// A single exact service/method grant retained by the WASM policy layer.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceMethodPermission {
    pub service: String,
    pub method: String,
}

/// The grouped wire representation used by product declarations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServicePermissionDeclaration {
    pub service: String,
    pub methods: Vec<String>,
}

/// A validated, loadable product declaration and its resolved package entry.
#[derive(Clone, Debug)]
pub struct WasmProductDeclaration {
    pub manifest: tessivum_extism::PluginManifest,
    pub service_permissions: BTreeSet<ServiceMethodPermission>,
    pub declaration_path: PathBuf,
    pub entry: PathBuf,
    pub root: PathBuf,
}

impl WasmProductDeclaration {
    pub fn manifest(&self) -> &tessivum_extism::PluginManifest {
        &self.manifest
    }

    pub fn service_permissions(&self) -> &BTreeSet<ServiceMethodPermission> {
        &self.service_permissions
    }
}


/// Default bound for each package or external manifest before parsing.
pub const DEFAULT_MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// Default bound for each scanned JavaScript or TypeScript source file.
pub const DEFAULT_MAX_SOURCE_BYTES: usize = 1024 * 1024;
/// Default bound for all manifest and source reads in one advisory report.
pub const DEFAULT_MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum number of directory entries walked while discovering artifacts.
pub const DEFAULT_MAX_RECURSIVE_ENTRIES: usize = 4096;
/// Default maximum directory nesting while discovering artifacts.
pub const DEFAULT_MAX_RECURSIVE_DEPTH: usize = 32;

/// Upper bounds for untrusted package inspection. Every bound is enforced before
/// content is retained or parsed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginInspectionLimits {
    pub max_manifest_bytes: usize,
    pub max_source_bytes: usize,
    pub max_total_bytes: usize,
    pub max_recursive_entries: usize,
    pub max_recursive_depth: usize,
}

impl Default for PluginInspectionLimits {
    fn default() -> Self {
        Self {
            max_manifest_bytes: DEFAULT_MAX_MANIFEST_BYTES,
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_recursive_entries: DEFAULT_MAX_RECURSIVE_ENTRIES,
            max_recursive_depth: DEFAULT_MAX_RECURSIVE_DEPTH,
        }
    }
}
const SOURCE_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "ts", "tsx", "jsx"];
const NODE_BUILTINS: &[&str] = &[
    "assert",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];
const STABLE_SERVICES: &[(&str, &str)] = &[
    ("tools", "tools@1"),
    ("systemprompt", "systemPrompt@1"),
    ("llm", "llm@1"),
    ("sessions", "sessions@1"),
    ("agents", "agents@1"),
    ("logger", "logger@1"),
    ("timers", "timers@1"),
    ("timer", "timers@1"),
    ("settings", "settings@1"),
    ("credentials", "credentials@1"),
];

/// The execution targets understood by package routing. `Browser` has no core
/// `RuntimeKind`, because it is published to a client instead of instantiated by the host loader.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginRuntime {
    Native,
    Wasm,
    LegacyNode,
    Browser,
}

impl PluginRuntime {
    pub fn core_runtime(self) -> Option<RuntimeKind> {
        match self {
            Self::Native => Some(RuntimeKind::Native),
            Self::Wasm => Some(RuntimeKind::Wasm),
            Self::LegacyNode => Some(RuntimeKind::LegacyNode),
            Self::Browser => None,
        }
    }
}

impl fmt::Display for PluginRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Native => "native",
            Self::Wasm => "wasm",
            Self::LegacyNode => "legacy-node",
            Self::Browser => "browser",
        })
    }
}

impl FromStr for PluginRuntime {
    type Err = PluginError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "native" => Ok(Self::Native),
            "wasm" => Ok(Self::Wasm),
            "legacy-node" | "legacy_node" => Ok(Self::LegacyNode),
            "browser" => Ok(Self::Browser),
            _ => Err(PluginError::unsupported_runtime(value)),
        }
    }
}

impl From<RuntimeKind> for PluginRuntime {
    fn from(value: RuntimeKind) -> Self {
        match value {
            RuntimeKind::Native => Self::Native,
            RuntimeKind::Wasm => Self::Wasm,
            RuntimeKind::LegacyNode => Self::LegacyNode,
        }
    }
}

impl TryFrom<PluginRuntime> for RuntimeKind {
    type Error = PluginError;

    fn try_from(value: PluginRuntime) -> Result<Self, Self::Error> {
        value
            .core_runtime()
            .ok_or_else(|| PluginError::unsupported_runtime("browser"))
    }
}

/// A stable diagnostic suitable for JSON command output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDiagnostic {
    pub code: String,
    pub message: String,
    pub help: String,
}

/// Routing and inspection failures deliberately distinguish invalid declarations from an
/// otherwise valid package that merely needs a different compatibility host.
#[derive(Debug, Error)]
pub enum PluginError {
    #[error("could not read {path}: {reason}")]
    Unreadable { path: PathBuf, reason: String },
    #[error("malformed manifest {path}: {reason}")]
    MalformedManifest { path: PathBuf, reason: String },
    #[error("invalid service permission manifest {path}: {reason}")]
    ManifestPermissionInvalid { path: PathBuf, reason: String },
    #[error("plugin inspection limit exceeded: {limit}")]
    BudgetExceeded { limit: &'static str },
    #[error("unsupported plugin runtime {runtime:?}")]
    UnsupportedRuntime { runtime: String },
    #[error("ambiguous plugin runtime: {runtimes:?}")]
    AmbiguousRuntime { runtimes: Vec<String> },
    #[error("invalid {runtime} route: {reason}")]
    InvalidRoute {
        runtime: PluginRuntime,
        reason: String,
    },
}

impl PluginError {
    fn unreadable(path: impl Into<PathBuf>, error: impl fmt::Display) -> Self {
        Self::Unreadable {
            path: path.into(),
            reason: error.to_string(),
        }
    }

    fn malformed(path: impl Into<PathBuf>, reason: impl fmt::Display) -> Self {
        Self::MalformedManifest {
            path: path.into(),
            reason: reason.to_string(),
        }
    }

    fn permission_invalid(path: impl Into<PathBuf>, reason: impl fmt::Display) -> Self {
        Self::ManifestPermissionInvalid {
            path: path.into(),
            reason: reason.to_string(),
        }
    }

    fn unsupported_runtime(runtime: impl Into<String>) -> Self {
        Self::UnsupportedRuntime {
            runtime: runtime.into(),
        }
    }

    pub fn diagnostic(&self) -> PluginDiagnostic {
        match self {
            Self::Unreadable { .. } => PluginDiagnostic {
                code: "PLUGIN_UNREADABLE".into(),
                message: self.to_string(),
                help: "Pass a readable package directory, manifest, or .wasm artifact.".into(),
            },
            Self::BudgetExceeded { .. } => PluginDiagnostic {
                code: "PLUGIN_INSPECTION_LIMIT_EXCEEDED".into(),
                message: self.to_string(),
                help: "Reduce the package size or configure higher inspection limits explicitly.".into(),
            },
            Self::MalformedManifest { .. } => PluginDiagnostic {
                code: "PLUGIN_MANIFEST_INVALID".into(),
                message: self.to_string(),
                help: format!("Use schemaVersion {PLUGIN_SCHEMA} and a supported runtime declaration."),
            },
            Self::ManifestPermissionInvalid { .. } => PluginDiagnostic {
                code: "MANIFEST_PERMISSION_INVALID".into(),
                message: self.to_string(),
                help: "Use only the exact versioned service catalog and nonempty methods.".into(),
            },
            Self::UnsupportedRuntime { .. } => PluginDiagnostic {
                code: "PLUGIN_RUNTIME_UNSUPPORTED".into(),
                message: self.to_string(),
                help: "Use native, wasm, legacy-node, or browser; browser is client-only.".into(),
            },
            Self::AmbiguousRuntime { .. } => PluginDiagnostic {
                code: "PLUGIN_RUNTIME_AMBIGUOUS".into(),
                message: self.to_string(),
                help: "Declare one versioned runtime or set the entry runtime explicitly.".into(),
            },
            Self::InvalidRoute { .. } => PluginDiagnostic {
                code: "PLUGIN_RUNTIME_INVALID".into(),
                message: self.to_string(),
                help: "Fix the selected runtime artifact or choose a runtime compatible with this package.".into(),
            },
        }
    }
}

#[derive(Clone, Debug)]
struct RuntimeDeclaration {
    runtime: PluginRuntime,
    entry: Option<PathBuf>,
    source: String,
    manifest: Value,
    declaration_path: PathBuf,
    service_permissions: Vec<ServicePermissionDeclaration>,
}


/// Package facts retained by the inspector. It intentionally retains no plugin configuration or
/// source text, so reports cannot disclose runtime secrets.
#[derive(Clone)]
pub struct PluginPackage {
    root: PathBuf,
    package: Option<Value>,
    manifest: Option<Value>,
    declarations: Vec<RuntimeDeclaration>,
    wasm_artifacts: Vec<PathBuf>,
    source_files: Vec<PathBuf>,
    limits: PluginInspectionLimits,
    manifest_bytes: usize,
}

impl fmt::Debug for PluginPackage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginPackage")
            .field("root", &self.root)
            .field("package_name", &self.package_name())
            .field("manifest_present", &self.manifest.is_some())
            .field(
                "declaration_runtimes",
                &self
                    .declarations
                    .iter()
                    .map(|declaration| declaration.runtime)
                    .collect::<Vec<_>>(),
            )
            .field("wasm_artifact_count", &self.wasm_artifacts.len())
            .field("source_file_count", &self.source_files.len())
            .finish()
    }
}

impl PluginPackage {
    pub fn inspect(path: impl AsRef<Path>) -> Result<Self, PluginError> {
        Self::inspect_with_limits(path, PluginInspectionLimits::default())
    }

    pub fn inspect_with_limits(
        path: impl AsRef<Path>,
        limits: PluginInspectionLimits,
    ) -> Result<Self, PluginError> {
        let requested = path.as_ref();
        let path = fs::canonicalize(requested)
            .map_err(|error| PluginError::unreadable(requested, error))?;
        let metadata =
            fs::metadata(&path).map_err(|error| PluginError::unreadable(&path, error))?;
        let root = if metadata.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        };
        let mut read_budget = ReadBudget::new(limits.max_total_bytes);

        let package_path = root.join("package.json");
        let package = if package_path.exists() {
            Some(read_json(&package_path, &limits, &mut read_budget)?)
        } else {
            None
        };
        let external_manifest = read_external_manifest(&root, &limits, &mut read_budget)?;
        let manifest = external_manifest
            .as_ref()
            .map(|(manifest, _)| manifest.clone());
        let mut declarations = package
            .as_ref()
            .map(|package| package_declarations(package, &root, &package_path))
            .transpose()?
            .unwrap_or_default();
        if let Some((manifest, declaration_path)) = external_manifest {
            declarations.push(parse_runtime_declaration(
                manifest,
                &root,
                &declaration_path,
                "external-manifest",
            )?);
        }

        let files = collect_files(&root, &limits)?;
        let wasm_artifacts = if metadata.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "wasm")
        {
            vec![path.to_path_buf()]
        } else {
            files.wasm_artifacts
        };

        Ok(Self {
            root,
            package,
            manifest,
            declarations,
            wasm_artifacts,
            source_files: files.source_files,
            limits,
            manifest_bytes: read_budget.used,
        })
    }

    /// Absolute, canonical package directory suitable for a confined legacy host.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves the executable selected for a specific runtime. Declarations for
    /// other runtimes cannot influence this choice.
    pub fn resolve(&self, runtime: PluginRuntime) -> Result<PathBuf, PluginError> {
        match runtime {
            PluginRuntime::LegacyNode => self.resolve_legacy_entry(),
            PluginRuntime::Wasm => self.resolve_wasm_artifact(),
            PluginRuntime::Browser => Err(PluginError::InvalidRoute {
                runtime,
                reason: "browser packages have no server-side executable artifact".into(),
            }),
            PluginRuntime::Native => Err(PluginError::InvalidRoute {
                runtime,
                reason: "package inspection cannot resolve native Rust registrations".into(),
            }),
        }
    }

    fn resolve_legacy_entry(&self) -> Result<PathBuf, PluginError> {
        let mut declared_entries = self

            .declarations
            .iter()
            .filter(|declaration| declaration.runtime == PluginRuntime::LegacyNode)
            .filter_map(|declaration| declaration.entry.as_ref())
            .map(|entry| self.canonical_entry(entry, PluginRuntime::LegacyNode))
            .collect::<Result<Vec<_>, _>>()?;
        declared_entries.sort();
        declared_entries.dedup();
        if !declared_entries.is_empty() {
            return match declared_entries.as_slice() {
                [entry] => Ok(entry.clone()),
                _ => Err(PluginError::AmbiguousRuntime {
                    runtimes: declared_entries
                        .iter()
                        .map(|entry| format!("legacy-node:{}", entry.display()))
                        .collect(),
                }),
            };
        }
        if let Some(entry) = self.package.as_ref().and_then(package_entry) {
            return self.canonical_entry(&self.root.join(entry), PluginRuntime::LegacyNode);
        }
        for entry in ["index.js", "index.mjs", "index.cjs"] {
            let entry = self.root.join(entry);
            if entry.is_file() {
                return self.canonical_entry(&entry, PluginRuntime::LegacyNode);
            }
        }
        Err(PluginError::InvalidRoute {
            runtime: PluginRuntime::LegacyNode,
            reason: "package has no resolvable declared entry, main, exports, or index entry"
                .into(),
        })
    }

    /// Projects the selected product declaration into the generic Extism manifest.
    /// Sparse Legacy Node and browser declarations remain inspectable; this is the
    /// explicit boundary that requires the complete WASM product contract.
    pub fn wasm_product_declaration(&self) -> Result<WasmProductDeclaration, PluginError> {
        let declarations = self
            .declarations
            .iter()
            .filter(|declaration| declaration.runtime == PluginRuntime::Wasm)
            .collect::<Vec<_>>();
        let declaration = match declarations.as_slice() {
            [declaration] => *declaration,
            [] => {
                return Err(PluginError::InvalidRoute {
                    runtime: PluginRuntime::Wasm,
                    reason: "a loadable WASM product requires one versioned wasm declaration"
                        .into(),
                });
            }
            declarations => {
                return Err(PluginError::AmbiguousRuntime {
                    runtimes: declarations
                        .iter()
                        .map(|declaration| {
                            format!("wasm:{}", declaration.declaration_path.display())
                        })
                        .collect(),
                });
            }
        };
        let manifest = project_wasm_manifest(&declaration.manifest, &declaration.declaration_path)?;
        if Path::new(&manifest.entry)
            .extension()
            .is_none_or(|extension| extension != "wasm")
        {
            return Err(PluginError::malformed(
                &declaration.declaration_path,
                "WASM product entry must end in .wasm",
            ));
        }
        let entry = self.canonical_entry(
            declaration
                .entry
                .as_ref()
                .ok_or_else(|| PluginError::malformed(&declaration.declaration_path, "entry is required"))?,
            PluginRuntime::Wasm,
        )?;
        if !self
            .wasm_artifacts
            .iter()
            .filter_map(|artifact| fs::canonicalize(artifact).ok())
            .any(|artifact| artifact == entry)
        {
            return Err(PluginError::InvalidRoute {
                runtime: PluginRuntime::Wasm,
                reason: format!("declared wasm entry {} is not a package artifact", entry.display()),
            });
        }
        let service_permissions = declaration
            .service_permissions
            .iter()
            .flat_map(|declaration| {
                declaration.methods.iter().map(|method| ServiceMethodPermission {
                    service: declaration.service.clone(),
                    method: method.clone(),
                })
            })
            .collect();
        Ok(WasmProductDeclaration {
            manifest,
            service_permissions,
            declaration_path: declaration.declaration_path.clone(),
            entry,
            root: self.root.clone(),
        })
    }

    fn resolve_wasm_artifact(&self) -> Result<PathBuf, PluginError> {
        let declarations = self
            .declarations
            .iter()
            .filter(|declaration| declaration.runtime == PluginRuntime::Wasm)
            .collect::<Vec<_>>();
        if !declarations.is_empty() {
            let mut artifacts = Vec::with_capacity(declarations.len());
            for declaration in declarations {
                let entry =
                    declaration
                        .entry
                        .as_ref()
                        .ok_or_else(|| PluginError::InvalidRoute {
                            runtime: PluginRuntime::Wasm,
                            reason:
                                "a versioned wasm declaration requires an entry ending in .wasm"
                                    .into(),
                        })?;
                if entry
                    .extension()
                    .is_none_or(|extension| extension != "wasm")
                {
                    return Err(PluginError::InvalidRoute {
                        runtime: PluginRuntime::Wasm,
                        reason: format!(
                            "declared wasm entry {} must end in .wasm",
                            entry.display()
                        ),
                    });
                }
                let entry = self.canonical_entry(entry, PluginRuntime::Wasm)?;
                if !self
                    .wasm_artifacts
                    .iter()
                    .filter_map(|artifact| fs::canonicalize(artifact).ok())
                    .any(|artifact| artifact == entry)
                {
                    return Err(PluginError::InvalidRoute {
                        runtime: PluginRuntime::Wasm,
                        reason: format!(
                            "declared wasm entry {} is not a package artifact",
                            entry.display()
                        ),
                    });
                }
                artifacts.push(entry);
            }
            artifacts.sort();
            artifacts.dedup();
            return match artifacts.as_slice() {
                [artifact] => Ok(artifact.clone()),
                _ => Err(PluginError::AmbiguousRuntime {
                    runtimes: artifacts
                        .iter()
                        .map(|artifact| format!("wasm:{}", artifact.display()))
                        .collect(),
                }),
            };
        }

        match self.wasm_artifacts.as_slice() {
            [artifact] => self.canonical_entry(artifact, PluginRuntime::Wasm),
            artifacts if artifacts.len() > 1 => Err(PluginError::AmbiguousRuntime {
                runtimes: artifacts
                    .iter()
                    .map(|artifact| format!("wasm:{}", artifact.display()))
                    .collect(),
            }),
            _ => Err(PluginError::InvalidRoute {
                runtime: PluginRuntime::Wasm,
                reason: "no .wasm artifact was found; explicit runtime is not allowed to fall back"
                    .into(),
            }),
        }
    }

    fn canonical_entry(
        &self,
        entry: &Path,
        runtime: PluginRuntime,
    ) -> Result<PathBuf, PluginError> {
        let entry =
            fs::canonicalize(entry).map_err(|error| PluginError::unreadable(entry, error))?;
        if entry.starts_with(&self.root) && entry.is_file() {
            Ok(entry)
        } else {
            Err(PluginError::InvalidRoute {
                runtime,
                reason: "selected artifact must be a regular file inside the package".into(),
            })
        }
    }

    fn package_name(&self) -> Option<&str> {
        self.package
            .as_ref()
            .and_then(|package| package.get("name"))
            .and_then(Value::as_str)
    }

    fn version(&self) -> Option<String> {
        self.manifest
            .as_ref()
            .and_then(|manifest| manifest.get("version"))
            .and_then(Value::as_str)
            .or_else(|| {
                self.package
                    .as_ref()
                    .and_then(|package| package.get("version"))
                    .and_then(Value::as_str)
            })
            .map(ToOwned::to_owned)
    }

    fn identifier(&self) -> String {
        self.manifest
            .as_ref()
            .and_then(|manifest| manifest.get("id"))
            .and_then(Value::as_str)
            .or_else(|| self.package_name())
            .unwrap_or("anonymous-wasm")
            .to_owned()
    }

    fn license(&self) -> Option<String> {
        self.package
            .as_ref()
            .and_then(|package| package.get("license"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    }

    fn dsh_client(&self) -> Result<Option<Value>, PluginError> {
        let Some(package) = &self.package else {
            return Ok(None);
        };
        let Some(dsh) = package.get("dsh") else {
            return Ok(None);
        };
        let dsh = dsh.as_object().ok_or_else(|| {
            PluginError::malformed(self.root.join("package.json"), "dsh must be an object")
        })?;
        Ok(dsh.get("client").map(project_dsh_client))
    }

    fn service_permissions(&self, runtime: PluginRuntime) -> Vec<ServicePermissionDeclaration> {
        self.declarations
            .iter()
            .find(|declaration| declaration.runtime == runtime)
            .map(|declaration| declaration.service_permissions.clone())
            .unwrap_or_default()
    }

    fn select_declaration(&self) -> Result<Option<&RuntimeDeclaration>, PluginError> {
        let runtimes = self
            .declarations
            .iter()
            .map(|declaration| declaration.runtime)
            .collect::<BTreeSet<_>>();
        if runtimes.len() > 1 {
            return Err(PluginError::AmbiguousRuntime {
                runtimes: runtimes
                    .into_iter()
                    .map(|runtime| runtime.to_string())
                    .collect(),
            });
        }
        Ok(self.declarations.first())
    }

    fn has_package(&self) -> bool {
        self.package_name().is_some()
    }
}

/// Reads package facts without choosing a host runtime.
#[derive(Clone, Debug, Default)]
pub struct PluginPackageResolver {
    limits: PluginInspectionLimits,
}

impl PluginPackageResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: PluginInspectionLimits) -> Self {
        Self { limits }
    }

    pub fn inspect(&self, path: impl AsRef<Path>) -> Result<PluginPackage, PluginError> {
        PluginPackage::inspect_with_limits(path, self.limits.clone())
    }

    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<PluginPackage, PluginError> {
        self.inspect(path)
    }
}

/// The selected runtime and its exact server-side artifact, if it has one.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginRoute {
    pub runtime: PluginRuntime,
    pub rule: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<PathBuf>,
}

/// A scanner is deliberately not a loader: its static observations never select a runtime.
#[derive(Clone, Debug, Default)]
pub struct PluginRouter {
    limits: PluginInspectionLimits,
}

impl PluginRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: PluginInspectionLimits) -> Self {
        Self { limits }
    }

    /// Inspects a package and returns its route and advisory compatibility report.
    pub fn inspect(
        &self,
        path: impl AsRef<Path>,
        explicit_runtime: Option<PluginRuntime>,
    ) -> Result<CompatibilityReport, PluginError> {
        self.report(path, explicit_runtime)
    }

    pub fn package(&self, path: impl AsRef<Path>) -> Result<PluginPackage, PluginError> {
        PluginPackage::inspect_with_limits(path, self.limits.clone())
    }

    /// Resolves the artifact for the routed runtime without loading untrusted code.
    pub fn resolve(
        &self,
        path: impl AsRef<Path>,
        explicit_runtime: Option<PluginRuntime>,
    ) -> Result<PluginRoute, PluginError> {
        let package = self.package(path)?;
        self.route(&package, explicit_runtime)
    }

    /// Selects execution with the required precedence: entry override, versioned declaration,
    /// wasm artifact, browser-only package, then ordinary npm/Cordis legacy-node.
    pub fn route(
        &self,
        package: &PluginPackage,
        explicit_runtime: Option<PluginRuntime>,
    ) -> Result<PluginRoute, PluginError> {
        if let Some(runtime) = explicit_runtime {
            self.validate_route(package, runtime)?;
            return self.route_with_artifact(package, runtime, "explicit-entry-runtime".into());
        }

        if let Some(declaration) = package.select_declaration()? {
            self.validate_declared_route(package, declaration)?;
            return self.route_with_artifact(
                package,
                declaration.runtime,
                format!("versioned-runtime-declaration:{}", declaration.source),
            );
        }

        match package.wasm_artifacts.as_slice() {
            [artifact] => {
                return self.route_with_artifact(
                    package,
                    PluginRuntime::Wasm,
                    format!(
                        "wasm-artifact:{}",
                        artifact
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("plugin.wasm")
                    ),
                );
            }
            artifacts if artifacts.len() > 1 => {
                return Err(PluginError::AmbiguousRuntime {
                    runtimes: artifacts
                        .iter()
                        .map(|artifact| format!("wasm:{}", artifact.display()))
                        .collect(),
                });
            }
            _ => {}
        }

        if package.dsh_client()?.is_some() {
            return self.route_with_artifact(
                package,
                PluginRuntime::Browser,
                "dsh-client-browser-package".into(),
            );
        }
        if package.has_package() {
            self.validate_route(package, PluginRuntime::LegacyNode)?;
            return self.route_with_artifact(
                package,
                PluginRuntime::LegacyNode,
                "npm-cordis-default-legacy-node".into(),
            );
        }

        Err(PluginError::InvalidRoute {
            runtime: PluginRuntime::LegacyNode,
            reason: "no explicit runtime, versioned runtime declaration, .wasm artifact, dsh.client metadata, or npm package name was found".into(),
        })
    }

    pub fn report(
        &self,
        path: impl AsRef<Path>,
        explicit_runtime: Option<PluginRuntime>,
    ) -> Result<CompatibilityReport, PluginError> {
        let package = self.package(path)?;
        let route = self.route(&package, explicit_runtime)?;
        CompatibilityReport::from_package(&package, route)
    }

    fn route_with_artifact(
        &self,
        package: &PluginPackage,
        runtime: PluginRuntime,
        rule: String,
    ) -> Result<PluginRoute, PluginError> {
        let artifact = match runtime {
            PluginRuntime::LegacyNode | PluginRuntime::Wasm => Some(package.resolve(runtime)?),
            PluginRuntime::Browser => None,
            PluginRuntime::Native => {
                unreachable!("native package routes are rejected before resolution")
            }
        };
        Ok(PluginRoute {
            runtime,
            rule,
            artifact,
        })
    }

    fn validate_declared_route(
        &self,
        package: &PluginPackage,
        declaration: &RuntimeDeclaration,
    ) -> Result<(), PluginError> {
        self.validate_route(package, declaration.runtime)
    }

    fn validate_route(
        &self,
        package: &PluginPackage,
        runtime: PluginRuntime,
    ) -> Result<(), PluginError> {
        match runtime {
            PluginRuntime::Wasm if package.wasm_artifacts.is_empty() => Err(PluginError::InvalidRoute {
                runtime,
                reason: "no .wasm artifact was found; explicit runtime is not allowed to fall back".into(),
            }),
            PluginRuntime::LegacyNode if !package.has_package() => Err(PluginError::InvalidRoute {
                runtime,
                reason: "legacy-node requires a package.json name; explicit runtime is not allowed to fall back".into(),
            }),
            PluginRuntime::Browser if package.dsh_client()?.is_none() => Err(PluginError::InvalidRoute {
                runtime,
                reason: "browser requires package dsh.client metadata; explicit runtime is not allowed to fall back".into(),
            }),
            PluginRuntime::Native => Err(PluginError::InvalidRoute {
                runtime,
                reason: "package inspection cannot load native Rust registrations; register the native entry in the host instead".into(),
            }),
            _ => Ok(()),
        }
    }
}

/// Public result of a non-executing package inspection.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibilityReport {
    pub package: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    pub selected_runtime: PluginRuntime,
    pub selected_rule: String,
    pub exports: ExportShape,
    pub inject: Vec<String>,
    pub service_permissions: Vec<ServicePermissionDeclaration>,
    pub provide: Vec<String>,
    pub events: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsh_client: Option<Value>,
    pub static_markers: StaticMarkers,
    pub stable_cross_runtime_services: Vec<String>,
    pub compatibility: CompatibilityClass,
    pub reasons: Vec<String>,
    /// Static scanning is informational and never asserts migration or runtime loadability.
    pub static_scan_advisory: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportShape {
    pub names: Vec<String>,
    pub apply: bool,
    pub class: bool,
    pub service: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticMarkers {
    pub node_builtins: Vec<String>,
    pub native_addon_markers: Vec<String>,
    pub dom_markers: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompatibilityClass {
    DirectLegacy,
    NeedsProxy,
    WasmPort,
    Browser,
    Unsupported,
}

impl CompatibilityReport {
    fn from_package(package: &PluginPackage, route: PluginRoute) -> Result<Self, PluginError> {
        let analysis = analyze_sources(package)?;
        let dsh_client = package.dsh_client()?;
        let mut inject = analysis.inject;
        if let Some(manifest) = &package.manifest {
            inject.extend(manifest_services(manifest, "inject"));
        }
        inject.sort();
        inject.dedup();

        let stable_cross_runtime_services = stable_services(&inject, &analysis.service_references);
        let service_permissions = package.service_permissions(route.runtime);
        let mut reasons = vec![
            "Static source analysis is advisory only: it neither migrates this plugin nor proves runtime loadability."
                .into(),
        ];
        let compatibility = match route.runtime {
            PluginRuntime::Browser => {
                reasons.push("dsh.client metadata selects the browser Cordis host; it is not a WASM migration.".into());
                CompatibilityClass::Browser
            }
            PluginRuntime::Wasm => {
                reasons.push("WASM was selected by routing; legacy apply(ctx) source still requires an explicit port if present.".into());
                CompatibilityClass::WasmPort
            }
            PluginRuntime::LegacyNode => {
                if !analysis.markers.dom_markers.is_empty() {
                    reasons.push("DOM/browser APIs were found without a dsh.client browser declaration; publish a browser half or remove the DOM dependency.".into());
                    CompatibilityClass::Unsupported
                } else if !stable_cross_runtime_services.is_empty() {
                    reasons.push(format!(
                        "Uses stable cross-runtime services ({}); load through the typed service proxies.",
                        stable_cross_runtime_services.join(", ")
                    ));
                    CompatibilityClass::NeedsProxy
                } else {
                    if !analysis.markers.native_addon_markers.is_empty() {
                        reasons.push("Native-addon markers require a compatible Node host and platform binary; they are trusted Legacy Node code, not WASM.".into());
                    }
                    CompatibilityClass::DirectLegacy
                }
            }
            PluginRuntime::Native => {
                unreachable!("native package routes are rejected before reporting")
            }
        };

        Ok(Self {
            package: package
                .package_name()
                .unwrap_or("anonymous-wasm")
                .to_owned(),
            id: package.identifier(),
            version: package.version(),
            license: package.license(),
            selected_runtime: route.runtime,
            selected_rule: route.rule,
            exports: analysis.exports,
            inject,
            service_permissions,
            provide: analysis.provide,
            events: analysis.events,
            dsh_client,
            static_markers: analysis.markers,
            stable_cross_runtime_services,
            compatibility,
            reasons,
            static_scan_advisory:
                "advisory only; source scanning never performs or claims automatic migration".into(),
        })
    }
}

#[derive(Default)]
struct SourceAnalysis {
    exports: ExportShape,
    inject: Vec<String>,
    provide: Vec<String>,
    events: Vec<String>,
    markers: StaticMarkers,
    service_references: Vec<String>,
}

fn analyze_sources(package: &PluginPackage) -> Result<SourceAnalysis, PluginError> {
    let mut analysis = SourceAnalysis::default();
    let export = Regex::new(r"(?m)\bexport\s+(?:const|let|var|function|class)\s+([A-Za-z_$][A-Za-z0-9_$]*)|\bexports\.([A-Za-z_$][A-Za-z0-9_$]*)\s*=")
        .expect("valid export expression");
    let lists = ["inject", "provide"];
    let event = Regex::new(r#"\bctx\.(?:on|once|emit|waterfall)\s*\(\s*['\"]([^'\"]+)['\"]"#)
        .expect("valid event expression");
    let provide_call = Regex::new(r#"\bctx\.provide\s*\(\s*['\"]([^'\"]+)['\"]"#)
        .expect("valid provide expression");
    let builtins = Regex::new(
        r#"(?:\bfrom\s+|\brequire\s*\(\s*)['\"](?:node:)?([A-Za-z_][A-Za-z0-9_-]*)['\"]"#,
    )
    .expect("valid builtin expression");
    let mut read_budget = ReadBudget {
        maximum: package.limits.max_total_bytes,
        used: package.manifest_bytes,
    };

    for path in &package.source_files {
        let source = match read_limited(
            path,
            package.limits.max_source_bytes,
            "source bytes",
            &mut read_budget,
        ) {
            Ok(source) => source,
            Err(error @ PluginError::BudgetExceeded { .. }) => return Err(error),
            Err(_) => continue,
        };
        let Ok(source) = std::str::from_utf8(&source) else {
            continue;
        };
        for captures in export.captures_iter(source) {
            if let Some(name) = captures.get(1).or_else(|| captures.get(2)) {
                analysis.exports.names.push(name.as_str().to_owned());
            }
        }
        analysis.exports.apply |= has_exported_name(source, "apply");
        analysis.exports.class |=
            source.contains("export default class") || source.contains("module.exports = class");
        analysis.exports.service |=
            source.contains("extends Service") || source.contains("ctx.provide(");
        for name in lists {
            let values = assignment_strings(source, name);
            if name == "inject" {
                analysis.inject.extend(values);
            }
        }
        analysis.provide.extend(
            provide_call
                .captures_iter(source)
                .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_owned())),
        );
        analysis.events.extend(
            event
                .captures_iter(source)
                .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_owned())),
        );
        for captures in builtins.captures_iter(source) {
            let module = captures.get(1).expect("builtin capture").as_str();
            if NODE_BUILTINS.contains(&module) {
                analysis.markers.node_builtins.push(module.to_owned());
            }
        }
        for marker in [
            "process.dlopen",
            ".node'",
            ".node\"",
            "bindings(",
            "node-gyp",
            "node-pre-gyp",
            "prebuild-install",
            "napi-rs",
        ] {
            if source.contains(marker) {
                analysis
                    .markers
                    .native_addon_markers
                    .push(marker.to_owned());
            }
        }
        for marker in [
            "window.",
            "document.",
            "HTMLElement",
            "React",
            "from 'react'",
            "from \"react\"",
        ] {
            if source.contains(marker) {
                analysis.markers.dom_markers.push(marker.to_owned());
            }
        }
        for (name, _) in STABLE_SERVICES {
            if source.contains(&format!("ctx.{name}"))
                || (*name == "systemprompt" && source.contains("ctx.systemPrompt"))
            {
                analysis.service_references.push((*name).to_owned());
            }
        }
    }

    if let Some(package_json) = &package.package {
        for dependency_field in ["dependencies", "optionalDependencies", "devDependencies"] {
            if let Some(dependencies) = package_json
                .get(dependency_field)
                .and_then(Value::as_object)
            {
                for dependency in dependencies.keys() {
                    let lower = dependency.to_ascii_lowercase();
                    if [
                        "bindings",
                        "node-gyp",
                        "node-pre-gyp",
                        "prebuild-install",
                        "ffi-napi",
                        "napi-rs",
                    ]
                    .iter()
                    .any(|marker| lower.contains(marker))
                    {
                        analysis
                            .markers
                            .native_addon_markers
                            .push(dependency.clone());
                    }
                }
            }
        }
    }

    analysis.exports.names.sort();
    analysis.exports.names.dedup();
    for values in [
        &mut analysis.inject,
        &mut analysis.provide,
        &mut analysis.events,
        &mut analysis.markers.node_builtins,
        &mut analysis.markers.native_addon_markers,
        &mut analysis.markers.dom_markers,
        &mut analysis.service_references,
    ] {
        values.sort();
        values.dedup();
    }
    Ok(analysis)
}

fn has_exported_name(source: &str, name: &str) -> bool {
    source.contains(&format!("export function {name}"))
        || source.contains(&format!("export const {name}"))
        || source.contains(&format!("export let {name}"))
        || source.contains(&format!("exports.{name}"))
}

fn assignment_strings(source: &str, name: &str) -> Vec<String> {
    let assignment = Regex::new(&format!(
        r"(?s)\b(?:export\s+(?:const|let|var)\s+)?{name}\s*=\s*\[([^\]]*)\]"
    ))
    .expect("valid assignment expression");
    let string = Regex::new(r#"['\"]([^'\"]+)['\"]"#).expect("valid string expression");
    assignment
        .captures_iter(source)
        .flat_map(|capture| {
            string
                .captures_iter(capture.get(1).expect("list capture").as_str())
                .filter_map(|value| value.get(1).map(|value| value.as_str().to_owned()))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn stable_services(inject: &[String], references: &[String]) -> Vec<String> {
    let mut services = BTreeSet::new();
    for value in inject.iter().chain(references) {
        let normalized = value
            .split('@')
            .next()
            .unwrap_or(value)
            .replace(['-', '_'], "")
            .to_ascii_lowercase();
        if let Some((_, stable)) = STABLE_SERVICES.iter().find(|(name, _)| *name == normalized) {
            services.insert((*stable).to_owned());
        }
    }
    services.into_iter().collect()
}

fn manifest_services(manifest: &Value, field: &str) -> Vec<String> {
    manifest
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| match value {
            Value::String(value) => Some(value.clone()),
            Value::Object(value) => value
                .get("service")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            _ => None,
        })
        .collect()
}

fn package_entry(package: &Value) -> Option<&str> {
    package
        .get("exports")
        .and_then(export_entry)
        .or_else(|| package.get("main").and_then(Value::as_str))
        .or_else(|| package.get("module").and_then(Value::as_str))
}

fn export_entry(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value),
        Value::Object(values) => values
            .get(".")
            .and_then(export_entry)
            .or_else(|| values.get("import").and_then(export_entry))
            .or_else(|| values.get("require").and_then(export_entry))
            .or_else(|| values.get("default").and_then(export_entry)),
        _ => None,
    }
}

struct ReadBudget {
    maximum: usize,
    used: usize,
}

impl ReadBudget {
    fn new(maximum: usize) -> Self {
        Self { maximum, used: 0 }
    }

    fn check(&self, bytes: usize) -> Result<(), PluginError> {
        if bytes > self.maximum.saturating_sub(self.used) {
            return Err(PluginError::BudgetExceeded {
                limit: "total bytes",
            });
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> Result<(), PluginError> {
        self.check(bytes)?;
        self.used += bytes;
        Ok(())
    }
}

fn read_limited(
    path: &Path,
    maximum: usize,
    limit: &'static str,
    budget: &mut ReadBudget,
) -> Result<Vec<u8>, PluginError> {
    let metadata = fs::metadata(path).map_err(|error| PluginError::unreadable(path, error))?;
    if !metadata.is_file() {
        return Err(PluginError::unreadable(path, "not a regular file"));
    }
    if metadata.len() > u64::try_from(maximum).unwrap_or(u64::MAX) {
        return Err(PluginError::BudgetExceeded { limit });
    }
    budget.check(metadata.len() as usize)?;

    let mut file = fs::File::open(path).map_err(|error| PluginError::unreadable(path, error))?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let remaining = maximum.saturating_sub(bytes.len());
        if remaining == 0 {
            let mut extra = [0_u8; 1];
            if file
                .read(&mut extra)
                .map_err(|error| PluginError::unreadable(path, error))?
                != 0
            {
                return Err(PluginError::BudgetExceeded { limit });
            }
            break;
        }
        let chunk_len = remaining.min(chunk.len());
        let count = file
            .read(&mut chunk[..chunk_len])
            .map_err(|error| PluginError::unreadable(path, error))?;
        if count == 0 {
            break;
        }
        budget.charge(count)?;
        bytes
            .try_reserve_exact(count)
            .map_err(|_| PluginError::unreadable(path, "bounded reader allocation failed"))?;
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(bytes)
}

fn read_json(
    path: &Path,
    limits: &PluginInspectionLimits,
    budget: &mut ReadBudget,
) -> Result<Value, PluginError> {
    let document = read_limited(path, limits.max_manifest_bytes, "manifest bytes", budget)?;
    serde_json::from_slice(&document).map_err(|error| PluginError::malformed(path, error))
}

fn read_external_manifest(
    root: &Path,
    limits: &PluginInspectionLimits,
    budget: &mut ReadBudget,
) -> Result<Option<(Value, PathBuf)>, PluginError> {
    let candidates = [
        "cordis.plugin.json",
        "cordis.plugin.yaml",
        "cordis.plugin.yml",
        "plugin.json",
        "plugin.yaml",
        "plugin.yml",
    ];
    let found = candidates
        .iter()
        .map(|name| root.join(name))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if found.len() > 1 {
        return Err(PluginError::AmbiguousRuntime {
            runtimes: found
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        });
    }
    let Some(path) = found.first() else {
        return Ok(None);
    };
    let document = read_limited(path, limits.max_manifest_bytes, "manifest bytes", budget)?;
    let value = if path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        serde_json::from_slice(&document).map_err(|error| PluginError::malformed(path, error))?
    } else {
        let yaml: serde_yaml::Value = serde_yaml::from_slice(&document)
            .map_err(|error| PluginError::malformed(path, error))?;
        serde_json::to_value(yaml).map_err(|error| PluginError::malformed(path, error))?
    };
    Ok(Some((value, path.clone())))
}

fn parse_runtime_declaration(
    manifest: Value,
    root: &Path,
    declaration_path: &Path,
    source: impl Into<String>,
) -> Result<RuntimeDeclaration, PluginError> {
    let service_permissions = validate_manifest(&manifest, declaration_path)?;
    let runtime = manifest
        .get("runtime")
        .and_then(Value::as_str)
        .expect("manifest validation requires runtime")
        .parse()?;
    let entry = manifest
        .get("entry")
        .and_then(Value::as_str)
        .map(|entry| root.join(entry));
    Ok(RuntimeDeclaration {
        runtime,
        entry,
        source: source.into(),
        manifest,
        declaration_path: declaration_path.to_path_buf(),
        service_permissions,
    })
}

fn package_declarations(
    package: &Value,
    directory: &Path,
    package_path: &Path,
) -> Result<Vec<RuntimeDeclaration>, PluginError> {
    let package = package
        .as_object()
        .ok_or_else(|| PluginError::malformed(package_path, "package root must be an object"))?;
    let mut declarations = Vec::new();
    for key in ["cordis", "tessivum"] {
        let Some(value) = package.get(key) else {
            continue;
        };
        let object = value.as_object().ok_or_else(|| {
            PluginError::malformed(package_path, format!("{key} must be an object"))
        })?;
        let candidate = object.get("plugin").unwrap_or(value);
        let candidate = candidate.as_object().ok_or_else(|| {
            PluginError::malformed(package_path, format!("{key}.plugin must be an object"))
        })?;
        if candidate.contains_key("runtime") || candidate.contains_key("schemaVersion") {
            declarations.push(parse_runtime_declaration(
                Value::Object(candidate.clone()),
                directory,
                package_path,
                format!("package.{key}"),
            )?);
        }
    }
    Ok(declarations)
}

fn validate_manifest(
    value: &Value,
    path: &Path,
) -> Result<Vec<ServicePermissionDeclaration>, PluginError> {
    let object = value
        .as_object()
        .ok_or_else(|| PluginError::malformed(path, "manifest must be an object"))?;
    let schema = object
        .get("schemaVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::malformed(path, "schemaVersion is required"))?;
    if schema != PLUGIN_SCHEMA {
        return Err(PluginError::malformed(
            path,
            format!("schemaVersion must be {PLUGIN_SCHEMA}, got {schema}"),
        ));
    }
    let runtime = object
        .get("runtime")
        .and_then(Value::as_str)
        .ok_or_else(|| PluginError::malformed(path, "runtime is required"))?;
    runtime.parse::<PluginRuntime>()?;
    if object
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id.trim().is_empty())
    {
        return Err(PluginError::malformed(path, "id must not be blank"));
    }
    parse_service_permissions(value, path)
}

fn parse_service_permissions(
    manifest: &Value,
    path: &Path,
) -> Result<Vec<ServicePermissionDeclaration>, PluginError> {
    let Some(value) = manifest.get("servicePermissions") else {
        return Ok(Vec::new());
    };
    let declarations = serde_json::from_value::<Vec<ServicePermissionDeclaration>>(value.clone())
        .map_err(|error| PluginError::permission_invalid(path, error))?;
    let mut services = BTreeSet::new();
    let mut grouped = Vec::with_capacity(declarations.len());
    for mut declaration in declarations {
        if declaration.service.is_empty() || contains_permission_pattern(&declaration.service) {
            return Err(PluginError::permission_invalid(
                path,
                format!("service {:?} must be an exact catalog name", declaration.service),
            ));
        }
        let Some((_, allowed_methods)) = SERVICE_CATALOG
            .iter()
            .find(|(service, _)| *service == declaration.service.as_str())
        else {
            return Err(PluginError::permission_invalid(
                path,
                format!("service {:?} is not in the catalog", declaration.service),
            ));
        };
        if !services.insert(declaration.service.clone()) {
            return Err(PluginError::permission_invalid(
                path,
                format!("service {:?} is declared more than once", declaration.service),
            ));
        }
        if declaration.methods.is_empty() {
            return Err(PluginError::permission_invalid(
                path,
                format!("service {:?} must declare at least one method", declaration.service),
            ));
        }
        let mut methods = BTreeSet::new();
        for method in declaration.methods {
            if method.is_empty() || contains_permission_pattern(&method) {
                return Err(PluginError::permission_invalid(
                    path,
                    format!("method {method:?} must be an exact catalog name"),
                ));
            }
            if !allowed_methods
                .iter()
                .any(|allowed| *allowed == method.as_str())
            {
                return Err(PluginError::permission_invalid(
                    path,
                    format!("method {method:?} is not allowed for {}", declaration.service),
                ));
            }
            if !methods.insert(method) {
                return Err(PluginError::permission_invalid(
                    path,
                    format!("service method {} is declared more than once", declaration.service),
                ));
            }
        }
        declaration.methods = methods.into_iter().collect();
        grouped.push(declaration);
    }
    grouped.sort_by(|left, right| left.service.cmp(&right.service));
    if !grouped.is_empty()
        && !manifest
            .get("permissions")
            .and_then(Value::as_array)
            .is_some_and(|permissions| {
                permissions.iter().any(|permission| {
                    permission.as_str() == Some(Capability::ServiceCall.as_str())
                })
            })
    {
        return Err(PluginError::permission_invalid(
            path,
            "servicePermissions requires permissions to include cordis.service.call",
        ));
    }
    Ok(grouped)
}

fn contains_permission_pattern(value: &str) -> bool {
    value.chars().any(|character| {
        matches!(
            character,
            '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '|' | '^' | '$' | '+' | '\\'
        )
    })
}

fn project_wasm_manifest(
    declaration: &Value,
    path: &Path,
) -> Result<tessivum_extism::PluginManifest, PluginError> {
    let object = declaration
        .as_object()
        .ok_or_else(|| PluginError::malformed(path, "manifest must be an object"))?;
    const PRODUCT_FIELDS: &[&str] = &[
        "schemaVersion",
        "runtime",
        "id",
        "version",
        "entry",
        "abi",
        "inject",
        "permissions",
        "servicePermissions",
        "configSchema",
        "exports",
    ];
    const CORE_FIELDS: &[&str] = &[
        "id",
        "version",
        "entry",
        "abi",
        "inject",
        "permissions",
        "configSchema",
        "exports",
    ];
    for field in object.keys() {
        if !PRODUCT_FIELDS.contains(&field.as_str()) {
            return Err(PluginError::malformed(
                path,
                format!("unknown WASM product field {field:?}"),
            ));
        }
    }
    for field in CORE_FIELDS {
        if !object.contains_key(*field) {
            return Err(PluginError::malformed(
                path,
                format!("{field} is required for a loadable WASM product"),
            ));
        }
    }
    if object.get("runtime").and_then(Value::as_str) != Some("wasm") {
        return Err(PluginError::malformed(path, "loadable product runtime must be wasm"));
    }
    if object.get("abi").and_then(Value::as_str) != Some(WASM_ABI) {
        return Err(PluginError::malformed(
            path,
            format!("abi must be {WASM_ABI}"),
        ));
    }
    let mut projected = Map::new();
    for field in CORE_FIELDS {
        projected.insert(
            (*field).to_owned(),
            object
                .get(*field)
                .expect("required product fields were checked")
                .clone(),
        );
    }
    let manifest = serde_json::from_value::<tessivum_extism::PluginManifest>(Value::Object(projected))
        .map_err(|error| PluginError::malformed(path, error))?;
    manifest
        .validate()
        .map_err(|error| PluginError::malformed(path, error))?;
    Ok(manifest)
}

#[derive(Default)]
struct PackageFiles {
    wasm_artifacts: Vec<PathBuf>,
    source_files: Vec<PathBuf>,
}

fn collect_files(
    root: &Path,
    limits: &PluginInspectionLimits,
) -> Result<PackageFiles, PluginError> {
    let mut files = PackageFiles::default();
    let mut entries = 0;
    collect_files_at(root, 0, limits, &mut entries, &mut files)?;
    files.wasm_artifacts.sort();
    files.source_files.sort();
    Ok(files)
}

fn collect_files_at(
    root: &Path,
    depth: usize,
    limits: &PluginInspectionLimits,
    entries: &mut usize,
    files: &mut PackageFiles,
) -> Result<(), PluginError> {
    for entry in fs::read_dir(root).map_err(|error| PluginError::unreadable(root, error))? {
        let entry = entry.map_err(|error| PluginError::unreadable(root, error))?;
        *entries = entries.checked_add(1).ok_or(PluginError::BudgetExceeded {
            limit: "recursive entry count",
        })?;
        if *entries > limits.max_recursive_entries {
            return Err(PluginError::BudgetExceeded {
                limit: "recursive entry count",
            });
        }
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| PluginError::unreadable(&path, error))?;
        if file_type.is_dir() {
            let name = entry.file_name();
            if matches!(name.to_str(), Some("node_modules" | ".git" | "target")) {
                continue;
            }
            if depth >= limits.max_recursive_depth {
                return Err(PluginError::BudgetExceeded {
                    limit: "recursive depth",
                });
            }
            collect_files_at(&path, depth + 1, limits, entries, files)?;
        } else if file_type.is_file() {
            if path
                .extension()
                .is_some_and(|extension| extension == "wasm")
            {
                files.wasm_artifacts.push(path.clone());
            }
            if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| SOURCE_EXTENSIONS.contains(&extension))
            {
                files.source_files.push(path);
            }
        }
    }
    Ok(())
}

fn project_dsh_client(value: &Value) -> Value {
    let Some(client) = value.as_object() else {
        return Value::Object(Map::new());
    };
    let mut projection = Map::new();
    if let Some(platform) = client.get("platform").and_then(Value::as_str) {
        projection.insert("platform".into(), Value::String(platform.into()));
    }
    if let Some(inject) = client.get("inject").and_then(Value::as_array) {
        projection.insert(
            "inject".into(),
            Value::Array(
                inject
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|name| Value::String(name.into()))
                    .collect(),
            ),
        );
    }
    if let Some(immediately) = client.get("immediately").and_then(Value::as_bool) {
        projection.insert("immediately".into(), Value::Bool(immediately));
    }
    let entry = client.get("entry").and_then(Value::as_object);
    let url = entry
        .and_then(|entry| entry.get("url"))
        .and_then(Value::as_str)
        .or_else(|| client.get("entry").and_then(Value::as_str))
        .or_else(|| client.get("entryUrl").and_then(Value::as_str));
    let hash = entry
        .and_then(|entry| entry.get("hash"))
        .and_then(Value::as_str)
        .or_else(|| client.get("entryHash").and_then(Value::as_str));
    if url.is_some() || hash.is_some() {
        let mut entry = Map::new();
        if let Some(url) = url {
            entry.insert("url".into(), Value::String(url.into()));
        }
        if let Some(hash) = hash {
            entry.insert("hash".into(), Value::String(hash.into()));
        }
        projection.insert("entry".into(), Value::Object(entry));
    }
    Value::Object(projection)
}

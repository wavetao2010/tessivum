//! Fail-closed sandbox preparation for subprocess effects.

use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tessivum_core::{ContextHandle, CoreError, ServiceHandle, ServiceKey};

use crate::{
    subprocess::{ProcessDone, Subprocess, SubprocessRequest, SubprocessRuntime},
    TessivumError,
};

/// Stable key for the sandbox policy capability.
pub fn sandbox_service_key() -> ServiceKey {
    ServiceKey::new("harness.sandbox", "1")
}

/// The maximum file-effect authority requested for one command.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// Whether explicitly approved supplemental read roots may be used.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxReadPolicy {
    Deny,
    Allow,
}

/// One explicit approval. Missing fields never broaden the request.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxApproval {
    pub mode: Option<SandboxMode>,
    pub read_policy: Option<SandboxReadPolicy>,
}

/// Untrusted sandbox intent. Paths are validated before they reach a provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRequest {
    pub mode: SandboxMode,
    pub workspace: PathBuf,
    pub read_policy: SandboxReadPolicy,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<SandboxApproval>,
}

impl SandboxRequest {
    pub fn read_only(workspace: impl Into<PathBuf>) -> Self {
        Self {
            mode: SandboxMode::ReadOnly,
            workspace: workspace.into(),
            read_policy: SandboxReadPolicy::Deny,
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            approval: None,
        }
    }
}

/// Provider-reported confinement strength. Only full enforcement can run a
/// confined command.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxEnforcement {
    Full,
    Partial,
}

/// Facts used to recognize a sandbox-runner refusal.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerRules {
    /// If absent, every nonzero exit is eligible. If present, only these codes
    /// are eligible; exit status alone is never a denial.
    pub denial_exit_codes: Option<BTreeSet<i32>>,
    /// Lines matching one of these strings exactly are informational.
    pub informational_stderr: BTreeSet<String>,
}

/// Stable denial facts supplied by a provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDenial {
    pub code: String,
    pub message: String,
}

/// A provider's complete decision: wrapped argv, enforcement claim, denial
/// facts, and its runner-recognition contract.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxPlan {
    pub argv: Vec<String>,
    pub enforcement: SandboxEnforcement,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denial: Option<SandboxDenial>,
    pub runner_rules: RunnerRules,
}

/// A validated effective request passed to sandbox providers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveSandboxRequest {
    pub mode: SandboxMode,
    pub workspace: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
}

/// Platform-specific confinement. Implementations must return the argv that
/// invokes their wrapper, never merely a policy description.
pub trait SandboxProvider: Send + Sync {
    fn confine(
        &self,
        request: &EffectiveSandboxRequest,
        argv: &[String],
    ) -> Result<SandboxPlan, TessivumError>;
}

/// A sandbox policy bound to an optional OS provider.
#[derive(Clone, Default)]
pub struct Sandbox {
    provider: Option<Arc<dyn SandboxProvider>>,
}

impl fmt::Debug for Sandbox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Sandbox")
            .field("provider", &self.provider.is_some())
            .finish()
    }
}

impl Sandbox {
    pub fn new(provider: Option<Arc<dyn SandboxProvider>>) -> Self {
        Self { provider }
    }
    /// Uses the host's native file-effect sandbox when one is available.
    pub fn local() -> Self {
        Self {
            provider: LocalSandboxProvider::detect()
                .map(|provider| Arc::new(provider) as Arc<dyn SandboxProvider>),
        }
    }

    pub fn publish(&self, context: &ContextHandle) -> Result<ServiceHandle<Sandbox>, CoreError> {
        context.provide(sandbox_service_key(), self.clone())
    }

    /// Validates the request and produces a provider-wrapped argv. Danger mode
    /// is an explicit bypass; every other mode requires full enforcement.
    pub fn prepare(
        &self,
        request: &SandboxRequest,
        argv: &[String],
    ) -> Result<SandboxPlan, TessivumError> {
        validate_argv(argv)?;
        let effective = effective_request(request)?;
        if effective.mode == SandboxMode::DangerFullAccess {
            return Ok(SandboxPlan {
                argv: argv.to_vec(),
                enforcement: SandboxEnforcement::Full,
                denial: None,
                runner_rules: RunnerRules::default(),
            });
        }
        let provider = self.provider.as_ref().ok_or_else(sandbox_unavailable)?;
        let plan = provider
            .confine(&effective, argv)
            .map_err(provider_failure)?;
        validate_plan(&plan)?;
        if plan.enforcement != SandboxEnforcement::Full {
            return Err(sandbox_unavailable());
        }
        Ok(plan)
    }

    /// Spawns only the wrapped provider argv. A missing, refusing, or partial
    /// provider returns before the subprocess runtime can see the raw argv.
    pub async fn spawn(
        &self,
        runtime: &SubprocessRuntime,
        request: &SandboxRequest,
        mut process: SubprocessRequest,
    ) -> Result<SandboxedProcess, TessivumError> {
        let plan = self.prepare(request, &process.argv)?;
        if let Some(denial) = &plan.denial {
            // An immediate refusal has no runner output to classify.
            if plan.argv.is_empty() {
                return Err(denial_error(denial, json!({"stage": "provider"})));
            }
        }
        process.argv = plan.argv.clone();
        let process = runtime.spawn(process).await?;
        Ok(SandboxedProcess { process, plan })
    }
}

/// A background confined process. Runner recognition is performed only after
/// completion and before a provider denial is reported.
#[derive(Clone, Debug)]
pub struct SandboxedProcess {
    process: Subprocess,
    plan: SandboxPlan,
}

impl SandboxedProcess {
    pub fn done(&self) -> Option<ProcessDone> {
        self.process.done()
    }

    pub fn process(&self) -> &Subprocess {
        &self.process
    }

    pub async fn wait(&self) -> Result<ProcessDone, TessivumError> {
        let done = self.process.wait().await;
        self.classify(done).await
    }

    async fn classify(&self, done: ProcessDone) -> Result<ProcessDone, TessivumError> {
        let Some(denial) = &self.plan.denial else {
            return Ok(done);
        };
        let stderr = self
            .process
            .read_stderr(0, 16 * 1024 * 1024)
            .await
            .map(|read| read.bytes)
            .unwrap_or_default();
        if runner_denied(&self.plan.runner_rules, &done, &stderr) {
            return Err(denial_error(
                denial,
                json!({
                    "exitCode": done.exit_code,
                    "signal": done.signal,
                    "termination": done.termination,
                }),
            ));
        }
        Ok(done)
    }
}

/// Checks runner exit and stderr together. A nonzero exit with no fatal stderr
/// is not a sandbox denial, and informational exclusion is exact-line only.
pub fn runner_denied(rules: &RunnerRules, done: &ProcessDone, stderr: &[u8]) -> bool {
    if done.termination.is_some() {
        return false;
    }
    let Some(exit_code) = done.exit_code else {
        return false;
    };
    let eligible_exit = match &rules.denial_exit_codes {
        Some(codes) => codes.contains(&exit_code),
        None => exit_code != 0,
    };
    if !eligible_exit {
        return false;
    }
    String::from_utf8_lossy(stderr)
        .lines()
        .any(|line| !line.is_empty() && !rules.informational_stderr.contains(line))
}

fn effective_request(request: &SandboxRequest) -> Result<EffectiveSandboxRequest, TessivumError> {
    let workspace = canonical_directory(&request.workspace, "workspace")?;
    let approval = request.approval.as_ref();
    let mode = match request.mode {
        SandboxMode::DangerFullAccess => SandboxMode::DangerFullAccess,
        SandboxMode::ReadOnly => SandboxMode::ReadOnly,
        SandboxMode::WorkspaceWrite => {
            if approval.is_some_and(|approval| approval.mode == Some(SandboxMode::WorkspaceWrite)) {
                SandboxMode::WorkspaceWrite
            } else {
                SandboxMode::ReadOnly
            }
        }
    };
    let reads_allowed = request.read_policy == SandboxReadPolicy::Allow
        && approval.is_some_and(|approval| approval.read_policy == Some(SandboxReadPolicy::Allow));
    let mut read_roots = if reads_allowed {
        canonical_roots(&request.read_roots, "read root")?
    } else {
        Vec::new()
    };
    let write_roots = if mode == SandboxMode::WorkspaceWrite {
        let roots = canonical_roots(&request.write_roots, "write root")?;
        for root in &roots {
            if !root.starts_with(&workspace) {
                return Err(sandbox_error(
                    "SANDBOX_INVALID_PATH",
                    "workspace-write root must be inside the workspace",
                    json!({"root": root.display().to_string(), "workspace": workspace.display().to_string()}),
                ));
            }
        }
        roots
    } else {
        Vec::new()
    };
    // The workspace itself is always readable to a confined command; external
    // reads must pass the explicit approved policy above.
    if !read_roots.contains(&workspace) {
        read_roots.insert(0, workspace.clone());
    }
    Ok(EffectiveSandboxRequest {
        mode,
        workspace,
        read_roots,
        write_roots,
    })
}

fn canonical_roots(roots: &[PathBuf], label: &str) -> Result<Vec<PathBuf>, TessivumError> {
    let mut canonical = Vec::new();
    for root in roots {
        let root = canonical_directory(root, label)?;
        if !canonical.contains(&root) {
            canonical.push(root);
        }
    }
    Ok(canonical)
}

#[derive(Clone, Debug)]
struct LocalSandboxProvider {
    runner: String,
}

impl LocalSandboxProvider {
    fn detect() -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            let runner = Path::new("/usr/bin/sandbox-exec");
            return runner.is_file().then(|| Self {
                runner: runner.display().to_string(),
            });
        }
        #[cfg(target_os = "linux")]
        {
            return executable("bwrap").map(|runner| Self { runner });
        }
        #[allow(unreachable_code)]
        None
    }
}

impl SandboxProvider for LocalSandboxProvider {
    fn confine(
        &self,
        request: &EffectiveSandboxRequest,
        argv: &[String],
    ) -> Result<SandboxPlan, TessivumError> {
        #[cfg(target_os = "macos")]
        let wrapped = {
            let mut forms = vec![
                "(version 1)".to_owned(),
                "(allow default)".to_owned(),
                "(deny file-write*)".to_owned(),
                format!(
                    "(allow file-write* (literal {}))",
                    sbpl_string(Path::new("/dev/null"))
                ),
            ];
            if request.mode == SandboxMode::WorkspaceWrite {
                forms.push(format!(
                    "(allow file-write* {})",
                    request
                        .write_roots
                        .iter()
                        .map(|root| format!("(subpath {})", sbpl_string(root)))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
                forms.push(format!(
                    "(allow file-write* (subpath {}) (subpath {}))",
                    sbpl_string(Path::new("/private/tmp")),
                    sbpl_string(&std::env::temp_dir()),
                ));
            }
            let mut wrapped = vec![
                self.runner.clone(),
                "-p".into(),
                forms.join(" "),
                "--".into(),
            ];
            wrapped.extend_from_slice(argv);
            wrapped
        };
        #[cfg(target_os = "linux")]
        let wrapped = {
            let mut wrapped = vec![
                self.runner.clone(),
                "--ro-bind".into(),
                "/".into(),
                "/".into(),
                "--dev".into(),
                "/dev".into(),
                "--proc".into(),
                "/proc".into(),
                "--die-with-parent".into(),
            ];
            if request.mode == SandboxMode::WorkspaceWrite {
                wrapped.extend(["--tmpfs".into(), "/tmp".into()]);
                for root in &request.write_roots {
                    let root = root.display().to_string();
                    wrapped.extend(["--bind".into(), root.clone(), root]);
                }
            }
            wrapped.push("--".into());
            wrapped.extend_from_slice(argv);
            wrapped
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let wrapped = argv.to_vec();
        Ok(SandboxPlan {
            argv: wrapped,
            enforcement: SandboxEnforcement::Full,
            denial: None,
            runner_rules: RunnerRules::default(),
        })
    }
}

#[cfg(target_os = "macos")]
fn sbpl_string(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('\"', "\\\"")
    )
}

#[cfg(target_os = "linux")]
fn executable(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH");
    path.as_deref()
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.display().to_string())
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, TessivumError> {
    if !path.is_absolute() {
        return Err(sandbox_error(
            "SANDBOX_INVALID_PATH",
            "sandbox path must be absolute",
            json!({"label": label, "path": path.display().to_string()}),
        ));
    }
    let path = std::fs::canonicalize(path).map_err(|error| {
        sandbox_error(
            "SANDBOX_INVALID_PATH",
            "sandbox path cannot be resolved",
            json!({"label": label, "path": path.display().to_string(), "error": error.to_string()}),
        )
    })?;
    if path.is_dir() {
        Ok(path)
    } else {
        Err(sandbox_error(
            "SANDBOX_INVALID_PATH",
            "sandbox path must be a directory",
            json!({"label": label, "path": path.display().to_string()}),
        ))
    }
}

fn validate_argv(argv: &[String]) -> Result<(), TessivumError> {
    if argv.first().is_none_or(|program| program.is_empty())
        || argv.iter().any(|arg| arg.contains('\0'))
    {
        return Err(sandbox_error(
            "SANDBOX_INVALID_ARGV",
            "sandbox argv must contain a non-empty NUL-free program",
            json!({}),
        ));
    }
    Ok(())
}

fn validate_plan(plan: &SandboxPlan) -> Result<(), TessivumError> {
    if plan.argv.first().is_none_or(|program| program.is_empty())
        || plan.argv.iter().any(|arg| arg.contains('\0'))
    {
        return Err(sandbox_unavailable());
    }
    if let Some(denial) = &plan.denial {
        if denial.code.trim().is_empty() || denial.message.trim().is_empty() {
            return Err(sandbox_unavailable());
        }
    }
    Ok(())
}

fn provider_failure(error: TessivumError) -> TessivumError {
    if error.code == "SANDBOX_DENIED" {
        error
    } else {
        sandbox_unavailable()
    }
}

fn denial_error(denial: &SandboxDenial, details: serde_json::Value) -> TessivumError {
    TessivumError::new("SANDBOX_DENIED", denial.message.clone(), "sandbox", details)
}

fn sandbox_unavailable() -> TessivumError {
    sandbox_error(
        "SANDBOX_UNAVAILABLE",
        "requested sandbox confinement is unavailable",
        json!({}),
    )
}

fn sandbox_error(code: &str, message: &str, details: serde_json::Value) -> TessivumError {
    TessivumError::new(code, message, "sandbox", details)
}

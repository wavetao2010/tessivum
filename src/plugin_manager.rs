use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::{self, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command as TokioCommand,
    sync::mpsc,
};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tessivum_core::{CancellationToken, Entry, EntryId, EntryOptions, EntryTree, RuntimeKind};
use tessivum_node_bridge::{BridgeError, ClientConfig, HostCommand};
use thiserror::Error;

use crate::{
    host::{HostConfig, LegacyHostConfig},
    legacy::ProductPackageResolver,
    plugins::{assignment_strings, PluginRouter, PluginRuntime},
};

const MAX_PROFILE_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_PROFILE_LOCK_BYTES: u64 = 8 * 1024 * 1024;
const MAX_BUNDLE_PATCH_BYTES: u64 = 1024 * 1024;
const MAX_PACKAGE_ENTRY_BYTES: u64 = 2 * 1024 * 1024;
const DSH_COMPATIBILITY_BASELINE: &str = "0.1.0-rc.5";
const PLUGIN_DSH_ENGINE_UNSUPPORTED: &str = "PLUGIN_DSH_ENGINE_UNSUPPORTED";
const PLUGIN_PACKAGE_ENTRY_INVALID: &str = "PLUGIN_PACKAGE_ENTRY_INVALID";
const PLUGIN_BUNDLE_PATCH_INVALID: &str = "PLUGIN_BUNDLE_PATCH_INVALID";
const PLUGIN_RUNTIME_DEPENDENCY_MISSING: &str = "PLUGIN_RUNTIME_DEPENDENCY_MISSING";
const PLUGIN_CLIENT_ENTRY_INVALID: &str = "PLUGIN_CLIENT_ENTRY_INVALID";
const PLUGIN_INJECT_UNAVAILABLE: &str = "PLUGIN_INJECT_UNAVAILABLE";
const PLUGIN_MUTATION_ROLLBACK_FAILED: &str = "PLUGIN_MUTATION_ROLLBACK_FAILED";
const FIRST_PARTY_MARKET_NAME: &str = "tessivum-market";
const FIRST_PARTY_MARKET_VERSION: &str = env!("CARGO_PKG_VERSION");
const FIRST_PARTY_MARKET_TARBALL: &str =
    concat!("tessivum-market-", env!("CARGO_PKG_VERSION"), ".tgz");
const MAX_FIRST_PARTY_MARKET_TARBALL_BYTES: u64 = 64 * 1024 * 1024;
const RETIRED_REMOTE_WEB_UI_NAME: &str = "@linxin666/dsh-remote-web-ui";

#[derive(Debug, Error)]
pub enum PluginManagerError {
    #[error("plugin profile I/O failed at {path}: {reason}")]
    Io { path: PathBuf, reason: String },
    #[error("plugin profile is invalid: {0}")]
    Invalid(String),
    #[error("plugin profile is busy: {0}")]
    Busy(PathBuf),
    #[error("plugin mutation failed: {reason}; partial state: {partial_state}")]
    Mutation {
        reason: String,
        partial_state: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginMutation {
    Add(String),
    Remove(String),
}

const PNPM_OPERATION_TIMEOUT: Duration = Duration::from_secs(16 * 60);

/// Executes one compatibility-host package operation inside its owned profile.
pub struct PnpmProfileBoundary {
    profile: PathBuf,
}

impl PnpmProfileBoundary {
    pub fn new(profile: impl AsRef<Path>) -> Result<Self, PluginManagerError> {
        let profile = profile.as_ref();
        fs::create_dir_all(profile).map_err(|error| io_error(profile, error))?;
        Ok(Self {
            profile: fs::canonicalize(profile).map_err(|error| io_error(profile, error))?,
        })
    }
}

#[async_trait::async_trait]
impl crate::bridge::PnpmBoundary for PnpmProfileBoundary {
    async fn run(
        &self,
        request: crate::bridge::PnpmRunRequest,
        cancellation: CancellationToken,
        sink: crate::bridge::PnpmOutputSink,
    ) -> Result<crate::bridge::PnpmRunResult, BridgeError> {
        if let Some(mutation) = pnpm_request_mutation(&request.args) {
            reject_known_unsupported_mutation(&mutation)
                .map_err(|error| BridgeError::Process(error.to_string()))?;
        }
        let profile = self.profile.clone();
        let request_args = request.args.clone();
        let (_lock, transaction, snapshot, args) = tokio::task::spawn_blocking({
            let profile = profile.clone();
            move || {
                let lock = ProfileLock::acquire(&profile)?;
                let transaction = ProfileMutationSnapshot::capture(&profile)?;
                let prepared = (|| {
                    if let Some(mutation) = pnpm_request_mutation(&request_args) {
                        preflight_materialized_candidate(&profile, &mutation)?;
                    }
                    ensure_profile(&profile)?;
                    let args = route_pnpm_args(&request_args, &profile)
                        .map_err(|error| PluginManagerError::Invalid(error.to_string()))?;
                    let snapshot = ProfileSnapshot::capture(&load_profile(&profile, true)?);
                    Ok::<_, PluginManagerError>((snapshot, args))
                })();
                match prepared {
                    Ok((snapshot, args)) => Ok((lock, transaction, snapshot, args)),
                    Err(error) => match rollback_generic_mutation(&profile, &transaction, false) {
                        Ok(()) => Err(error),
                        Err(recovery) => Err(PluginManagerError::Invalid(format!(
                            "{PLUGIN_MUTATION_ROLLBACK_FAILED}: {error}; recovery proof failed: {recovery}"
                        ))),
                    },
                }
            }
        })
        .await
        .map_err(|error| BridgeError::Process(format!("pnpm lock worker failed: {error}")))?
        .map_err(|error| match error {
            PluginManagerError::Busy(_) => {
                BridgeError::Process("another desktop pnpm operation is already running".into())
            }
            error => BridgeError::Process(error.to_string()),
        })?;
        let reconciliation = reconciliation_mode(&args);
        let mut command = TokioCommand::new("pnpm");
        command
            .current_dir(&profile)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Err(error) = set_process_group(&mut command) {
            return Err(restore_pnpm_boundary_failure(&profile, &transaction, false, error).await);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Err(restore_pnpm_boundary_failure(
                    &profile,
                    &transaction,
                    false,
                    BridgeError::Process(format!("could not run pnpm: {error}")),
                )
                .await);
            }
        };
        let (sender, mut receiver) = mpsc::channel(8);
        let stdout = child.stdout.take().expect("piped stdout exists");
        let stderr = child.stderr.take().expect("piped stderr exists");
        let stdout_task = tokio::spawn(pump_pnpm_output(
            stdout,
            crate::bridge::PnpmOutputStream::Stdout,
            sender.clone(),
        ));
        let stderr_task = tokio::spawn(pump_pnpm_output(
            stderr,
            crate::bridge::PnpmOutputStream::Stderr,
            sender,
        ));
        let deadline = tokio::time::sleep(PNPM_OPERATION_TIMEOUT);
        tokio::pin!(deadline);
        let mut ended = 0;
        let mut failure = None;
        while ended < 2 {
            tokio::select! {
                _ = cancellation.cancelled() => { failure = Some(BridgeError::Cancelled); break; }
                _ = &mut deadline => { failure = Some(BridgeError::Timeout); break; }
                item = receiver.recv() => match item {
                    Some(Some((stream, bytes))) => if let Err(error) = sink.emit(stream, &bytes) { failure = Some(error); break; },
                    Some(None) => ended += 1,
                    None => break,
                },
            }
        }
        if failure.is_some() {
            stop_process_group(&mut child).await;
        }
        let status = match child.wait().await {
            Ok(status) => status,
            Err(error) => {
                return Err(restore_pnpm_boundary_failure(
                    &profile,
                    &transaction,
                    true,
                    BridgeError::Process(format!("could not wait for pnpm: {error}")),
                )
                .await);
            }
        };
        for task in [stdout_task, stderr_task] {
            let output = task
                .await
                .map_err(|error| {
                    BridgeError::Process(format!("pnpm output worker failed: {error}"))
                })
                .and_then(|result| {
                    result.map_err(|error| {
                        BridgeError::Process(format!("could not read pnpm output: {error}"))
                    })
                });
            if let Err(error) = output {
                return Err(
                    restore_pnpm_boundary_failure(&profile, &transaction, true, error).await,
                );
            }
        }
        if let Some(error) = failure {
            return Err(restore_pnpm_boundary_failure(&profile, &transaction, true, error).await);
        }
        if !status.success() {
            let diagnostic = format!(
                "tessivum: pnpm exited with {}; partial state: {}\n",
                status
                    .code()
                    .map_or_else(|| "no exit code".into(), |code| code.to_string(),),
                pnpm_partial_state(&profile, &request.args),
            );
            let _ = sink.emit(
                crate::bridge::PnpmOutputStream::Stderr,
                diagnostic.as_bytes(),
            );
            if let Err(error) = tokio::task::spawn_blocking({
                let profile = profile.clone();
                let transaction = transaction.clone();
                move || rollback_generic_mutation(&profile, &transaction, true)
            })
            .await
            .map_err(|error| BridgeError::Process(format!("rollback worker failed: {error}")))
            .and_then(|result| result.map_err(|error| BridgeError::Process(error.to_string())))
            {
                return Err(BridgeError::Process(format!(
                    "{PLUGIN_MUTATION_ROLLBACK_FAILED}: pnpm exited unsuccessfully; recovery proof failed: {error}"
                )));
            }
        } else {
            let validated = tokio::task::spawn_blocking({
                let profile = profile.clone();
                let args = args.clone();
                move || {
                    validate_pnpm_candidate(&profile, &args)?;
                    reconcile_profile(&profile, &snapshot, reconciliation)?;
                    remove_legacy_package_lock(&profile)
                }
            })
            .await
            .map_err(|error| {
                BridgeError::Process(format!("profile reconciliation worker failed: {error}"))
            })
            .and_then(|result| {
                result.map_err(|error| {
                    BridgeError::Process(format!(
                        "pnpm completed but Profile activation record was not committed: {error}"
                    ))
                })
            });
            if let Err(error) = validated {
                return Err(
                    restore_pnpm_boundary_failure(&profile, &transaction, true, error).await,
                );
            }
        }
        Ok(crate::bridge::PnpmRunResult {
            exit_code: status.code(),
            signal: exit_signal(&status),
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}
async fn restore_pnpm_boundary_failure(
    profile: &Path,
    snapshot: &ProfileMutationSnapshot,
    pnpm_started: bool,
    error: BridgeError,
) -> BridgeError {
    let profile = profile.to_path_buf();
    let snapshot = snapshot.clone();
    match tokio::task::spawn_blocking(move || {
        rollback_generic_mutation(&profile, &snapshot, pnpm_started)
    })
    .await
    {
        Ok(Ok(())) => error,
        Ok(Err(recovery)) => BridgeError::Process(format!(
            "{PLUGIN_MUTATION_ROLLBACK_FAILED}: {error}; recovery proof failed: {recovery}"
        )),
        Err(recovery) => BridgeError::Process(format!(
            "{PLUGIN_MUTATION_ROLLBACK_FAILED}: {error}; rollback worker failed: {recovery}"
        )),
    }
}

fn pnpm_request_mutation(args: &[String]) -> Option<PluginMutation> {
    let command = args
        .iter()
        .find(|argument| matches!(argument.as_str(), "add" | "remove"))?;
    let target = args.iter().find(|argument| {
        !matches!(argument.as_str(), "add" | "remove" | "install")
            && !MARKET_PNPM_FLAGS.contains(&argument.as_str())
    })?;
    Some(if command == "add" {
        PluginMutation::Add(target.clone())
    } else {
        PluginMutation::Remove(target.clone())
    })
}

fn validate_pnpm_candidate(profile: &Path, args: &[String]) -> Result<(), PluginManagerError> {
    match pnpm_request_mutation(args) {
        Some(PluginMutation::Add(specifier)) => {
            validate_mutation_candidate(profile, &PluginMutation::Add(specifier))
        }
        _ => Ok(()),
    }
}

async fn pump_pnpm_output<R: AsyncRead + Unpin>(
    mut reader: R,
    stream: crate::bridge::PnpmOutputStream,
    sender: mpsc::Sender<Option<(crate::bridge::PnpmOutputStream, Vec<u8>)>>,
) -> io::Result<()> {
    let mut buffer = [0; 16 * 1024];
    loop {
        let length = reader.read(&mut buffer).await?;
        if length == 0 {
            let _ = sender.send(None).await;
            return Ok(());
        }
        if sender
            .send(Some((stream, buffer[..length].to_vec())))
            .await
            .is_err()
        {
            return Ok(());
        }
    }
}

const MARKET_PNPM_FLAGS: [&str; 6] = [
    "-w",
    "--reporter=ndjson",
    "--no-frozen-lockfile",
    "--config.minimumReleaseAge=0",
    "--config.fetchTimeout=600000",
    "--config.auto-install-peers=false",
];

pub(crate) fn validate_market_pnpm_args(args: &[String]) -> Result<(), &'static str> {
    if args.is_empty() || args.len() > 64 {
        return Err("args must contain one bounded command");
    }
    let mut command = None;
    let mut target = None;
    let mut flags = BTreeSet::new();
    for argument in args {
        match argument.as_str() {
            value @ ("add" | "remove" | "install") if command.is_none() => command = Some(value),
            value if MARKET_PNPM_FLAGS.contains(&value) => {
                if !flags.insert(value) {
                    return Err("pnpm flags must not be duplicated");
                }
            }
            value if value.starts_with('-') => return Err("a pnpm flag is not permitted"),
            value => {
                if value.is_empty()
                    || value.len() > 512
                    || !value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric()
                            || matches!(
                                byte,
                                b'@' | b':'
                                    | b'.'
                                    | b'/'
                                    | b'_'
                                    | b'#'
                                    | b'+'
                                    | b'~'
                                    | b'^'
                                    | b'='
                                    | b'-'
                            )
                    })
                    || Path::new(value).is_absolute()
                    || Path::new(value).components().any(|part| {
                        matches!(
                            part,
                            std::path::Component::ParentDir
                                | std::path::Component::CurDir
                                | std::path::Component::RootDir
                        )
                    })
                    || target.replace(value).is_some()
                {
                    return Err("a pnpm package target is invalid");
                }
            }
        }
    }
    let command = command.ok_or("pnpm command is missing")?;
    if (flags.contains("-w") && !matches!(command, "add" | "remove"))
        || (matches!(command, "add" | "remove") != target.is_some())
        || (command == "install" && target.is_some())
    {
        return Err("pnpm command arguments are not permitted");
    }
    Ok(())
}

fn route_pnpm_args(args: &[String], profile: &Path) -> Result<Vec<String>, BridgeError> {
    validate_market_pnpm_args(args).map_err(|message| BridgeError::Process(message.into()))?;
    let command = args
        .iter()
        .find(|argument| matches!(argument.as_str(), "add" | "remove" | "install"))
        .expect("validated pnpm args contain a command");
    let target = args.iter().find(|argument| {
        !matches!(argument.as_str(), "add" | "remove" | "install")
            && !MARKET_PNPM_FLAGS.contains(&argument.as_str())
    });
    let mut result = Vec::with_capacity(8);
    result.push(command.clone());
    if args.iter().any(|argument| argument == "-w") {
        result.push("-w".into());
    }
    if let Some(target) = target {
        result.push((*target).clone());
    }
    for flag in &MARKET_PNPM_FLAGS[2..5] {
        if args.iter().any(|argument| argument == flag) {
            result.push((*flag).into());
        }
    }
    result.push("--reporter=ndjson".into());
    result.push("--config.auto-install-peers=false".into());
    if !profile_allows_builds(profile)? {
        result.push(if command == "remove" {
            "--config.ignore-scripts=true".into()
        } else {
            "--ignore-scripts".into()
        });
    }
    Ok(result)
}

fn profile_allows_builds(profile: &Path) -> Result<bool, BridgeError> {
    let manifest = read_json(&profile.join("package.json"), MAX_PROFILE_MANIFEST_BYTES)
        .map_err(|error| BridgeError::Process(error.to_string()))?;
    Ok(manifest
        .pointer("/pnpm/onlyBuiltDependencies")
        .and_then(Value::as_array)
        .is_some_and(|list| !list.is_empty() && list.iter().all(Value::is_string)))
}

fn pnpm_partial_state(profile: &Path, args: &[String]) -> String {
    let command = args
        .iter()
        .find(|argument| matches!(argument.as_str(), "add" | "remove" | "install"))
        .map(String::as_str);
    let target = args.iter().find(|argument| {
        !matches!(argument.as_str(), "add" | "remove" | "install")
            && !MARKET_PNPM_FLAGS.contains(&argument.as_str())
    });
    match (command, target) {
        (Some("add"), Some(target)) => {
            mutation_partial_state(profile, &PluginMutation::Add((*target).clone()))
        }
        (Some("remove"), Some(target)) => {
            mutation_partial_state(profile, &PluginMutation::Remove((*target).clone()))
        }
        _ => {
            let (manifest, lock) = profile_document_state(profile);
            format!(
                "package.json={manifest}; pnpm-lock.yaml={lock}; target package entry=not applicable"
            )
        }
    }
}

fn profile_document_state(profile: &Path) -> (String, String) {
    let manifest = match read_json(&profile.join("package.json"), MAX_PROFILE_MANIFEST_BYTES) {
        Ok(manifest)
            if manifest
                .get("dependencies")
                .and_then(Value::as_object)
                .is_some() =>
        {
            "valid".into()
        }
        Ok(_) => "invalid dependencies".into(),
        Err(error) => format!("invalid ({error})"),
    };
    let lock = match read_bounded(&profile.join("pnpm-lock.yaml"), MAX_PROFILE_LOCK_BYTES) {
        Ok(lock) => match serde_yaml::from_slice::<serde_yaml::Value>(&lock) {
            Ok(lock) if lock.is_mapping() => "valid".into(),
            Ok(_) => "invalid root".into(),
            Err(error) => format!("invalid ({error})"),
        },
        Err(error) => format!("invalid ({error})"),
    };
    (manifest, lock)
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| signal.to_string())
}

#[cfg(not(unix))]
fn exit_signal(_: &std::process::ExitStatus) -> Option<String> {
    None
}

#[cfg(unix)]
fn set_process_group(command: &mut TokioCommand) -> Result<(), BridgeError> {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.as_std_mut().pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    Ok(())
}
#[cfg(not(unix))]
fn set_process_group(_: &mut TokioCommand) -> Result<(), BridgeError> {
    Ok(())
}

async fn stop_process_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(id) = child.id() {
        let group = -(id as libc::pid_t);
        unsafe {
            libc::kill(group, libc::SIGTERM);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
}

#[derive(Deserialize, Serialize)]
struct ProfileLockOwner {
    pid: u32,
    nonce: String,
}

struct ProfileLock {
    path: PathBuf,
    nonce: String,
}

pub fn mutate_plugins(
    data_dir: impl AsRef<Path>,
    mutation: PluginMutation,
) -> Result<(), PluginManagerError> {
    reject_known_unsupported_mutation(&mutation)?;
    let profile = plugin_profile_root(data_dir);
    fs::create_dir_all(&profile).map_err(|error| io_error(&profile, error))?;
    let _lock = ProfileLock::acquire(&profile)?;
    let transaction = ProfileMutationSnapshot::capture(&profile)?;
    let mut pnpm_started = false;
    let result = (|| {
        preflight_materialized_candidate(&profile, &mutation)?;
        ensure_profile(&profile)?;
        let snapshot = ProfileSnapshot::capture(&load_profile(&profile, true)?);
        let cwd = env::current_dir().map_err(|error| io_error(".", error))?;
        let argument = match &mutation {
            PluginMutation::Add(specifier) => anchor_path_spec(specifier, &cwd)?,
            PluginMutation::Remove(package) => package.clone(),
        };
        run_generic_mutation_pnpm(
            &profile,
            pnpm_arguments(&mutation),
            &argument,
            &mut pnpm_started,
        )?;
        validate_mutation_candidate(&profile, &mutation)?;
        reconcile_profile(&profile, &snapshot, ReconciliationMode::Mutation)?;
        remove_legacy_package_lock(&profile)
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(restore_generic_mutation_failure(
            &profile,
            &mutation,
            &transaction,
            pnpm_started,
            error,
        )),
    }
}
/// Installs the release-owned market tarball into the profile without retaining a
/// reference to the release directory.
pub fn install_first_party_market(
    data_dir: impl AsRef<Path>,
    tarball: impl AsRef<Path>,
    expected_sha256: impl AsRef<str>,
) -> Result<(), PluginManagerError> {
    let data_dir = data_dir.as_ref();
    let artifact =
        copy_first_party_market_artifact(data_dir, tarball.as_ref(), expected_sha256.as_ref())?;
    let profile = plugin_profile_root(data_dir);
    fs::create_dir_all(&profile).map_err(|error| io_error(&profile, error))?;
    let _lock = ProfileLock::acquire(&profile)?;
    let snapshot = FirstPartyMarketSnapshot::capture(&profile)?;
    let mut pnpm_started = false;
    let mut migrated_dependencies = Vec::new();
    let mut previous_bundles = None;

    let result = {
        let mut install = || -> Result<(), PluginManagerError> {
            ensure_profile(&profile)?;
            let document = read_profile_document(&profile)?;
            let migration = FirstPartyMarketMigration::discover(&profile, &document)?;
            migrated_dependencies = migration.migrated_dependencies().cloned().collect();
            previous_bundles = Some(migration.bundles.clone());
            if first_party_market_is_current(&profile, &document, &artifact, &migration)? {
                return Ok(());
            }

            run_first_party_market_pnpm(
                &profile,
                FirstPartyMarketPnpm::Add(&artifact),
                Some(&mut pnpm_started),
            )?;
            write_first_party_market_manifest(&profile, &artifact, &migration, false)?;
            validate_first_party_market_installation(&profile)?;

            for dependency in migration.migrated_dependencies() {
                run_first_party_market_pnpm(
                    &profile,
                    FirstPartyMarketPnpm::Remove(&dependency.name),
                    Some(&mut pnpm_started),
                )?;
            }
            write_first_party_market_manifest(&profile, &artifact, &migration, true)?;
            for dependency in migration.migrated_dependencies() {
                validate_market_package_removed(&profile, &dependency.name)?;
            }
            validate_first_party_market_installation(&profile)
        };

        install()
    };
    match result {
        Ok(_) => Ok(()),
        Err(error) => match rollback_first_party_market(
            &profile,
            &snapshot,
            pnpm_started,
            &migrated_dependencies,
            previous_bundles.as_deref(),
        ) {
            Ok(()) => Err(PluginManagerError::Mutation {
                reason: format!("first-party market installation failed and the profile was restored: {error}"),
                partial_state: first_party_market_partial_state(&profile),
            }),
            Err(recovery) => Err(PluginManagerError::Mutation {
                reason: format!(
                    "first-party market installation failed: {error}; recovery could not be proven: {recovery}"
                ),
                partial_state: first_party_market_partial_state(&profile),
            }),
        },
    }
}

impl ProfileLock {
    fn acquire(profile: &Path) -> Result<Self, PluginManagerError> {
        let path = profile.join(".tessivum-profile.lock");
        let nonce = uuid::Uuid::new_v4().to_string();
        let mut stale_locks = Vec::new();

        loop {
            match fs::create_dir(&path) {
                Ok(()) => {
                    let owner = ProfileLockOwner {
                        pid: process::id(),
                        nonce: nonce.clone(),
                    };
                    let owner_path = path.join("owner.json");
                    let owner_json = serde_json::to_vec(&owner).map_err(|error| {
                        PluginManagerError::Invalid(format!(
                            "profile lock owner cannot be encoded: {error}"
                        ))
                    })?;
                    if let Err(error) = fs::write(&owner_path, owner_json) {
                        let _ = fs::remove_dir_all(&path);
                        return Err(io_error(owner_path, error));
                    }

                    let lock = Self {
                        path: path.clone(),
                        nonce,
                    };
                    for stale in stale_locks {
                        fs::remove_dir_all(&stale).map_err(|error| io_error(stale, error))?;
                    }
                    return Ok(lock);
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let owner_path = path.join("owner.json");
                    let owner = match fs::metadata(&owner_path) {
                        Ok(_) => {
                            let bytes = read_bounded(&owner_path, MAX_PROFILE_MANIFEST_BYTES)?;
                            serde_json::from_slice::<ProfileLockOwner>(&bytes).map_err(|error| {
                                PluginManagerError::Invalid(format!(
                                    "{} is invalid: {error}",
                                    owner_path.display()
                                ))
                            })?
                        }
                        Err(error) if error.kind() == ErrorKind::NotFound => {
                            return Err(PluginManagerError::Busy(path.clone()));
                        }
                        Err(error) => return Err(io_error(owner_path, error)),
                    };
                    if profile_lock_owner_is_active(owner.pid) {
                        return Err(PluginManagerError::Busy(path.clone()));
                    }

                    let stale =
                        profile.join(format!(".tessivum-profile.lock.stale-{}", owner.nonce));
                    match fs::rename(&path, &stale) {
                        Ok(()) => stale_locks.push(stale),
                        Err(error)
                            if matches!(
                                error.kind(),
                                ErrorKind::NotFound
                                    | ErrorKind::AlreadyExists
                                    | ErrorKind::DirectoryNotEmpty
                            ) => {}
                        Err(error) => return Err(io_error(&path, error)),
                    }
                }
                Err(error) => return Err(io_error(&path, error)),
            }
        }
    }
}

impl Drop for ProfileLock {
    fn drop(&mut self) {
        let owner_path = self.path.join("owner.json");
        let Some(owner) = fs::read(&owner_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ProfileLockOwner>(&bytes).ok())
        else {
            return;
        };
        if owner.nonce != self.nonce {
            return;
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn profile_lock_owner_is_active(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn profile_lock_owner_is_active(pid: u32) -> bool {
    pid != 0
}

fn pnpm_arguments(mutation: &PluginMutation) -> &'static [&'static str] {
    match mutation {
        PluginMutation::Add(_) => &["add", "--save-exact", "--ignore-scripts"],
        PluginMutation::Remove(_) => &["remove", "--config.ignore-scripts=true"],
    }
}

fn remove_legacy_package_lock(profile: &Path) -> Result<(), PluginManagerError> {
    let lock = profile.join("package-lock.json");
    match fs::remove_file(&lock) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(lock, error)),
    }
}

fn mutation_error(reason: String, profile: &Path, mutation: &PluginMutation) -> PluginManagerError {
    PluginManagerError::Mutation {
        reason,
        partial_state: mutation_partial_state(profile, mutation),
    }
}

fn mutation_partial_state(profile: &Path, mutation: &PluginMutation) -> String {
    let (manifest_state, lock_state) = profile_document_state(profile);
    format!(
        "package.json={manifest_state}; pnpm-lock.yaml={lock_state}; target package entry={}",
        mutation_target_entry_state(profile, mutation)
    )
}

fn mutation_target_entry_state(profile: &Path, mutation: &PluginMutation) -> String {
    let requested = match mutation {
        PluginMutation::Add(specifier) => add_package_name(specifier),
        PluginMutation::Remove(package) => Some(package.as_str()),
    };
    let Some(package) = requested else {
        return "unresolved".into();
    };
    let root = match package_root(profile, package) {
        Ok(root) => root,
        Err(error) => return format!("invalid ({error})"),
    };
    let manifest = root.join("package.json");
    if !manifest.exists() {
        return format!("{package}=absent");
    }
    match read_json(&manifest, MAX_PROFILE_MANIFEST_BYTES) {
        Ok(manifest) if manifest.get("name").and_then(Value::as_str) == Some(package) => {
            format!("{package}=valid")
        }
        Ok(_) => format!("{package}=invalid manifest"),
        Err(error) => format!("{package}=invalid ({error})"),
    }
}

fn add_package_name(specifier: &str) -> Option<&str> {
    if is_local_package_specifier(specifier) {
        return None;
    }
    if let Some(scoped) = specifier.strip_prefix('@') {
        let slash = scoped.find('/')? + 1;
        let version = specifier[slash..].find('@').map(|offset| slash + offset);
        Some(&specifier[..version.unwrap_or(specifier.len())])
    } else {
        Some(specifier.split('@').next().unwrap_or_default())
    }
}

fn is_local_package_specifier(specifier: &str) -> bool {
    let path = specifier.strip_prefix("file:").unwrap_or(specifier);
    specifier.starts_with("file:")
        || Path::new(path).is_absolute()
        || path == "."
        || path == ".."
        || path.starts_with("./")
        || path.starts_with("../")
}

fn resolve_add_package_name(specifier: &str) -> Result<String, PluginManagerError> {
    if let Some(package) = add_package_name(specifier).filter(|name| !name.is_empty()) {
        return Ok(package.into());
    }
    if !is_local_package_specifier(specifier) {
        return Err(compatibility_error(
            PLUGIN_PACKAGE_ENTRY_INVALID,
            "the added package name could not be resolved",
        ));
    }
    let path = Path::new(specifier.strip_prefix("file:").unwrap_or(specifier));
    let root = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|error| io_error(".", error))?
            .join(path)
    };
    let manifest = read_json(&root.join("package.json"), MAX_PROFILE_MANIFEST_BYTES)?;
    manifest
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            compatibility_error(
                PLUGIN_PACKAGE_ENTRY_INVALID,
                format!("{} does not declare a package name", root.display()),
            )
        })
}

fn anchor_path_spec(specifier: &str, cwd: &Path) -> Result<String, PluginManagerError> {
    let (prefix, path) = specifier
        .strip_prefix("file:")
        .map_or(("", specifier), |path| ("file:", path));
    if !(path == "." || path == ".." || path.starts_with("./") || path.starts_with("../")) {
        return Ok(specifier.into());
    }
    let absolute = fs::canonicalize(cwd.join(path)).map_err(|error| io_error(path, error))?;
    Ok(format!("{prefix}{}", absolute.to_string_lossy()))
}

pub fn plugin_profile_root(data_dir: impl AsRef<Path>) -> PathBuf {
    data_dir.as_ref().join("plugins")
}

pub fn configure_host_plugins(config: &mut HostConfig) -> Result<(), PluginManagerError> {
    let profile = plugin_profile_root(&config.data_dir);
    let Some(entries) = load_plugin_entries(&profile)? else {
        return Ok(());
    };
    let needs_legacy = entries
        .active_entries()
        .iter()
        .any(|entry| entry.options.runtime == RuntimeKind::LegacyNode);
    config.package_resolver = Some(Arc::new(
        ProductPackageResolver::new()
            .confine_to(&profile)
            .map_err(|error| PluginManagerError::Invalid(error.to_string()))?,
    ));
    config.entries = Some(entries);
    if needs_legacy {
        config.legacy_host = Some(legacy_host_config(&config.cwd, &profile, &config.profile)?);
    }
    Ok(())
}

pub fn load_plugin_entries(profile: &Path) -> Result<Option<EntryTree>, PluginManagerError> {
    if !profile.join("package.json").exists() {
        return Ok(None);
    }
    let _lock = ProfileLock::acquire(profile)?;
    let state = load_profile(profile, true)?;
    let tree = build_bundle_entries(&state)?;
    Ok((!tree.entries.is_empty()).then_some(tree))
}

fn ensure_profile(profile: &Path) -> Result<(), PluginManagerError> {
    fs::create_dir_all(profile).map_err(|error| io_error(profile, error))?;
    let manifest = profile.join("package.json");
    if !manifest.exists() {
        fs::write(
            &manifest,
            b"{\n  \"name\": \"tessivum-plugins\",\n  \"private\": true,\n  \"dependencies\": {}\n}\n",
        )
        .map_err(|error| io_error(&manifest, error))?;
    }
    Ok(())
}

#[derive(Clone)]
struct ProfileBundle {
    name: String,
    package_root: PathBuf,
    patch: String,
}

struct ProfileDocument {
    manifest: Value,
    dependencies: Vec<String>,
    bundles: Option<Vec<String>>,
}

struct ProfileState {
    profile: PathBuf,
    dependencies: BTreeSet<String>,
    bundles: Vec<ProfileBundle>,
}

#[derive(Clone)]
struct ProfileSnapshot {
    dependencies: BTreeSet<String>,
    bundles: Vec<String>,
}
#[derive(Clone)]
struct ProfileFileSnapshot {
    bytes: Option<Vec<u8>>,
}

#[derive(Clone)]
struct ProfileMutationSnapshot {
    manifest: ProfileFileSnapshot,
    lock: ProfileFileSnapshot,
}

#[derive(Clone)]
struct FirstPartyMarketSnapshot {
    manifest: ProfileFileSnapshot,
    lock: ProfileFileSnapshot,
    state: ProfileFileSnapshot,
}

#[derive(Clone)]
struct FirstPartyMarketDependency {
    name: String,
    source: String,
}

struct FirstPartyMarketMigration {
    legacy: Option<FirstPartyMarketDependency>,
    retired_remote_web_ui: Option<FirstPartyMarketDependency>,
    bundles: Vec<String>,
    explicit_bundles: bool,
}

enum FirstPartyMarketPnpm<'a> {
    Add(&'a Path),
    Remove(&'a str),
    Install,
}

impl FirstPartyMarketSnapshot {
    fn capture(profile: &Path) -> Result<Self, PluginManagerError> {
        Ok(Self {
            manifest: capture_profile_file(
                &profile.join("package.json"),
                MAX_PROFILE_MANIFEST_BYTES,
            )?,
            lock: capture_profile_file(&profile.join("pnpm-lock.yaml"), MAX_PROFILE_LOCK_BYTES)?,
            state: capture_profile_file(
                &profile.join(".dsh-market").join("state.json"),
                MAX_PROFILE_MANIFEST_BYTES,
            )?,
        })
    }

    fn restore(&self, profile: &Path) -> Result<(), PluginManagerError> {
        restore_profile_file(&profile.join("package.json"), &self.manifest)?;
        restore_profile_file(&profile.join("pnpm-lock.yaml"), &self.lock)?;
        restore_profile_file(&profile.join(".dsh-market").join("state.json"), &self.state)
    }
}

impl FirstPartyMarketMigration {
    fn discover(profile: &Path, document: &ProfileDocument) -> Result<Self, PluginManagerError> {
        let dependencies = document
            .manifest
            .get("dependencies")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                PluginManagerError::Invalid("plugin profile dependencies must be an object".into())
            })?;
        let legacy = ["dshmarket", "dsh-market"]
            .into_iter()
            .filter_map(|name| dependencies.get(name).map(|source| (name, source)))
            .map(|(name, source)| {
                source.as_str().map_or_else(
                    || {
                        Err(PluginManagerError::Invalid(format!(
                            "legacy market dependency {name:?} must have a string source"
                        )))
                    },
                    |source| {
                        Ok(FirstPartyMarketDependency {
                            name: name.into(),
                            source: source.into(),
                        })
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        if legacy.len() > 1 {
            return Err(PluginManagerError::Invalid(
                "plugin profile contains both dshmarket and dsh-market".into(),
            ));
        }
        let legacy = legacy.into_iter().next();
        let retired_remote_web_ui = dependencies
            .get(RETIRED_REMOTE_WEB_UI_NAME)
            .map(|source| {
                source.as_str().map_or_else(
                    || {
                        Err(PluginManagerError::Invalid(format!(
                            "retired dependency {RETIRED_REMOTE_WEB_UI_NAME:?} must have a string source"
                        )))
                    },
                    |source| {
                        Ok(FirstPartyMarketDependency {
                            name: RETIRED_REMOTE_WEB_UI_NAME.into(),
                            source: source.into(),
                        })
                    },
                )
            })
            .transpose()?;
        let explicit_bundles = document.bundles.is_some();
        let bundles = match &document.bundles {
            Some(bundles) => bundles.clone(),
            None => {
                let dependencies: Vec<_> = document
                    .dependencies
                    .iter()
                    .filter(|name| name.as_str() != RETIRED_REMOTE_WEB_UI_NAME)
                    .cloned()
                    .collect();
                derive_profile_bundles(profile, &dependencies)?
                    .into_iter()
                    .map(|bundle| bundle.name)
                    .collect()
            }
        };
        let legacy_bundles: BTreeSet<_> = bundles
            .iter()
            .filter(|name| matches!(name.as_str(), "dshmarket" | "dsh-market"))
            .collect();
        if legacy_bundles.len() > 1 {
            return Err(PluginManagerError::Invalid(
                "plugin profile contains both dshmarket and dsh-market bundles".into(),
            ));
        }
        if let Some(bundle) = legacy_bundles.into_iter().next() {
            if legacy.as_ref().map(|legacy| legacy.name.as_str()) != Some(bundle.as_str()) {
                return Err(PluginManagerError::Invalid(format!(
                    "legacy market bundle {bundle:?} does not match its dependency"
                )));
            }
        }
        let migration = Self {
            legacy,
            retired_remote_web_ui,
            bundles,
            explicit_bundles,
        };
        migration.market_bundles()?;
        Ok(migration)
    }

    fn migrated_dependencies(&self) -> impl Iterator<Item = &FirstPartyMarketDependency> {
        self.legacy.iter().chain(self.retired_remote_web_ui.iter())
    }

    fn market_bundles(&self) -> Result<Vec<String>, PluginManagerError> {
        let mut bundles = self.bundles.clone();
        if let Some(legacy) = &self.legacy {
            for bundle in &mut bundles {
                if bundle == &legacy.name {
                    *bundle = FIRST_PARTY_MARKET_NAME.into();
                }
            }
        }
        bundles.retain(|bundle| bundle != RETIRED_REMOTE_WEB_UI_NAME);
        if !self.explicit_bundles
            && !bundles
                .iter()
                .any(|bundle| bundle == FIRST_PARTY_MARKET_NAME)
        {
            bundles.push(FIRST_PARTY_MARKET_NAME.into());
        }
        let mut seen = BTreeSet::new();
        if bundles.iter().any(|bundle| !seen.insert(bundle)) {
            return Err(PluginManagerError::Invalid(
                "first-party market migration would duplicate a bundle".into(),
            ));
        }
        Ok(bundles)
    }
}

#[derive(Clone, Copy)]
enum ReconciliationMode {
    Mutation,
    Restore,
}

impl ProfileSnapshot {
    fn capture(state: &ProfileState) -> Self {
        Self {
            dependencies: state.dependencies.clone(),
            bundles: state
                .bundles
                .iter()
                .map(|bundle| bundle.name.clone())
                .collect(),
        }
    }
}

impl ProfileMutationSnapshot {
    fn capture(profile: &Path) -> Result<Self, PluginManagerError> {
        Ok(Self {
            manifest: capture_profile_file(
                &profile.join("package.json"),
                MAX_PROFILE_MANIFEST_BYTES,
            )?,
            lock: capture_profile_file(&profile.join("pnpm-lock.yaml"), MAX_PROFILE_LOCK_BYTES)?,
        })
    }

    fn restore(&self, profile: &Path) -> Result<(), PluginManagerError> {
        restore_profile_file(&profile.join("package.json"), &self.manifest)?;
        restore_profile_file(&profile.join("pnpm-lock.yaml"), &self.lock)
    }

    fn verify(&self, profile: &Path) -> Result<(), PluginManagerError> {
        verify_profile_file(
            &profile.join("package.json"),
            &self.manifest,
            MAX_PROFILE_MANIFEST_BYTES,
        )?;
        verify_profile_file(
            &profile.join("pnpm-lock.yaml"),
            &self.lock,
            MAX_PROFILE_LOCK_BYTES,
        )
    }
}

fn reject_known_unsupported_mutation(mutation: &PluginMutation) -> Result<(), PluginManagerError> {
    if matches!(mutation, PluginMutation::Add(specifier) if specifier == "@linxin666/dsh-remote-web-ui@0.3.6")
    {
        return Err(compatibility_error(
            PLUGIN_DSH_ENGINE_UNSUPPORTED,
            "@linxin666/dsh-remote-web-ui@0.3.6 requires DSH >=0.1.1-rc.1; this host is fixed at 0.1.0-rc.5",
        ));
    }
    Ok(())
}

fn run_generic_mutation_pnpm(
    profile: &Path,
    arguments: &[&str],
    argument: &str,
    pnpm_started: &mut bool,
) -> Result<(), PluginManagerError> {
    let mut child = Command::new("pnpm")
        .current_dir(profile)
        .args(arguments)
        .arg(argument)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| PluginManagerError::Invalid(format!("could not run pnpm: {error}")))?;
    *pnpm_started = true;
    let status = child.wait().map_err(|error| {
        PluginManagerError::Invalid(format!("could not wait for pnpm: {error}"))
    })?;
    if status.success() {
        return Ok(());
    }
    Err(PluginManagerError::Invalid(status.code().map_or_else(
        || "pnpm terminated without an exit code".into(),
        |code| format!("pnpm exited with code {code}"),
    )))
}

fn restore_generic_mutation_failure(
    profile: &Path,
    mutation: &PluginMutation,
    snapshot: &ProfileMutationSnapshot,
    pnpm_started: bool,
    error: PluginManagerError,
) -> PluginManagerError {
    match rollback_generic_mutation(profile, snapshot, pnpm_started) {
        Ok(()) => mutation_error(format!("{error}; profile restored"), profile, mutation),
        Err(recovery) => mutation_error(
            format!(
                "{PLUGIN_MUTATION_ROLLBACK_FAILED}: {error}; recovery proof failed: {recovery}"
            ),
            profile,
            mutation,
        ),
    }
}

fn rollback_generic_mutation(
    profile: &Path,
    snapshot: &ProfileMutationSnapshot,
    pnpm_started: bool,
) -> Result<(), PluginManagerError> {
    snapshot.restore(profile)?;
    if pnpm_started {
        restore_profile_node_modules(profile, snapshot)?;
    }
    snapshot.restore(profile)?;
    snapshot.verify(profile)?;
    if snapshot.manifest.bytes.is_some() {
        let document = read_profile_document(profile)?;
        for package in &document.dependencies {
            let root = installed_package_root(profile, package)?;
            let manifest = read_json(&root.join("package.json"), MAX_PROFILE_MANIFEST_BYTES)?;
            if manifest.get("name").and_then(Value::as_str) != Some(package.as_str()) {
                return Err(PluginManagerError::Invalid(format!(
                    "restored package {package:?} does not declare its exact package name"
                )));
            }
        }
        let state = load_profile(profile, false)?;
        build_bundle_entries(&state)?;
    }
    Ok(())
}

fn restore_profile_node_modules(
    profile: &Path,
    snapshot: &ProfileMutationSnapshot,
) -> Result<(), PluginManagerError> {
    if snapshot.manifest.bytes.is_none() {
        let modules = profile.join("node_modules");
        return match fs::symlink_metadata(&modules) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                fs::remove_dir_all(&modules).map_err(|error| io_error(&modules, error))
            }
            Ok(_) => fs::remove_file(&modules).map_err(|error| io_error(&modules, error)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(&modules, error)),
        };
    }
    let scripts_allowed = profile_allows_builds(profile)
        .map_err(|error| PluginManagerError::Invalid(error.to_string()))?;
    let mut command = Command::new("pnpm");
    command
        .current_dir(profile)
        .arg("install")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if !scripts_allowed {
        command.arg("--ignore-scripts");
    }
    let mut child = command.spawn().map_err(|error| {
        PluginManagerError::Invalid(format!("could not run pnpm restoration: {error}"))
    })?;
    let deadline = Instant::now() + PNPM_OPERATION_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            PluginManagerError::Invalid(format!("could not wait for pnpm restoration: {error}"))
        })? {
            return status.success().then_some(()).ok_or_else(|| {
                PluginManagerError::Invalid(status.code().map_or_else(
                    || "pnpm restoration terminated without an exit code".into(),
                    |code| format!("pnpm restoration exited with code {code}"),
                ))
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(PluginManagerError::Invalid(
                "pnpm restoration timed out".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn verify_profile_file(
    path: &Path,
    snapshot: &ProfileFileSnapshot,
    maximum: u64,
) -> Result<(), PluginManagerError> {
    let current = capture_profile_file(path, maximum)?;
    if current.bytes == snapshot.bytes {
        Ok(())
    } else {
        Err(PluginManagerError::Invalid(format!(
            "{} was not restored byte-for-byte",
            path.display()
        )))
    }
}

fn reconciliation_mode(args: &[String]) -> ReconciliationMode {
    if args.first().is_some_and(|argument| argument == "install") {
        ReconciliationMode::Restore
    } else {
        ReconciliationMode::Mutation
    }
}

fn load_profile(profile: &Path, migrate: bool) -> Result<ProfileState, PluginManagerError> {
    let mut document = read_profile_document(profile)?;
    let dependencies = document.dependencies.iter().cloned().collect();
    let bundles = match document.bundles.clone() {
        Some(names) => {
            let bundles = resolve_profile_bundles(profile, &dependencies, &names)?;
            validate_bundle_sequence(profile, &bundles)?;
            bundles
        }
        None if migrate => {
            let bundles = derive_profile_bundles(profile, &document.dependencies)?;
            validate_bundle_sequence(profile, &bundles)?;
            let names = bundles.iter().map(|bundle| bundle.name.clone()).collect();
            set_profile_bundles(&mut document.manifest, names)?;
            write_profile_manifest(&profile.join("package.json"), &document.manifest)?;
            bundles
        }
        None => Vec::new(),
    };
    Ok(ProfileState {
        profile: profile.to_path_buf(),
        dependencies,
        bundles,
    })
}

fn read_profile_document(profile: &Path) -> Result<ProfileDocument, PluginManagerError> {
    let manifest = read_json(&profile.join("package.json"), MAX_PROFILE_MANIFEST_BYTES)?;
    let dependencies = manifest
        .get("dependencies")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            PluginManagerError::Invalid("plugin profile dependencies must be an object".into())
        })?
        .keys()
        .cloned()
        .collect();
    Ok(ProfileDocument {
        bundles: profile_bundle_names(&manifest)?,
        manifest,
        dependencies,
    })
}

fn profile_bundle_names(manifest: &Value) -> Result<Option<Vec<String>>, PluginManagerError> {
    let Some(dsh) = manifest.get("dsh") else {
        return Ok(None);
    };
    let dsh = dsh.as_object().ok_or_else(|| {
        PluginManagerError::Invalid("plugin profile dsh must be an object".into())
    })?;
    let Some(profile) = dsh.get("profile") else {
        return Ok(None);
    };
    let profile = profile.as_object().ok_or_else(|| {
        PluginManagerError::Invalid("plugin profile dsh.profile must be an object".into())
    })?;
    let Some(bundles) = profile.get("bundles") else {
        return Ok(None);
    };
    let bundles = bundles.as_array().ok_or_else(|| {
        PluginManagerError::Invalid("plugin profile dsh.profile.bundles must be an array".into())
    })?;
    let mut names = Vec::with_capacity(bundles.len());
    let mut seen = BTreeSet::new();
    for bundle in bundles {
        let name = bundle.as_str().ok_or_else(|| {
            PluginManagerError::Invalid(
                "plugin profile dsh.profile.bundles must contain only strings".into(),
            )
        })?;
        if !seen.insert(name) {
            return Err(PluginManagerError::Invalid(format!(
                "plugin profile dsh.profile.bundles contains duplicate {name:?}"
            )));
        }
        names.push(name.to_owned());
    }
    Ok(Some(names))
}

fn resolve_profile_bundles(
    profile: &Path,
    dependencies: &BTreeSet<String>,
    names: &[String],
) -> Result<Vec<ProfileBundle>, PluginManagerError> {
    let mut bundles = Vec::with_capacity(names.len());
    for name in names {
        if !dependencies.contains(name) {
            return Err(PluginManagerError::Invalid(format!(
                "plugin profile bundle {name:?} is not an installed dependency"
            )));
        }
        let package_root = installed_package_root(profile, name)?;
        let manifest = read_json(
            &package_root.join("package.json"),
            MAX_PROFILE_MANIFEST_BYTES,
        )?;
        let patch = bundle_patch(&manifest, name)?.ok_or_else(|| {
            PluginManagerError::Invalid(format!(
                "plugin profile bundle {name:?} does not declare dsh.bundle.patch"
            ))
        })?;
        safe_join(&package_root, patch)?;
        bundles.push(ProfileBundle {
            name: name.clone(),
            package_root,
            patch: patch.to_owned(),
        });
    }
    Ok(bundles)
}

fn derive_profile_bundles(
    profile: &Path,
    dependencies: &[String],
) -> Result<Vec<ProfileBundle>, PluginManagerError> {
    let mut bundles = Vec::new();
    for name in dependencies {
        let package_root = installed_package_root(profile, name)?;
        let manifest = read_json(
            &package_root.join("package.json"),
            MAX_PROFILE_MANIFEST_BYTES,
        )?;
        let Some(patch) = bundle_patch(&manifest, name)? else {
            continue;
        };
        safe_join(&package_root, patch)?;
        bundles.push(ProfileBundle {
            name: name.clone(),
            package_root,
            patch: patch.to_owned(),
        });
    }
    Ok(bundles)
}

fn validate_bundle_sequence(
    profile: &Path,
    bundles: &[ProfileBundle],
) -> Result<(), PluginManagerError> {
    let state = ProfileState {
        profile: profile.to_path_buf(),
        dependencies: BTreeSet::new(),
        bundles: bundles.to_vec(),
    };
    build_bundle_entries(&state).map(|_| ())
}

fn build_bundle_entries(profile: &ProfileState) -> Result<EntryTree, PluginManagerError> {
    let router = PluginRouter::new();
    let mut entries = BTreeMap::<String, Entry>::new();
    let mut order = Vec::new();
    for bundle in &profile.bundles {
        apply_bundle(
            &profile.profile,
            &bundle.package_root,
            &bundle.patch,
            &router,
            &mut entries,
            &mut order,
        )?;
    }
    let tree = EntryTree {
        entries: order
            .into_iter()
            .filter_map(|id| entries.remove(&id))
            .collect(),
        groups: Vec::new(),
    };
    tree.validate()
        .map_err(|error| PluginManagerError::Invalid(error.to_string()))?;
    Ok(tree)
}

fn bundle_patch<'a>(
    manifest: &'a Value,
    package: &str,
) -> Result<Option<&'a str>, PluginManagerError> {
    let Some(dsh) = dsh_declaration(manifest, package)? else {
        return Ok(None);
    };
    let Some(bundle) = dsh.get("bundle") else {
        return Ok(None);
    };
    let bundle = bundle.as_object().ok_or_else(|| {
        PluginManagerError::Invalid(format!("{package} dsh.bundle must be an object"))
    })?;
    let patch = bundle.get("patch").and_then(Value::as_str).ok_or_else(|| {
        PluginManagerError::Invalid(format!("{package} dsh.bundle.patch must be a string"))
    })?;
    Ok(Some(patch))
}

fn dsh_declaration<'a>(
    manifest: &'a Value,
    package: &str,
) -> Result<Option<&'a Map<String, Value>>, PluginManagerError> {
    let Some(dsh) = manifest.get("dsh") else {
        return Ok(None);
    };
    dsh.as_object().map(Some).ok_or_else(|| {
        PluginManagerError::Invalid(format!("{package} dsh declaration must be an object"))
    })
}

fn package_declares_bundle(profile: &Path, package: &str) -> Result<bool, PluginManagerError> {
    let package_root = installed_package_root(profile, package)?;
    let manifest = read_json(
        &package_root.join("package.json"),
        MAX_PROFILE_MANIFEST_BYTES,
    )?;
    let Some(patch) = bundle_patch(&manifest, package)? else {
        return Ok(false);
    };
    safe_join(&package_root, patch)?;
    Ok(true)
}

fn set_profile_bundles(manifest: &mut Value, names: Vec<String>) -> Result<(), PluginManagerError> {
    let root = manifest.as_object_mut().ok_or_else(|| {
        PluginManagerError::Invalid("plugin profile manifest must be an object".into())
    })?;
    let dsh = root.entry("dsh").or_insert_with(|| json!({}));
    let dsh = dsh.as_object_mut().ok_or_else(|| {
        PluginManagerError::Invalid("plugin profile dsh must be an object".into())
    })?;
    let profile = dsh.entry("profile").or_insert_with(|| json!({}));
    let profile = profile.as_object_mut().ok_or_else(|| {
        PluginManagerError::Invalid("plugin profile dsh.profile must be an object".into())
    })?;
    profile.insert(
        "bundles".into(),
        Value::Array(names.into_iter().map(Value::String).collect()),
    );
    Ok(())
}

fn write_profile_manifest(path: &Path, manifest: &Value) -> Result<(), PluginManagerError> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        PluginManagerError::Invalid(format!("plugin profile cannot be encoded: {error}"))
    })?;
    let metadata = fs::metadata(path).map_err(|error| io_error(path, error))?;
    let parent = path.parent().ok_or_else(|| {
        PluginManagerError::Invalid("plugin profile manifest has no parent directory".into())
    })?;
    let temporary = parent.join(format!(".package.json-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| io_error(&temporary, error))?;
        file.write_all(&bytes)
            .map_err(|error| io_error(&temporary, error))?;
        file.sync_all()
            .map_err(|error| io_error(&temporary, error))?;
        fs::set_permissions(&temporary, metadata.permissions())
            .map_err(|error| io_error(&temporary, error))?;
        file.sync_all()
            .map_err(|error| io_error(&temporary, error))?;
        drop(file);
        fs::rename(&temporary, path).map_err(|error| io_error(path, error))?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| io_error(parent, error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn copy_first_party_market_artifact(
    data_dir: &Path,
    tarball: &Path,
    expected_sha256: &str,
) -> Result<PathBuf, PluginManagerError> {
    let metadata = fs::symlink_metadata(tarball).map_err(|error| io_error(tarball, error))?;
    if !metadata.is_file()
        || metadata.len() > MAX_FIRST_PARTY_MARKET_TARBALL_BYTES
        || tarball.extension().and_then(|extension| extension.to_str()) != Some("tgz")
    {
        return Err(PluginManagerError::Invalid(format!(
            "{} must be a regular .tgz no larger than {MAX_FIRST_PARTY_MARKET_TARBALL_BYTES} bytes",
            tarball.display()
        )));
    }
    let expected = decode_sha256(expected_sha256)?;
    let directory = data_dir
        .join("artifacts")
        .join("market")
        .join(FIRST_PARTY_MARKET_VERSION);
    fs::create_dir_all(&directory).map_err(|error| io_error(&directory, error))?;
    let artifact = directory.join(FIRST_PARTY_MARKET_TARBALL);
    let temporary = directory.join(format!(
        ".{FIRST_PARTY_MARKET_TARBALL}-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut input = fs::File::open(tarball).map_err(|error| io_error(tarball, error))?;
        let opened = input.metadata().map_err(|error| io_error(tarball, error))?;
        if !opened.is_file() || opened.len() > MAX_FIRST_PARTY_MARKET_TARBALL_BYTES {
            return Err(PluginManagerError::Invalid(format!(
                "{} must be a regular .tgz no larger than {MAX_FIRST_PARTY_MARKET_TARBALL_BYTES} bytes",
                tarball.display()
            )));
        }
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| io_error(&temporary, error))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut copied = 0_u64;
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|error| io_error(tarball, error))?;
            if read == 0 {
                break;
            }
            copied = copied.checked_add(read as u64).ok_or_else(|| {
                PluginManagerError::Invalid(format!("{} is too large", tarball.display()))
            })?;
            if copied > MAX_FIRST_PARTY_MARKET_TARBALL_BYTES {
                return Err(PluginManagerError::Invalid(format!(
                    "{} is too large",
                    tarball.display()
                )));
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|error| io_error(&temporary, error))?;
        }
        let actual: [u8; 32] = hasher.finalize().into();
        if actual != expected {
            return Err(PluginManagerError::Invalid(format!(
                "{} does not match the supplied SHA-256",
                tarball.display()
            )));
        }
        output
            .sync_all()
            .map_err(|error| io_error(&temporary, error))?;
        drop(output);
        fs::rename(&temporary, &artifact).map_err(|error| io_error(&artifact, error))?;
        #[cfg(unix)]
        fs::File::open(&directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| io_error(&directory, error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map(|()| artifact)
}

fn decode_sha256(value: &str) -> Result<[u8; 32], PluginManagerError> {
    let value = value.trim();
    if value.len() != 64 {
        return Err(PluginManagerError::Invalid(
            "first-party market SHA-256 must be 64 hexadecimal characters".into(),
        ));
    }
    let mut digest = [0_u8; 32];
    for (index, bytes) in value.as_bytes().chunks_exact(2).enumerate() {
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        let Some(high) = digit(bytes[0]) else {
            return Err(PluginManagerError::Invalid(
                "first-party market SHA-256 must be hexadecimal".into(),
            ));
        };
        let Some(low) = digit(bytes[1]) else {
            return Err(PluginManagerError::Invalid(
                "first-party market SHA-256 must be hexadecimal".into(),
            ));
        };
        digest[index] = high << 4 | low;
    }
    Ok(digest)
}

fn first_party_market_file_spec(path: &Path) -> Result<String, PluginManagerError> {
    let path = path.to_str().ok_or_else(|| {
        PluginManagerError::Invalid("first-party market artifact path must be UTF-8".into())
    })?;
    Ok(format!("file:{path}"))
}

fn write_first_party_market_manifest(
    profile: &Path,
    artifact: &Path,
    migration: &FirstPartyMarketMigration,
    remove_migrated: bool,
) -> Result<(), PluginManagerError> {
    let mut document = read_profile_document(profile)?;
    {
        let root = document.manifest.as_object_mut().ok_or_else(|| {
            PluginManagerError::Invalid("plugin profile manifest must be an object".into())
        })?;
        let dependencies = root
            .get_mut("dependencies")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                PluginManagerError::Invalid("plugin profile dependencies must be an object".into())
            })?;
        dependencies.insert(
            FIRST_PARTY_MARKET_NAME.into(),
            Value::String(first_party_market_file_spec(artifact)?),
        );
        if remove_migrated {
            for dependency in migration.migrated_dependencies() {
                dependencies.remove(&dependency.name);
            }
        }
    }
    set_profile_bundles(&mut document.manifest, migration.market_bundles()?)?;
    write_profile_manifest(&profile.join("package.json"), &document.manifest)
}

fn first_party_market_is_current(
    profile: &Path,
    document: &ProfileDocument,
    artifact: &Path,
    migration: &FirstPartyMarketMigration,
) -> Result<bool, PluginManagerError> {
    if migration.legacy.is_some()
        || migration.retired_remote_web_ui.is_some()
        || document.bundles.as_ref() != Some(&migration.market_bundles()?)
    {
        return Ok(false);
    }
    let expected_source = first_party_market_file_spec(artifact)?;
    let source = document
        .manifest
        .pointer(&format!("/dependencies/{FIRST_PARTY_MARKET_NAME}"))
        .and_then(Value::as_str);
    if source != Some(expected_source.as_str()) {
        return Ok(false);
    }
    Ok(validate_first_party_market_installation(profile).is_ok())
}

fn validate_first_party_market_installation(profile: &Path) -> Result<(), PluginManagerError> {
    validate_market_package_entry(profile, FIRST_PARTY_MARKET_NAME)?;
    let state = load_profile(profile, false)?;
    build_bundle_entries(&state).map(|_| ())
}

fn validate_market_package_entry(profile: &Path, package: &str) -> Result<(), PluginManagerError> {
    let root = installed_package_root(profile, package)?;
    let manifest = read_json(&root.join("package.json"), MAX_PROFILE_MANIFEST_BYTES)?;
    if manifest.get("name").and_then(Value::as_str) != Some(package) {
        return Err(PluginManagerError::Invalid(format!(
            "{package} package manifest does not declare its package name"
        )));
    }
    if package == FIRST_PARTY_MARKET_NAME
        && manifest.get("version").and_then(Value::as_str) != Some(FIRST_PARTY_MARKET_VERSION)
    {
        return Err(PluginManagerError::Invalid(format!(
            "{package} package manifest does not declare version {FIRST_PARTY_MARKET_VERSION}"
        )));
    }
    let patch = bundle_patch(&manifest, package)?.ok_or_else(|| {
        PluginManagerError::Invalid(format!("{package} does not declare dsh.bundle.patch"))
    })?;
    safe_join(&root, patch)?;
    package_entry(profile, package, None, &PluginRouter::new()).map(|_| ())
}

fn validate_market_package_removed(
    profile: &Path,
    package: &str,
) -> Result<(), PluginManagerError> {
    let root = package_root(profile, package)?;
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(PluginManagerError::Invalid(format!(
            "legacy market package {package:?} is still installed"
        ))),
        Err(error) => Err(io_error(&root, error)),
    }
}

fn run_first_party_market_pnpm(
    profile: &Path,
    operation: FirstPartyMarketPnpm<'_>,
    pnpm_started: Option<&mut bool>,
) -> Result<(), PluginManagerError> {
    let scripts_allowed = profile_allows_builds(profile)
        .map_err(|error| PluginManagerError::Invalid(error.to_string()))?;
    let mut command = Command::new("pnpm");
    command
        .current_dir(profile)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let is_remove = match operation {
        FirstPartyMarketPnpm::Add(artifact) => {
            command
                .arg("add")
                .arg("--save-exact")
                .arg(first_party_market_file_spec(artifact)?);
            false
        }
        FirstPartyMarketPnpm::Remove(package) => {
            command.arg("remove").arg(package);
            true
        }
        FirstPartyMarketPnpm::Install => {
            command.arg("install");
            false
        }
    };
    if !scripts_allowed {
        command.arg(if is_remove {
            "--config.ignore-scripts=true"
        } else {
            "--ignore-scripts"
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| PluginManagerError::Mutation {
            reason: format!("could not run pnpm: {error}"),
            partial_state: first_party_market_partial_state(profile),
        })?;
    if let Some(pnpm_started) = pnpm_started {
        *pnpm_started = true;
    }
    let status = child.wait().map_err(|error| PluginManagerError::Mutation {
        reason: format!("could not wait for pnpm: {error}"),
        partial_state: first_party_market_partial_state(profile),
    })?;
    if status.success() {
        return Ok(());
    }
    Err(PluginManagerError::Mutation {
        reason: status.code().map_or_else(
            || "pnpm terminated without an exit code".into(),
            |code| format!("pnpm exited with code {code}"),
        ),
        partial_state: first_party_market_partial_state(profile),
    })
}

fn rollback_first_party_market(
    profile: &Path,
    snapshot: &FirstPartyMarketSnapshot,
    pnpm_started: bool,
    migrated_dependencies: &[FirstPartyMarketDependency],
    previous_bundles: Option<&[String]>,
) -> Result<(), PluginManagerError> {
    snapshot.restore(profile)?;
    if pnpm_started {
        if snapshot.manifest.bytes.is_some() {
            run_first_party_market_pnpm(profile, FirstPartyMarketPnpm::Install, None)?;
        } else {
            remove_first_party_market_package(profile)?;
        }
    }
    snapshot.restore(profile)?;
    if snapshot.manifest.bytes.is_some() {
        if let Some(previous_bundles) = previous_bundles {
            validate_profile_bundle_sequence(profile, previous_bundles)?;
        } else {
            let state = load_profile(profile, false)?;
            build_bundle_entries(&state)?;
        }
    }
    for dependency in migrated_dependencies {
        let document = read_profile_document(profile)?;
        let source = document
            .manifest
            .pointer(&format!("/dependencies/{}", dependency.name))
            .and_then(Value::as_str);
        if source != Some(dependency.source.as_str()) {
            return Err(PluginManagerError::Invalid(format!(
                "migrated dependency {:?} was not restored to its exact source",
                dependency.name
            )));
        }
        let root = installed_package_root(profile, &dependency.name)?;
        let manifest = read_json(&root.join("package.json"), MAX_PROFILE_MANIFEST_BYTES)?;
        if manifest.get("name").and_then(Value::as_str) != Some(dependency.name.as_str()) {
            return Err(PluginManagerError::Invalid(format!(
                "restored package {:?} does not declare its exact package name",
                dependency.name
            )));
        }
    }
    Ok(())
}
fn validate_profile_bundle_sequence(
    profile: &Path,
    names: &[String],
) -> Result<(), PluginManagerError> {
    let document = read_profile_document(profile)?;
    let dependencies = document.dependencies.iter().cloned().collect();
    let bundles = resolve_profile_bundles(profile, &dependencies, names)?;
    validate_bundle_sequence(profile, &bundles)
}

fn remove_first_party_market_package(profile: &Path) -> Result<(), PluginManagerError> {
    let package = package_root(profile, FIRST_PARTY_MARKET_NAME)?;
    match fs::symlink_metadata(&package) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            fs::remove_file(&package).map_err(|error| io_error(&package, error))
        }
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(&package).map_err(|error| io_error(&package, error))
        }
        Ok(_) => Err(PluginManagerError::Invalid(format!(
            "{} is not a removable package entry",
            package.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(&package, error)),
    }
}

fn first_party_market_partial_state(profile: &Path) -> String {
    let (manifest, lock) = profile_document_state(profile);
    format!(
        "package.json={manifest}; pnpm-lock.yaml={lock}; first-party market entry={}",
        mutation_target_entry_state(
            profile,
            &PluginMutation::Add(FIRST_PARTY_MARKET_NAME.into())
        )
    )
}

fn capture_profile_file(
    path: &Path,
    maximum: u64,
) -> Result<ProfileFileSnapshot, PluginManagerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(ProfileFileSnapshot {
            bytes: Some(read_bounded(path, maximum)?),
        }),
        Ok(_) => Err(PluginManagerError::Invalid(format!(
            "{} must be a regular file",
            path.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            Ok(ProfileFileSnapshot { bytes: None })
        }
        Err(error) => Err(io_error(path, error)),
    }
}

fn restore_profile_file(
    path: &Path,
    snapshot: &ProfileFileSnapshot,
) -> Result<(), PluginManagerError> {
    let Some(bytes) = &snapshot.bytes else {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(path, error)),
        };
    };
    let parent = path
        .parent()
        .ok_or_else(|| PluginManagerError::Invalid(format!("{} has no parent", path.display())))?;
    fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    let temporary = parent.join(format!(
        ".restore-{}-{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| io_error(&temporary, error))?;
        file.write_all(bytes)
            .map_err(|error| io_error(&temporary, error))?;
        file.sync_all()
            .map_err(|error| io_error(&temporary, error))?;
        if let Some(permissions) = permissions {
            fs::set_permissions(&temporary, permissions)
                .map_err(|error| io_error(&temporary, error))?;
        }
        drop(file);
        fs::rename(&temporary, path).map_err(|error| io_error(path, error))?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| io_error(parent, error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn reconcile_profile(
    profile: &Path,
    snapshot: &ProfileSnapshot,
    mode: ReconciliationMode,
) -> Result<(), PluginManagerError> {
    let mut document = read_profile_document(profile)?;
    let dependencies: BTreeSet<_> = document.dependencies.iter().cloned().collect();
    if matches!(mode, ReconciliationMode::Mutation)
        && document.bundles.as_ref() != Some(&snapshot.bundles)
    {
        if let Some(names) = document.bundles.as_deref() {
            let names = names
                .iter()
                .filter(|name| dependencies.contains(*name))
                .cloned()
                .collect::<Vec<_>>();
            let bundles = resolve_profile_bundles(profile, &dependencies, &names)?;
            validate_bundle_sequence(profile, &bundles)?;
        }
    }
    let mut names = match mode {
        ReconciliationMode::Mutation => snapshot.bundles.clone(),
        ReconciliationMode::Restore => document
            .bundles
            .clone()
            .unwrap_or_else(|| snapshot.bundles.clone()),
    };
    names.retain(|name| dependencies.contains(name));
    if matches!(mode, ReconciliationMode::Mutation) {
        for name in &document.dependencies {
            if !snapshot.dependencies.contains(name) && package_declares_bundle(profile, name)? {
                names.push(name.clone());
            }
        }
    }
    let bundles = resolve_profile_bundles(profile, &dependencies, &names)?;
    validate_bundle_sequence(profile, &bundles)?;
    if document.bundles.as_ref() != Some(&names) {
        set_profile_bundles(&mut document.manifest, names)?;
        write_profile_manifest(&profile.join("package.json"), &document.manifest)?;
    }
    Ok(())
}

fn resolve_bundle_expressions(
    source: &str,
    entries: &BTreeMap<String, Entry>,
    patch_path: &Path,
) -> Result<String, PluginManagerError> {
    const PREFIX: &str = "[...ctx.loader.entries()].some((e) => e.options.name === '";
    const MIDDLE: &str = "' && e.options.id !== '";
    const SUFFIX: &str = "' && !e.disabled)";
    let unsupported = || {
        PluginManagerError::Invalid(format!(
            "{} uses an unsupported legacy !!js configuration expression",
            patch_path.display()
        ))
    };
    let mut output = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let (body, ending) = line.strip_suffix("\r\n").map_or_else(
            || (line.strip_suffix('\n').unwrap_or(line), "\n"),
            |body| (body, "\r\n"),
        );
        let trimmed = body.trim_start();
        if trimmed.starts_with('#') || !trimmed.contains("!!js") {
            output.push_str(line);
            continue;
        }
        let raw = trimmed
            .strip_prefix("disabled: !!js")
            .ok_or_else(&unsupported)?;
        let expression = serde_yaml::from_str::<String>(raw.trim()).map_err(|_| unsupported())?;
        let rest = expression.strip_prefix(PREFIX).ok_or_else(&unsupported)?;
        let (package, rest) = rest.split_once(MIDDLE).ok_or_else(&unsupported)?;
        let id = rest.strip_suffix(SUFFIX).ok_or_else(&unsupported)?;
        let disabled = entries.values().any(|entry| {
            entry.options.name.as_deref() == Some(package)
                && entry.options.id.as_str() != id
                && !entry.options.disabled
        });
        output.push_str(&body[..body.len() - trimmed.len()]);
        output.push_str(if disabled {
            "disabled: true"
        } else {
            "disabled: false"
        });
        output.push_str(ending);
    }
    Ok(output)
}

fn apply_bundle(
    profile: &Path,
    package_root: &Path,
    patch: &str,
    router: &PluginRouter,
    entries: &mut BTreeMap<String, Entry>,
    order: &mut Vec<String>,
) -> Result<(), PluginManagerError> {
    let patch_path = safe_join(package_root, patch)?;
    let bytes = read_bounded(&patch_path, MAX_BUNDLE_PATCH_BYTES)?;
    let source = String::from_utf8(bytes).map_err(|error| {
        PluginManagerError::Invalid(format!("{} is not UTF-8: {error}", patch_path.display()))
    })?;
    let source = resolve_bundle_expressions(&source, entries, &patch_path)?;
    let patches: Vec<Value> = serde_yaml::from_str(&source).map_err(|error| {
        PluginManagerError::Invalid(format!("{} is invalid YAML: {error}", patch_path.display()))
    })?;
    for patch in patches {
        let object = patch.as_object().ok_or_else(|| {
            PluginManagerError::Invalid(format!(
                "{} contains a non-object patch",
                patch_path.display()
            ))
        })?;
        if let Some(inserted) = object.get("insert") {
            let inserted = inserted.as_array().ok_or_else(|| {
                PluginManagerError::Invalid("bundle insert must be an array".into())
            })?;
            for raw in inserted {
                let row = raw.as_object().ok_or_else(|| {
                    PluginManagerError::Invalid("bundle entry must be an object".into())
                })?;
                let package = required_string(row, "name")?;
                let id = required_string(row, "id")?;
                if let Some(entry) = package_entry(profile, package, Some((id, row)), router)? {
                    if entries.insert(id.into(), entry).is_none() {
                        order.push(id.into());
                    }
                }
            }
        } else if let Some(id) = object.get("id").and_then(Value::as_str) {
            if let Some(entry) = entries.get_mut(id) {
                apply_entry_override(entry, object)?;
            }
        } else {
            return Err(PluginManagerError::Invalid(
                "bundle patch must contain insert or id".into(),
            ));
        }
    }
    Ok(())
}

fn package_entry(
    profile: &Path,
    package: &str,
    declared: Option<(&str, &Map<String, Value>)>,
    router: &PluginRouter,
) -> Result<Option<Entry>, PluginManagerError> {
    let root = installed_package_root(profile, package)?;
    let route = router
        .resolve(
            &root,
            declared.is_some().then_some(PluginRuntime::LegacyNode),
        )
        .map_err(|error| PluginManagerError::Invalid(format!("{package}: {error}")))?;
    let runtime = match route.runtime {
        PluginRuntime::Browser => return Ok(None),
        PluginRuntime::Wasm => RuntimeKind::Wasm,
        PluginRuntime::LegacyNode => RuntimeKind::LegacyNode,
        PluginRuntime::Native => {
            return Err(PluginManagerError::Invalid(format!(
                "{package} declares a native runtime that this binary does not register"
            )))
        }
    };
    let (id, row) = declared.unwrap_or_else(|| ("", empty_map()));
    let id = if id.is_empty() {
        stable_entry_id(package)
    } else {
        id.to_owned()
    };
    let mut options = EntryOptions {
        id: EntryId::new(id).map_err(|error| PluginManagerError::Invalid(error.to_string()))?,
        name: Some(package.into()),
        runtime,
        config: row.get("config").cloned().unwrap_or_else(|| json!({})),
        inject: string_array(row.get("inject"), "inject")?,
        isolate: string_array(row.get("isolate"), "isolate")?,
        intercept: row.get("intercept").cloned().unwrap_or_else(|| json!({})),
        disabled: row
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        group: None,
    };
    apply_entry_override_options(&mut options, row)?;
    Ok(Some(Entry::new(root.to_string_lossy(), options)))
}

fn apply_entry_override(
    entry: &mut Entry,
    patch: &Map<String, Value>,
) -> Result<(), PluginManagerError> {
    apply_entry_override_options(&mut entry.options, patch)
}

fn apply_entry_override_options(
    options: &mut EntryOptions,
    patch: &Map<String, Value>,
) -> Result<(), PluginManagerError> {
    if let Some(config) = patch.get("config") {
        options.config = config.clone();
    }
    if let Some(disabled) = patch.get("disabled") {
        options.disabled = disabled.as_bool().ok_or_else(|| {
            PluginManagerError::Invalid("bundle disabled must be a boolean".into())
        })?;
    }
    if patch.contains_key("inject") {
        options.inject = string_array(patch.get("inject"), "inject")?;
    }
    if patch.contains_key("isolate") {
        options.isolate = string_array(patch.get("isolate"), "isolate")?;
    }
    if let Some(intercept) = patch.get("intercept") {
        options.intercept = intercept.clone();
    }
    Ok(())
}

fn package_root(profile: &Path, package: &str) -> Result<PathBuf, PluginManagerError> {
    let segment = |value: &str| {
        !value.is_empty()
            && value != "."
            && value != ".."
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'~')
            })
    };
    let valid = if let Some(scoped) = package.strip_prefix('@') {
        let mut parts = scoped.split('/');
        matches!((parts.next(), parts.next(), parts.next()), (Some(scope), Some(name), None) if segment(scope) && segment(name))
    } else {
        !package.contains('/') && segment(package)
    };
    if !valid {
        return Err(PluginManagerError::Invalid(format!(
            "invalid npm package name {package:?}"
        )));
    }
    Ok(profile.join("node_modules").join(package))
}

fn installed_package_root(profile: &Path, package: &str) -> Result<PathBuf, PluginManagerError> {
    let package_root = package_root(profile, package)?;
    let package_root =
        fs::canonicalize(&package_root).map_err(|error| io_error(&package_root, error))?;
    let profile_root = fs::canonicalize(profile).map_err(|error| io_error(profile, error))?;
    let modules = profile.join("node_modules");
    let modules = fs::canonicalize(&modules).map_err(|error| io_error(&modules, error))?;
    if !modules.starts_with(&profile_root) || !package_root.starts_with(&modules) {
        return Err(PluginManagerError::Invalid(format!(
            "package {package:?} escapes the plugin profile"
        )));
    }
    Ok(package_root)
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, PluginManagerError> {
    use std::path::Component;

    let relative = relative.strip_prefix("./").unwrap_or(relative);
    if relative.is_empty()
        || Path::new(relative).components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(PluginManagerError::Invalid(
            "bundle patch must be a safe relative path".into(),
        ));
    }
    let path = root.join(relative);
    let canonical = fs::canonicalize(&path).map_err(|error| io_error(&path, error))?;
    let root = fs::canonicalize(root).map_err(|error| io_error(root, error))?;
    if !canonical.starts_with(root) {
        return Err(PluginManagerError::Invalid(
            "bundle patch escapes its package".into(),
        ));
    }
    Ok(canonical)
}

fn validate_mutation_candidate(
    profile: &Path,
    mutation: &PluginMutation,
) -> Result<(), PluginManagerError> {
    let PluginMutation::Add(specifier) = mutation else {
        return Ok(());
    };
    let package = resolve_add_package_name(specifier)?;
    validate_candidate_package(profile, &package)
}

fn preflight_materialized_candidate(
    profile: &Path,
    mutation: &PluginMutation,
) -> Result<(), PluginManagerError> {
    let PluginMutation::Add(specifier) = mutation else {
        return Ok(());
    };
    let Some(package) = add_package_name(specifier) else {
        return Ok(());
    };
    let Some(version) = specifier
        .strip_prefix(package)
        .and_then(|specifier| specifier.strip_prefix('@'))
        .filter(|version| !version.is_empty())
    else {
        return Ok(());
    };
    let Ok(root) = installed_package_root(profile, package) else {
        return Ok(());
    };
    let Ok(manifest) = read_json(&root.join("package.json"), MAX_PROFILE_MANIFEST_BYTES) else {
        return Ok(());
    };
    if manifest.get("version").and_then(Value::as_str) == Some(version) {
        validate_dsh_engine(&manifest, package)?;
    }
    Ok(())
}

fn validate_candidate_package(profile: &Path, package: &str) -> Result<(), PluginManagerError> {
    let root = installed_package_root(profile, package).map_err(|error| {
        compatibility_error(PLUGIN_PACKAGE_ENTRY_INVALID, format!("{package}: {error}"))
    })?;
    let manifest =
        read_json(&root.join("package.json"), MAX_PROFILE_MANIFEST_BYTES).map_err(|error| {
            compatibility_error(PLUGIN_PACKAGE_ENTRY_INVALID, format!("{package}: {error}"))
        })?;
    if manifest.get("name").and_then(Value::as_str) != Some(package) {
        return Err(compatibility_error(
            PLUGIN_PACKAGE_ENTRY_INVALID,
            format!("{package} package manifest does not declare its exact package name"),
        ));
    }
    validate_dsh_engine(&manifest, package)?;
    let entry = package_export_entry(&manifest, ".")
        .or_else(|| manifest.get("main").and_then(Value::as_str))
        .or_else(|| manifest.get("module").and_then(Value::as_str))
        .ok_or_else(|| {
            compatibility_error(
                PLUGIN_PACKAGE_ENTRY_INVALID,
                format!("{package} has no main or root export"),
            )
        })?;
    let entry = resolve_package_entry(&root, entry, package, PLUGIN_PACKAGE_ENTRY_INVALID)?;
    let entry_source = read_bounded(&entry, MAX_PACKAGE_ENTRY_BYTES).map_err(|error| {
        compatibility_error(
            PLUGIN_PACKAGE_ENTRY_INVALID,
            format!("{}: {error}", entry.display()),
        )
    })?;
    let entry_source = std::str::from_utf8(&entry_source).map_err(|error| {
        compatibility_error(
            PLUGIN_PACKAGE_ENTRY_INVALID,
            format!("{} is not UTF-8: {error}", entry.display()),
        )
    })?;
    validate_declared_inject(&assignment_strings(entry_source, "inject"), package)?;
    let mut runtime_entries = vec![entry];
    let dsh = dsh_declaration(&manifest, package).map_err(|error| {
        compatibility_error(PLUGIN_PACKAGE_ENTRY_INVALID, format!("{package}: {error}"))
    })?;
    if let Some(dsh) = dsh {
        if dsh.contains_key("client") {
            let client = dsh
                .get("client")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    compatibility_error(
                        PLUGIN_CLIENT_ENTRY_INVALID,
                        format!("{package} dsh.client must be an object"),
                    )
                })?;
            if client.get("platform").and_then(Value::as_str) != Some("web") {
                return Err(compatibility_error(
                    PLUGIN_CLIENT_ENTRY_INVALID,
                    format!("{package} dsh.client.platform must be web"),
                ));
            }
            let client_entry = package_export_entry(&manifest, "./client").ok_or_else(|| {
                compatibility_error(
                    PLUGIN_CLIENT_ENTRY_INVALID,
                    format!("{package} does not export ./client"),
                )
            })?;
            runtime_entries.push(resolve_package_entry(
                &root,
                client_entry,
                package,
                PLUGIN_CLIENT_ENTRY_INVALID,
            )?);
        }
    }
    validate_runtime_imports(&manifest, &runtime_entries, package)?;
    validate_bundle_candidate(profile, &root, &manifest, package)
}

fn validate_dsh_engine(manifest: &Value, package: &str) -> Result<(), PluginManagerError> {
    let Some(dsh) = dsh_declaration(manifest, package).map_err(|error| {
        compatibility_error(PLUGIN_DSH_ENGINE_UNSUPPORTED, format!("{package}: {error}"))
    })?
    else {
        return Ok(());
    };
    let Some(engines) = dsh.get("engines") else {
        return Ok(());
    };
    let engine = engines
        .as_object()
        .and_then(|engines| engines.get("dsh"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            compatibility_error(
                PLUGIN_DSH_ENGINE_UNSUPPORTED,
                format!("{package} dsh.engines.dsh must be a semver range"),
            )
        })?;
    if dsh_engine_supports_baseline(engine) {
        return Ok(());
    }
    Err(compatibility_error(
        PLUGIN_DSH_ENGINE_UNSUPPORTED,
        format!(
            "{package} requires DSH {engine}; this host is fixed at {DSH_COMPATIBILITY_BASELINE}"
        ),
    ))
}

fn validate_bundle_candidate(
    profile: &Path,
    root: &Path,
    manifest: &Value,
    package: &str,
) -> Result<(), PluginManagerError> {
    let Some(patch) = bundle_patch(manifest, package).map_err(|error| {
        compatibility_error(PLUGIN_BUNDLE_PATCH_INVALID, format!("{package}: {error}"))
    })?
    else {
        return Ok(());
    };
    let patch_path = safe_join(root, patch).map_err(|error| {
        compatibility_error(PLUGIN_BUNDLE_PATCH_INVALID, format!("{package}: {error}"))
    })?;
    let patch_source = String::from_utf8(
        read_bounded(&patch_path, MAX_BUNDLE_PATCH_BYTES).map_err(|error| {
            compatibility_error(PLUGIN_BUNDLE_PATCH_INVALID, format!("{package}: {error}"))
        })?,
    )
    .map_err(|error| {
        compatibility_error(
            PLUGIN_BUNDLE_PATCH_INVALID,
            format!("{} is not UTF-8: {error}", patch_path.display()),
        )
    })?;
    let patch_source = resolve_bundle_expressions(&patch_source, &BTreeMap::new(), &patch_path)
        .map_err(|error| {
            compatibility_error(PLUGIN_BUNDLE_PATCH_INVALID, format!("{package}: {error}"))
        })?;
    serde_yaml::from_str::<Vec<Value>>(&patch_source).map_err(|error| {
        compatibility_error(
            PLUGIN_BUNDLE_PATCH_INVALID,
            format!("{} is invalid YAML: {error}", patch_path.display()),
        )
    })?;
    let mut entries = BTreeMap::new();
    let mut order = Vec::new();
    apply_bundle(
        profile,
        root,
        patch,
        &PluginRouter::new(),
        &mut entries,
        &mut order,
    )
    .map_err(|error| {
        compatibility_error(PLUGIN_PACKAGE_ENTRY_INVALID, format!("{package}: {error}"))
    })?;
    for entry in entries.values() {
        validate_declared_inject(
            &entry.options.inject,
            entry.options.name.as_deref().unwrap_or(package),
        )?;
    }
    Ok(())
}

fn resolve_package_entry(
    root: &Path,
    entry: &str,
    package: &str,
    code: &str,
) -> Result<PathBuf, PluginManagerError> {
    let path = safe_join(root, entry)
        .map_err(|error| compatibility_error(code, format!("{package}: {error}")))?;
    if path.is_file() {
        Ok(path)
    } else {
        Err(compatibility_error(
            code,
            format!("{package} entry {entry:?} is not a regular file"),
        ))
    }
}

fn package_export_entry<'a>(manifest: &'a Value, key: &str) -> Option<&'a str> {
    manifest.get("exports").and_then(|exports| match key {
        "." => export_entry(exports),
        key => exports.as_object()?.get(key).and_then(export_entry),
    })
}

fn export_entry(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) => Some(value),
        Value::Object(values) => values
            .get("import")
            .and_then(export_entry)
            .or_else(|| values.get("require").and_then(export_entry))
            .or_else(|| values.get("default").and_then(export_entry)),
        _ => None,
    }
}

fn validate_runtime_imports(
    manifest: &Value,
    entries: &[PathBuf],
    package: &str,
) -> Result<(), PluginManagerError> {
    let expression = Regex::new(
        r#"(?m)(?:\b(?:import|export)\s+(?:[^'\"]*?\s+from\s+)?|\brequire\s*\()\s*['\"]([@A-Za-z0-9._:/~-]+)['\"]"#,
    )
    .expect("valid runtime import expression");
    let mut imports = BTreeSet::new();
    for entry in entries {
        let bytes = read_bounded(entry, MAX_PACKAGE_ENTRY_BYTES).map_err(|error| {
            compatibility_error(
                PLUGIN_PACKAGE_ENTRY_INVALID,
                format!("{}: {error}", entry.display()),
            )
        })?;
        let source = String::from_utf8(bytes).map_err(|error| {
            compatibility_error(
                PLUGIN_PACKAGE_ENTRY_INVALID,
                format!("{} is not UTF-8: {error}", entry.display()),
            )
        })?;
        imports.extend(
            expression
                .captures_iter(&source)
                .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_owned())),
        );
    }
    let declared = ["dependencies", "peerDependencies"]
        .into_iter()
        .filter_map(|field| manifest.get(field).and_then(Value::as_object))
        .flat_map(|dependencies| dependencies.keys().map(String::as_str))
        .collect::<BTreeSet<_>>();
    for import in imports {
        let Some(dependency) = runtime_dependency_name(&import) else {
            continue;
        };
        if dependency == package
            || declared.contains(dependency)
            || HOST_MODULE_ALIASES.contains(&dependency)
        {
            continue;
        }
        return Err(compatibility_error(
            PLUGIN_RUNTIME_DEPENDENCY_MISSING,
            format!(
                "{package} imports {import:?} without a runtime dependency, peer, or Host alias"
            ),
        ));
    }
    Ok(())
}

fn runtime_dependency_name(specifier: &str) -> Option<&str> {
    if specifier.starts_with('.') || specifier.starts_with('/') || specifier.starts_with("node:") {
        return None;
    }
    let package = if specifier.starts_with('@') {
        let scope_end = specifier.find('/')?;
        let end = specifier[scope_end + 1..]
            .find('/')
            .map_or(specifier.len(), |index| scope_end + 1 + index);
        &specifier[..end]
    } else {
        specifier.split('/').next().unwrap_or_default()
    };
    (!package.is_empty() && !NODE_BUILTIN_MODULES.contains(&package)).then_some(package)
}

const HOST_MODULE_ALIASES: &[&str] = &[
    "cordis",
    "cosmokit",
    "@deepseek-ai/cordis",
    "@deepseek-ai/cosmokit",
    "@deepseek-ai/cordis-plugin-loader",
    "@cordisjs/plugin-loader",
    "@deepseek-ai/dsh-settings",
    "@deepseek-ai/schemastery",
    "@deepseek-ai/dsh-tools",
    "@deepseek-ai/dsh-llm",
    "@deepseek-ai/dsh-subagent",
];

const NODE_BUILTIN_MODULES: &[&str] = &[
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

fn validate_declared_inject(services: &[String], package: &str) -> Result<(), PluginManagerError> {
    for service in services {
        let service = service.split('@').next().unwrap_or(service);
        if AVAILABLE_LEGACY_SERVICES.contains(&service) {
            continue;
        }
        return Err(compatibility_error(
            PLUGIN_INJECT_UNAVAILABLE,
            format!("{package} requires unavailable Legacy Context service {service:?}"),
        ));
    }
    Ok(())
}

const AVAILABLE_LEGACY_SERVICES: &[&str] = &[
    "agents",
    "commands",
    "desktopPnpm",
    "desktopProfiles",
    "hostEvents",
    "hostLifecycle",
    "loader",
    "models",
    "settings",
    "sessions",
    "tools",
    "webListener",
    "webRuntime",
    "webServer",
    "workspaces",
];

fn compatibility_error(code: &str, message: impl std::fmt::Display) -> PluginManagerError {
    PluginManagerError::Invalid(format!("{code}: {message}"))
}

#[derive(Clone, Eq, PartialEq)]
struct DshVersion {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Option<String>,
}

fn dsh_engine_supports_baseline(range: &str) -> bool {
    let Some(baseline) = parse_dsh_version(DSH_COMPATIBILITY_BASELINE) else {
        return false;
    };
    range.split("||").any(|alternative| {
        let terms = alternative
            .split(|character: char| character.is_ascii_whitespace() || character == ',')
            .filter(|term| !term.is_empty());
        let mut saw_term = false;
        for term in terms {
            saw_term = true;
            if !dsh_comparator_matches(&baseline, term) {
                return false;
            }
        }
        saw_term
    })
}

fn dsh_comparator_matches(baseline: &DshVersion, term: &str) -> bool {
    let (operator, version) = [">=", "<=", ">", "<", "=", "^", "~"]
        .into_iter()
        .find_map(|operator| {
            term.strip_prefix(operator)
                .map(|version| (operator, version))
        })
        .unwrap_or(("=", term));
    let Some(version) = parse_dsh_version(version) else {
        return false;
    };
    let comparison = compare_dsh_versions(baseline, &version);
    match operator {
        ">=" => comparison.is_ge(),
        "<=" => comparison.is_le(),
        ">" => comparison.is_gt(),
        "<" => comparison.is_lt(),
        "=" => comparison.is_eq(),
        "^" => {
            comparison.is_ge() && compare_dsh_versions(baseline, &dsh_caret_upper(&version)).is_lt()
        }
        "~" => {
            comparison.is_ge() && compare_dsh_versions(baseline, &dsh_tilde_upper(&version)).is_lt()
        }
        _ => false,
    }
}

fn parse_dsh_version(value: &str) -> Option<DshVersion> {
    let (release, prerelease) =
        value
            .split_once('-')
            .map_or((value, None), |(release, prerelease)| {
                (
                    release,
                    (!prerelease.is_empty()).then_some(prerelease.to_owned()),
                )
            });
    let mut parts = release.split('.').map(str::parse::<u64>);
    let version = DshVersion {
        major: parts.next()?.ok()?,
        minor: parts.next()?.ok()?,
        patch: parts.next()?.ok()?,
        prerelease,
    };
    parts.next().is_none().then_some(version)
}

fn compare_dsh_versions(left: &DshVersion, right: &DshVersion) -> std::cmp::Ordering {
    for (left, right) in [
        (left.major, right.major),
        (left.minor, right.minor),
        (left.patch, right.patch),
    ] {
        let comparison = left.cmp(&right);
        if !comparison.is_eq() {
            return comparison;
        }
    }
    match (&left.prerelease, &right.prerelease) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => compare_prerelease(left, right),
    }
}

fn compare_prerelease(left: &str, right: &str) -> std::cmp::Ordering {
    let mut left = left.split('.');
    let mut right = right.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(left), Some(right)) => {
                let comparison = match (left.parse::<u64>(), right.parse::<u64>()) {
                    (Ok(left), Ok(right)) => left.cmp(&right),
                    (Ok(_), Err(_)) => std::cmp::Ordering::Less,
                    (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                    (Err(_), Err(_)) => left.cmp(right),
                };
                if !comparison.is_eq() {
                    return comparison;
                }
            }
        }
    }
}

fn dsh_caret_upper(version: &DshVersion) -> DshVersion {
    if version.major > 0 {
        DshVersion {
            major: version.major + 1,
            minor: 0,
            patch: 0,
            prerelease: None,
        }
    } else if version.minor > 0 {
        DshVersion {
            major: 0,
            minor: version.minor + 1,
            patch: 0,
            prerelease: None,
        }
    } else {
        DshVersion {
            major: 0,
            minor: 0,
            patch: version.patch + 1,
            prerelease: None,
        }
    }
}

fn dsh_tilde_upper(version: &DshVersion) -> DshVersion {
    DshVersion {
        major: version.major,
        minor: version.minor + 1,
        patch: 0,
        prerelease: None,
    }
}

fn read_json(path: &Path, maximum: u64) -> Result<Value, PluginManagerError> {
    serde_json::from_slice(&read_bounded(path, maximum)?).map_err(|error| {
        PluginManagerError::Invalid(format!("{} is invalid JSON: {error}", path.display()))
    })
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, PluginManagerError> {
    let metadata = fs::metadata(path).map_err(|error| io_error(path, error))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(PluginManagerError::Invalid(format!(
            "{} must be a regular file no larger than {maximum} bytes",
            path.display()
        )));
    }
    fs::read(path).map_err(|error| io_error(path, error))
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, PluginManagerError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            PluginManagerError::Invalid(format!("bundle entry {field} must be a non-empty string"))
        })
}

fn string_array(value: Option<&Value>, field: &str) -> Result<Vec<String>, PluginManagerError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| PluginManagerError::Invalid(format!("bundle {field} must be an array")))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    PluginManagerError::Invalid(format!(
                        "bundle {field} items must be non-empty strings"
                    ))
                })
        })
        .collect()
}

fn stable_entry_id(package: &str) -> String {
    let stem = package
        .trim_start_matches('@')
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let digest = Sha256::digest(package.as_bytes());
    format!(
        "plugin-{stem}-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

fn install_vendor_aliases(profile: &Path, vendor: &Path) -> Result<(), PluginManagerError> {
    let modules = profile.join("node_modules");
    for (name, package) in [
        ("cordis", "cordis"),
        ("cosmokit", "cosmokit"),
        ("@deepseek-ai/cordis", "cordis"),
        ("@deepseek-ai/cosmokit", "cosmokit"),
        ("@deepseek-ai/cordis-plugin-loader", "loader"),
        ("@cordisjs/plugin-loader", "loader"),
    ] {
        let source = vendor.join(package);
        let alias = modules.join(name);
        if fs::canonicalize(&alias).is_ok_and(|path| path == source) {
            continue;
        }
        match fs::symlink_metadata(&alias) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                fs::remove_dir_all(&alias).map_err(|error| io_error(&alias, error))?;
            }
            Ok(_) => fs::remove_file(&alias).map_err(|error| io_error(&alias, error))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&alias, error)),
        }
        fs::create_dir_all(alias.parent().expect("module aliases have a parent"))
            .map_err(|error| io_error(&alias, error))?;
        symlink_directory(&source, &alias).map_err(|error| io_error(&alias, error))?;
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_directory(source: &Path, alias: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, alias)
}

#[cfg(windows)]
fn symlink_directory(source: &Path, alias: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(source, alias)
}

fn install_host_module_aliases(profile: &Path, root: &Path) -> Result<(), PluginManagerError> {
    let modules = profile.join("node_modules");
    for name in [
        "@deepseek-ai/dsh-settings",
        "@deepseek-ai/schemastery",
        "@deepseek-ai/dsh-tools",
        "@deepseek-ai/dsh-llm",
        "@deepseek-ai/dsh-subagent",
    ] {
        let source = root.join(name);
        let manifest = read_json(&source.join("package.json"), MAX_PROFILE_MANIFEST_BYTES)?;
        let entry = manifest
            .get("module")
            .or_else(|| manifest.get("main"))
            .and_then(Value::as_str)
            .unwrap_or("index.js");
        if !source.join(entry.trim_start_matches("./")).is_file() {
            return Err(PluginManagerError::Invalid(format!(
                "{} has no usable entry",
                source.display()
            )));
        }
        let alias = modules.join(name);
        if fs::canonicalize(&alias).is_ok_and(|path| path == source) {
            continue;
        }
        match fs::symlink_metadata(&alias) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                fs::remove_dir_all(&alias).map_err(|error| io_error(&alias, error))?
            }
            Ok(_) => fs::remove_file(&alias).map_err(|error| io_error(&alias, error))?,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&alias, error)),
        }
        fs::create_dir_all(alias.parent().expect("scoped alias has a parent"))
            .map_err(|error| io_error(&alias, error))?;
        symlink_directory(&source, &alias).map_err(|error| io_error(&alias, error))?;
    }
    Ok(())
}

fn legacy_host_config(
    cwd: &Path,
    profile: &Path,
    profile_name: &str,
) -> Result<LegacyHostConfig, PluginManagerError> {
    let host = env::var_os("TESSIVUM_COMPAT_HOST")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.join("../tessivum-core/node/compat-host/src/index.ts"));
    let vendor = env::var_os("CORDIS_VENDOR_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.join("../upstream/deepseek-harness/vendor"));
    let host = fs::canonicalize(&host).map_err(|error| io_error(&host, error))?;
    let vendor = fs::canonicalize(&vendor).map_err(|error| io_error(&vendor, error))?;
    let profile = fs::canonicalize(profile).map_err(|error| io_error(profile, error))?;
    install_vendor_aliases(&profile, &vendor)?;
    let mut command = HostCommand::new("bun")
        .arg("run")
        .arg(&host)
        .current_dir(&profile)
        .env("CORDIS_VENDOR_ROOT", vendor)
        .env("TESSIVUM_BRIDGE_MAX_FRAME_SIZE", "12582912")
        .env("TESSIVUM_PROFILE_NAME", profile_name)
        .env("TESSIVUM_PROFILE_DIR", &profile);
    if let Some(root) = env::var_os("TESSIVUM_HOST_MODULE_ROOT") {
        let root = PathBuf::from(root);
        let root = fs::canonicalize(&root).map_err(|error| io_error(&root, error))?;
        install_host_module_aliases(&profile, &root)?;
        command = command.env("TESSIVUM_HOST_MODULE_ROOT", root);
    }
    Ok(LegacyHostConfig {
        command,
        client: ClientConfig::default(),
    })
}

fn io_error(path: impl AsRef<Path>, error: impl std::fmt::Display) -> PluginManagerError {
    PluginManagerError::Io {
        path: path.as_ref().to_path_buf(),
        reason: error.to_string(),
    }
}

fn empty_map() -> &'static Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

pub fn installed_plugin_names(profile: &Path) -> Result<BTreeSet<String>, PluginManagerError> {
    let manifest = profile.join("package.json");
    if !manifest.exists() {
        return Ok(BTreeSet::new());
    }
    Ok(read_json(&manifest, MAX_PROFILE_MANIFEST_BYTES)?
        .get("dependencies")
        .and_then(Value::as_object)
        .map(|dependencies| dependencies.keys().cloned().collect())
        .unwrap_or_default())
}

fn market_disabled_plugin_names(profile: &Path) -> BTreeSet<String> {
    let Ok(state) = read_json(
        &profile.join(".dsh-market").join("state.json"),
        MAX_PROFILE_MANIFEST_BYTES,
    ) else {
        return BTreeSet::new();
    };
    let Some(state) = state.as_object() else {
        return BTreeSet::new();
    };
    let Some(disabled) = state.get("disabled").or_else(|| state.get("disabledSkins")) else {
        return BTreeSet::new();
    };
    let Some(disabled) = disabled.as_array() else {
        return BTreeSet::new();
    };

    let mut names = BTreeSet::new();
    for name in disabled.iter().filter_map(Value::as_str) {
        if !name.is_empty() {
            names.insert(name.to_owned());
        }
    }
    names
}

pub fn enabled_client_plugin_names(profile: &Path) -> Result<BTreeSet<String>, PluginManagerError> {
    if !profile.join("package.json").exists() {
        return Ok(BTreeSet::new());
    }
    let _lock = ProfileLock::acquire(profile)?;
    let state = load_profile(profile, true)?;
    let disabled = market_disabled_plugin_names(profile);
    let bundles: BTreeSet<_> = state
        .bundles
        .iter()
        .map(|bundle| bundle.name.as_str())
        .collect();
    let mut names = BTreeSet::new();
    for package in &state.dependencies {
        if disabled.contains(package) {
            continue;
        }
        let root = installed_package_root(profile, package)?;
        let manifest = read_json(&root.join("package.json"), MAX_PROFILE_MANIFEST_BYTES)?;
        let Some(dsh) = dsh_declaration(&manifest, package)? else {
            continue;
        };
        if !dsh.contains_key("client") {
            continue;
        }
        if !dsh.contains_key("bundle") || bundles.contains(package.as_str()) {
            names.insert(package.clone());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pnpm_mutation_contract_uses_frozen_argv_and_clears_legacy_lock() {
        assert_eq!(
            pnpm_arguments(&PluginMutation::Add("plugin@1.0.0".into())),
            &["add", "--save-exact", "--ignore-scripts"]
        );
        assert_eq!(
            pnpm_arguments(&PluginMutation::Remove("plugin".into())),
            &["remove", "--config.ignore-scripts=true"]
        );

        let profile = temporary_profile();
        let lock = profile.join("package-lock.json");
        fs::write(&lock, "legacy").unwrap();
        remove_legacy_package_lock(&profile).unwrap();
        assert!(!lock.exists());
        fs::write(
            profile.join("package.json"),
            br#"{"dependencies":{"plugin":"1.0.0"}}"#,
        )
        .unwrap();
        fs::write(profile.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        let package = profile.join("node_modules/plugin");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("package.json"), br#"{"name":"plugin"}"#).unwrap();
        assert_eq!(
            mutation_partial_state(&profile, &PluginMutation::Add("plugin@1.0.0".into())),
            "package.json=valid; pnpm-lock.yaml=valid; target package entry=plugin=valid"
        );
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn profile_lock_serializes_active_owners_and_preserves_replacement_owner() {
        let profile = temporary_profile();
        let active = ProfileLock::acquire(&profile).unwrap();
        assert!(matches!(
            ProfileLock::acquire(&profile),
            Err(PluginManagerError::Busy(_))
        ));
        drop(active);

        let lock = profile.join(".tessivum-profile.lock");
        fs::create_dir(&lock).unwrap();
        fs::write(
            lock.join("owner.json"),
            serde_json::to_vec(&ProfileLockOwner {
                pid: 0,
                nonce: "dead".into(),
            })
            .unwrap(),
        )
        .unwrap();
        let recovered = ProfileLock::acquire(&profile).unwrap();
        fs::write(
            lock.join("owner.json"),
            serde_json::to_vec(&ProfileLockOwner {
                pid: process::id(),
                nonce: "replacement".into(),
            })
            .unwrap(),
        )
        .unwrap();
        drop(recovered);
        assert!(lock.exists());
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn package_names_cannot_escape_the_profile() {
        let profile = Path::new("/profile");
        assert_eq!(
            package_root(profile, "@cordisjs/plugin-timer").unwrap(),
            profile.join("node_modules/@cordisjs/plugin-timer")
        );
        for package in [".", "..", "../escape", "@scope/../escape"] {
            assert!(package_root(profile, package).is_err(), "{package}");
        }
    }

    #[test]
    fn compatibility_pnpm_argv_matches_dshmarket_and_remains_script_safe() {
        let profile = temporary_profile();
        fs::write(
            profile.join("package.json"),
            br#"{"dependencies":{},"pnpm":{"onlyBuiltDependencies":[]}}"#,
        )
        .unwrap();
        assert_eq!(
            route_pnpm_args(
                &[
                    "add".into(),
                    "-w".into(),
                    "dsh-example@^1.2.3".into(),
                    "--reporter=ndjson".into(),
                ],
                &profile,
            )
            .unwrap(),
            vec![
                "add",
                "-w",
                "dsh-example@^1.2.3",
                "--reporter=ndjson",
                "--config.auto-install-peers=false",
                "--ignore-scripts",
            ]
        );
        assert!(matches!(
            reconciliation_mode(&["add".into(), "install".into()]),
            ReconciliationMode::Mutation
        ));
        let install_args = route_pnpm_args(
            &[
                "--no-frozen-lockfile".into(),
                "--config.minimumReleaseAge=0".into(),
                "install".into(),
            ],
            &profile,
        )
        .unwrap();
        assert_eq!(
            install_args,
            vec![
                "install",
                "--no-frozen-lockfile",
                "--config.minimumReleaseAge=0",
                "--reporter=ndjson",
                "--config.auto-install-peers=false",
                "--ignore-scripts",
            ]
        );
        assert!(matches!(
            reconciliation_mode(&install_args),
            ReconciliationMode::Restore
        ));
        assert_eq!(
            route_pnpm_args(
                &["remove".into(), "-w".into(), "dsh-example".into()],
                &profile,
            )
            .unwrap(),
            vec![
                "remove",
                "-w",
                "dsh-example",
                "--reporter=ndjson",
                "--config.auto-install-peers=false",
                "--config.ignore-scripts=true",
            ]
        );
        for args in [
            vec!["add".into(), "../escape".into()],
            vec!["add".into(), "plugin@1".into(), "--force".into()],
            vec!["install".into(), "-w".into()],
        ] {
            assert!(route_pnpm_args(&args, &profile).is_err(), "{args:?}");
        }
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn known_duplicate_guard_is_resolved_without_executing_javascript() {
        let source = r#"# !!js remains inert in comments
- insert:
    - id: better-sidebar
      name: 'dsh-better-sidebar'
      disabled: !!js "[...ctx.loader.entries()].some((e) => e.options.name === 'dsh-better-sidebar' && e.options.id !== 'better-sidebar' && !e.disabled)"
"#;
        let resolved =
            resolve_bundle_expressions(source, &BTreeMap::new(), Path::new("cordis.patch.yml"))
                .unwrap();
        assert!(resolved.contains("disabled: false"));
        serde_yaml::from_str::<Vec<Value>>(&resolved).unwrap();
        assert!(resolve_bundle_expressions(
            "disabled: !!js \"process.env.UNSAFE\"\n",
            &BTreeMap::new(),
            Path::new("cordis.patch.yml"),
        )
        .is_err());
    }

    #[test]
    fn reconciliation_appends_only_new_bundles_and_prunes_removed_dependencies() {
        let profile = temporary_profile();
        write_test_package(&profile, "disabled", true);
        write_test_package(&profile, "new-bundle", true);
        write_test_package(&profile, "client-only", false);
        write_test_package(&profile, "unrelated", false);
        set_test_manifest(&profile, &["disabled"], Some(&["disabled"]));
        let snapshot = ProfileSnapshot::capture(&load_profile(&profile, true).unwrap());

        set_test_manifest(
            &profile,
            &["disabled", "new-bundle", "client-only"],
            Some(&["disabled"]),
        );
        reconcile_profile(&profile, &snapshot, ReconciliationMode::Mutation).unwrap();
        assert_eq!(test_bundles(&profile), vec!["disabled", "new-bundle"]);

        set_test_manifest(
            &profile,
            &["disabled", "new-bundle", "client-only", "unrelated"],
            Some(&["disabled", "new-bundle"]),
        );
        reconcile_profile(&profile, &snapshot, ReconciliationMode::Mutation).unwrap();
        assert_eq!(test_bundles(&profile), vec!["disabled", "new-bundle"]);

        set_test_manifest(
            &profile,
            &["new-bundle", "client-only", "unrelated"],
            Some(&["disabled", "new-bundle"]),
        );
        reconcile_profile(&profile, &snapshot, ReconciliationMode::Mutation).unwrap();
        assert_eq!(test_bundles(&profile), vec!["new-bundle"]);
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn market_and_cli_reconciliation_have_the_same_stable_append() {
        let profile = temporary_profile();
        write_test_package(&profile, "existing", true);
        write_test_package(&profile, "added", true);
        set_test_manifest(&profile, &["existing"], Some(&["existing"]));
        let snapshot = ProfileSnapshot::capture(&load_profile(&profile, true).unwrap());
        set_test_manifest(&profile, &["existing", "added"], Some(&["existing"]));
        reconcile_profile(&profile, &snapshot, ReconciliationMode::Mutation).unwrap();
        let cli_bundles = test_bundles(&profile);

        set_test_manifest(&profile, &["existing", "added"], Some(&["existing"]));
        reconcile_profile(&profile, &snapshot, ReconciliationMode::Mutation).unwrap();
        assert_eq!(test_bundles(&profile), cli_bundles);
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn restore_keeps_the_manifest_bundle_order() {
        let profile = temporary_profile();
        for package in ["old", "first", "second"] {
            write_test_package(&profile, package, true);
        }
        set_test_manifest(&profile, &["old"], Some(&["old"]));
        let snapshot = ProfileSnapshot::capture(&load_profile(&profile, true).unwrap());
        set_test_manifest(&profile, &["first", "second"], Some(&["second", "first"]));
        reconcile_profile(&profile, &snapshot, ReconciliationMode::Restore).unwrap();
        assert_eq!(test_bundles(&profile), vec!["second", "first"]);
        fs::remove_dir_all(profile).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failed_atomic_profile_replacement_preserves_the_original_file() {
        use std::os::unix::fs::PermissionsExt;

        let profile = temporary_profile();
        let manifest = profile.join("package.json");
        let original = br#"{"dependencies":{}}"#;
        fs::write(&manifest, original).unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(
            write_profile_manifest(&manifest, &json!({"dependencies": {"bundle": "1"}})).is_err()
        );
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(fs::read(&manifest).unwrap(), original);
        assert!(fs::read_dir(&profile).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp")));
        fs::remove_dir_all(profile).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_profile_replacement_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let profile = temporary_profile();
        let manifest = profile.join("package.json");
        fs::write(&manifest, br#"{"dependencies":{}}"#).unwrap();
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o640)).unwrap();
        write_profile_manifest(&manifest, &json!({"dependencies": {}})).unwrap();
        assert_eq!(
            fs::metadata(&manifest).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn market_disabled_names_prefer_disabled_and_filter_malformed_members() {
        let profile = temporary_profile();
        let market = profile.join(".dsh-market");
        fs::create_dir_all(&market).unwrap();
        let state = market.join("state.json");

        fs::write(
            &state,
            br#"{"disabled":["one",42,"","one","two"],"disabledSkins":["legacy"]}"#,
        )
        .unwrap();
        assert_eq!(
            market_disabled_plugin_names(&profile),
            BTreeSet::from(["one".to_owned(), "two".to_owned()])
        );

        fs::write(&state, br#"{"disabledSkins":["legacy",null,"","legacy"]}"#).unwrap();
        assert_eq!(
            market_disabled_plugin_names(&profile),
            BTreeSet::from(["legacy".to_owned()])
        );

        fs::remove_dir_all(profile).unwrap();
    }

    fn set_test_manifest(profile: &Path, dependencies: &[&str], bundles: Option<&[&str]>) {
        let dependencies = dependencies
            .iter()
            .map(|name| ((*name).to_owned(), Value::String("1.0.0".into())))
            .collect();
        let mut manifest = Map::from_iter([("dependencies".into(), Value::Object(dependencies))]);
        if let Some(bundles) = bundles {
            manifest.insert("dsh".into(), json!({"profile": {"bundles": bundles}}));
        }
        fs::write(
            profile.join("package.json"),
            serde_json::to_vec(&Value::Object(manifest)).unwrap(),
        )
        .unwrap();
    }

    fn write_test_package(profile: &Path, name: &str, bundle: bool) {
        let package = profile.join("node_modules").join(name);
        fs::create_dir_all(&package).unwrap();
        let manifest = if bundle {
            json!({"name": name, "dsh": {"bundle": {"patch": "./cordis.patch.yml"}}})
        } else {
            json!({"name": name, "dsh": {"client": {"platform": "web"}}})
        };
        fs::write(
            package.join("package.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        if bundle {
            fs::write(package.join("cordis.patch.yml"), "[]\n").unwrap();
        }
    }

    fn test_bundles(profile: &Path) -> Vec<String> {
        read_profile_document(profile).unwrap().bundles.unwrap()
    }

    fn temporary_profile() -> PathBuf {
        let profile = env::temp_dir().join(format!("tessivum-pnpm-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&profile).unwrap();
        profile
    }
}

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tessivum_core::{Entry, EntryId, EntryOptions, EntryTree, RuntimeKind};
use tessivum_node_bridge::{ClientConfig, HostCommand};
use thiserror::Error;

use crate::{
    host::{HostConfig, LegacyHostConfig},
    legacy::ProductPackageResolver,
    plugins::{PluginRouter, PluginRuntime},
};

const MAX_PROFILE_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_BUNDLE_PATCH_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Error)]
pub enum PluginManagerError {
    #[error("plugin profile I/O failed at {path}: {reason}")]
    Io { path: PathBuf, reason: String },
    #[error("plugin profile is invalid: {0}")]
    Invalid(String),
    #[error("plugin package manager failed with exit code {0}")]
    PackageManager(i32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginMutation {
    Add(String),
    Remove(String),
}

pub fn mutate_plugins(
    data_dir: impl AsRef<Path>,
    mutation: PluginMutation,
) -> Result<(), PluginManagerError> {
    let profile = plugin_profile_root(data_dir);
    ensure_profile(&profile)?;
    let cwd = env::current_dir().map_err(|error| io_error(".", error))?;
    let (verb, argument) = match mutation {
        PluginMutation::Add(specifier) => ("install", anchor_path_spec(&specifier, &cwd)?),
        PluginMutation::Remove(package) => ("uninstall", package),
    };
    let status = Command::new("npm")
        .current_dir(&profile)
        .args([
            verb,
            "--save-exact",
            "--ignore-scripts",
            "--install-links",
            "--",
        ])
        .arg(argument)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| PluginManagerError::Io {
            path: profile.clone(),
            reason: format!("could not run npm: {error}"),
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(PluginManagerError::PackageManager(
            status.code().unwrap_or(1),
        ))
    }
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
        config.legacy_host = Some(legacy_host_config(&config.cwd, &profile)?);
    }
    Ok(())
}

pub fn load_plugin_entries(profile: &Path) -> Result<Option<EntryTree>, PluginManagerError> {
    let manifest_path = profile.join("package.json");
    if !manifest_path.exists() {
        return Ok(None);
    }
    let manifest = read_json(&manifest_path, MAX_PROFILE_MANIFEST_BYTES)?;
    let dependencies = manifest
        .get("dependencies")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            PluginManagerError::Invalid("plugin profile dependencies must be an object".into())
        })?;
    if dependencies.is_empty() {
        return Ok(None);
    }

    let router = PluginRouter::new();
    let mut entries = BTreeMap::<String, Entry>::new();
    let mut order = Vec::new();
    for package in dependencies.keys() {
        let root = package_root(profile, package)?;
        let package_manifest = read_json(&root.join("package.json"), MAX_PROFILE_MANIFEST_BYTES)?;
        if let Some(patch) = package_manifest
            .pointer("/dsh/bundle/patch")
            .and_then(Value::as_str)
        {
            apply_bundle(profile, &root, patch, &router, &mut entries, &mut order)?;
        } else if let Some(entry) = package_entry(profile, package, None, &router)? {
            let id = entry.options.id.as_str().to_owned();
            if entries.insert(id.clone(), entry).is_none() {
                order.push(id);
            }
        }
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
    if source.contains("!!js") {
        return Err(PluginManagerError::Invalid(format!(
            "{} uses legacy !!js configuration expressions, which Tessivum does not execute",
            patch_path.display()
        )));
    }
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
    let root = package_root(profile, package)?;
    let route = router
        .resolve(&root, None)
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
    Ok(Some(Entry::new(
        fs::canonicalize(&root)
            .map_err(|error| io_error(&root, error))?
            .to_string_lossy(),
        options,
    )))
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

fn legacy_host_config(cwd: &Path, profile: &Path) -> Result<LegacyHostConfig, PluginManagerError> {
    let host = env::var_os("TESSIVUM_COMPAT_HOST")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.join("../tessivum-core/node/compat-host/src/index.ts"));
    let vendor = env::var_os("CORDIS_VENDOR_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.join("../upstream/deepseek-harness/vendor"));
    let host = fs::canonicalize(&host).map_err(|error| io_error(&host, error))?;
    let vendor = fs::canonicalize(&vendor).map_err(|error| io_error(&vendor, error))?;
    let command = HostCommand::new("bun")
        .arg("run")
        .arg(&host)
        .current_dir(profile)
        .env("CORDIS_VENDOR_ROOT", vendor);
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

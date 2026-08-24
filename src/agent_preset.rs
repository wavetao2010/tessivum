//! Durable agent-preset discovery and user authoring.
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::fs;

const MAX_ID_BYTES: usize = 128;
const MAX_CONTENT_BYTES: usize = 4 * 1024 * 1024;
const COMPOSITION_FILE: &str = "agent.cordis.yml";
const METADATA_FILE: &str = "preset.yml";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentPresetTrust {
    System,
    User,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentPresetRoot {
    pub path: PathBuf,
    pub trust: AgentPresetTrust,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPresetSummary {
    pub id: String,
    pub trust: AgentPresetTrust,
    pub is_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPresetDocument {
    pub agent_preset: String,
    pub trust: AgentPresetTrust,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Model-facing behavior derived from an enabled preset composition.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentPresetModelCatalog {
    /// A complete persona replaces all host-supplied prompt sections.
    pub complete_system: Option<String>,
    /// Native tool names exposed by model-facing plugin rows.
    pub tools: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct PresetLocation {
    id: String,
    trust: AgentPresetTrust,
    directory: PathBuf,
    order: i64,
    summary: AgentPresetSummary,
}

#[derive(Clone, Debug)]
pub struct AgentPresetService {
    roots: Vec<AgentPresetRoot>,
    authoring_root: Option<PathBuf>,
}

impl AgentPresetService {
    pub fn new(system_root: impl Into<PathBuf>, user_root: impl Into<PathBuf>) -> Self {
        let system_root = system_root.into();
        let user_root = user_root.into();
        Self::with_roots(
            vec![
                AgentPresetRoot {
                    path: system_root,
                    trust: AgentPresetTrust::System,
                },
                AgentPresetRoot {
                    path: user_root.clone(),
                    trust: AgentPresetTrust::User,
                },
            ],
            Some(user_root),
        )
    }

    pub fn with_roots(roots: Vec<AgentPresetRoot>, authoring_root: Option<PathBuf>) -> Self {
        Self {
            roots,
            authoring_root,
        }
    }
    pub fn authorable(&self) -> bool {
        self.authoring_root.is_some()
    }

    pub async fn list(&self) -> Result<Vec<AgentPresetSummary>, Value> {
        Ok(self
            .locations()
            .await?
            .into_iter()
            .map(|location| location.summary)
            .collect())
    }

    async fn locations(&self) -> Result<Vec<PresetLocation>, Value> {
        let mut result = Vec::new();
        let mut seen = BTreeSet::new();
        for root in &self.roots {
            let mut locations = Vec::new();
            scan_root(&root.path, root.trust, &mut locations).await?;
            locations.sort_by(|left, right| {
                left.order
                    .cmp(&right.order)
                    .then_with(|| left.id.cmp(&right.id))
            });
            result.extend(
                locations
                    .into_iter()
                    .filter(|location| seen.insert(location.id.clone())),
            );
        }
        Ok(result)
    }

    async fn resolve(&self, id: &str) -> Result<PresetLocation, Value> {
        validate_id(id).map_err(|reason| rejected("agent-preset-invalid", id, reason))?;
        let locations = self.locations().await?;
        let available = locations
            .iter()
            .map(|location| location.id.clone())
            .collect::<Vec<_>>();
        locations
            .into_iter()
            .find(|location| location.id == id)
            .ok_or_else(|| {
                envelope(
                    "agent-preset-not-found",
                    "agent preset was not found",
                    json!({"agentPreset": id, "available": available}),
                )
            })
    }

    pub async fn read(&self, id: &str) -> Result<AgentPresetDocument, Value> {
        let location = self.resolve(id).await?;
        let content = read_text(location.directory.join(COMPOSITION_FILE)).await?;
        Ok(AgentPresetDocument {
            agent_preset: location.id,
            trust: location.trust,
            content,
            name: location.summary.name,
            description: location.summary.description,
        })
    }

    pub async fn path(&self, id: &str) -> Result<(AgentPresetTrust, PathBuf), Value> {
        let location = self.resolve(id).await?;
        Ok((
            location.trust,
            fs::canonicalize(location.directory)
                .await
                .map_err(internal)?,
        ))
    }

    pub async fn copy(
        &self,
        from: &str,
        target: &str,
        name: Option<String>,
    ) -> Result<String, Value> {
        let source = self.resolve(from).await?;
        validate_id(target).map_err(|reason| rejected("agent-preset-invalid", target, reason))?;
        if self
            .locations()
            .await?
            .iter()
            .any(|location| location.id == target)
        {
            return Err(rejected(
                "agent-preset-invalid",
                target,
                "target already exists",
            ));
        }
        let user_root = self.authoring_root.as_ref().ok_or_else(|| {
            envelope(
                "agent-preset-unsupported",
                "agent preset authoring is unavailable",
                json!({}),
            )
        })?;
        let destination = user_root.join(target);
        if fs::try_exists(&destination).await.map_err(internal)? {
            return Err(rejected(
                "agent-preset-invalid",
                target,
                "target already exists",
            ));
        }
        fs::create_dir_all(user_root).await.map_err(internal)?;
        if let Err(error) = copy_tree(&source.directory, &destination).await {
            let _ = fs::remove_dir_all(&destination).await;
            return Err(internal(error));
        }
        if let Err(error) = write_metadata(&destination, name, source.summary.description).await {
            let _ = fs::remove_dir_all(&destination).await;
            return Err(error);
        }
        if let Err(error) = tighten_modes(&destination).await {
            let _ = fs::remove_dir_all(&destination).await;
            return Err(internal(error));
        }
        Ok(target.to_owned())
    }

    pub async fn remove(&self, id: &str) -> Result<(), Value> {
        let location = self.resolve(id).await?;
        if location.trust == AgentPresetTrust::System {
            return Err(rejected(
                "agent-preset-read-only",
                id,
                "it ships with the deployment",
            ));
        }
        let authoring_root = self.authoring_root.as_ref().ok_or_else(|| {
            rejected(
                "agent-preset-read-only",
                id,
                "it does not live under the writable preset root",
            )
        })?;
        let writable_root = fs::canonicalize(authoring_root).await.map_err(internal)?;
        let directory = fs::canonicalize(&location.directory)
            .await
            .map_err(internal)?;
        if directory != writable_root.join(&location.id) {
            return Err(rejected(
                "agent-preset-read-only",
                id,
                "it does not live under the writable preset root",
            ));
        }
        fs::remove_dir_all(directory).await.map_err(internal)
    }

    pub async fn prepare_selection(&self, id: &str) -> Result<(), Value> {
        let location = self.resolve(id).await?;
        let content = read_text(location.directory.join(COMPOSITION_FILE)).await?;
        parse_composition(&content)
            .map(|_| ())
            .map_err(|reason| rejected("agent-preset-invalid", id, &reason))
    }

    pub async fn contains_plugin(&self, id: &str, plugin: &str) -> Result<bool, Value> {
        let location = self.resolve(id).await?;
        let content = read_text(location.directory.join(COMPOSITION_FILE)).await?;
        let rows = parse_composition(&content)
            .map_err(|reason| rejected("agent-preset-invalid", id, &reason))?;
        Ok(rows_contain_plugin(&rows, plugin))
    }
    pub async fn model_catalog(&self, id: &str) -> Result<AgentPresetModelCatalog, Value> {
        let location = self.resolve(id).await?;
        let content = read_text(location.directory.join(COMPOSITION_FILE)).await?;
        composition_model_catalog(&content)
            .map_err(|reason| rejected("agent-preset-invalid", id, &reason))
    }
}

async fn scan_root(
    root: &Path,
    trust: AgentPresetTrust,
    output: &mut Vec<PresetLocation>,
) -> Result<(), Value> {
    let mut entries = match fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(internal(error)),
    };
    while let Some(entry) = entries.next_entry().await.map_err(internal)? {
        if !entry.file_type().await.map_err(internal)?.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if validate_id(&id).is_err() {
            continue;
        }
        let directory = entry.path();
        let (name, description, order) = read_metadata(&directory).await.unwrap_or((None, None, 0));
        output.push(PresetLocation {
            id: id.clone(),
            trust,
            directory: directory.clone(),
            order,
            summary: AgentPresetSummary {
                id,
                trust,
                is_default: false,
                name,
                description,
                broken: composition_problem(&directory).await,
            },
        });
    }
    Ok(())
}

async fn read_metadata(directory: &Path) -> Result<(Option<String>, Option<String>, i64), ()> {
    let text = fs::read_to_string(directory.join(METADATA_FILE))
        .await
        .map_err(|_| ())?;
    let value = serde_yaml::from_str::<Value>(&text).map_err(|_| ())?;
    let object = value.as_object().ok_or(())?;
    let text = |field: &str| {
        object
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    Ok((
        text("name"),
        text("description"),
        object
            .get("order")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
    ))
}

async fn read_text(path: PathBuf) -> Result<String, Value> {
    let bytes = fs::read(path).await.map_err(internal)?;
    if bytes.len() > MAX_CONTENT_BYTES {
        return Err(envelope(
            "agent-preset-invalid",
            "preset content is too large",
            json!({}),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        envelope(
            "agent-preset-invalid",
            "preset content is not UTF-8",
            json!({}),
        )
    })
}

async fn composition_problem(directory: &Path) -> Option<String> {
    let text = match fs::read_to_string(directory.join(COMPOSITION_FILE)).await {
        Ok(text) => text,
        Err(_) => {
            return Some(format!(
                "the composition file {COMPOSITION_FILE} is missing or unreadable"
            ))
        }
    };
    parse_composition(&text).err()
}

fn parse_composition(text: &str) -> Result<Vec<serde_yaml::Value>, String> {
    let rows: Vec<serde_yaml::Value> = serde_yaml::from_str(text)
        .map_err(|error| format!("the composition is not valid YAML: {error}"))?;
    entry_list_problem(&rows)?;
    Ok(rows)
}

fn entry_list_problem(rows: &[serde_yaml::Value]) -> Result<(), String> {
    for row in rows {
        let row = row
            .as_mapping()
            .ok_or_else(|| "the composition must be a top-level list of plugin rows".to_owned())?;
        let name = row
            .get(serde_yaml::Value::String("name".into()))
            .and_then(serde_yaml::Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| "each plugin row must declare a non-empty name".to_owned())?;
        if row
            .get(serde_yaml::Value::String("group".into()))
            .and_then(serde_yaml::Value::as_bool)
            == Some(true)
        {
            let children = row
                .get(serde_yaml::Value::String("config".into()))
                .and_then(serde_yaml::Value::as_sequence)
                .ok_or_else(|| format!("group {name} must contain a config list"))?;
            entry_list_problem(children)?;
        }
    }
    Ok(())
}

pub fn composition_contains_plugin(text: &str, plugin: &str) -> Result<bool, String> {
    parse_composition(text).map(|rows| rows_contain_plugin(&rows, plugin))
}

fn rows_contain_plugin(rows: &[serde_yaml::Value], plugin: &str) -> bool {
    rows.iter().any(|row| {
        let Some(row) = row.as_mapping() else {
            return false;
        };
        if row
            .get(serde_yaml::Value::String("name".into()))
            .and_then(serde_yaml::Value::as_str)
            == Some(plugin)
        {
            return true;
        }
        row.get(serde_yaml::Value::String("config".into()))
            .and_then(serde_yaml::Value::as_sequence)
            .is_some_and(|children| rows_contain_plugin(children, plugin))
    })
}

/// Extracts the model contract from enabled, recursively nested plugin rows.
pub fn composition_model_catalog(text: &str) -> Result<AgentPresetModelCatalog, String> {
    let rows = parse_composition(text)?;
    let mut catalog = AgentPresetModelCatalog::default();
    collect_model_catalog(&rows, &mut catalog)?;
    Ok(catalog)
}

fn collect_model_catalog(
    rows: &[serde_yaml::Value],
    catalog: &mut AgentPresetModelCatalog,
) -> Result<(), String> {
    for row in rows {
        let Some(row) = row.as_mapping() else {
            continue;
        };
        if yaml_bool(row, "disabled") == Some(true) || yaml_bool(row, "enabled") == Some(false) {
            continue;
        }
        let Some(name) = yaml_text(row, "name") else {
            continue;
        };
        let config = yaml_value(row, "config").and_then(serde_yaml::Value::as_mapping);
        if name == "@deepseek-ai/dsh-persona"
            && config.and_then(|config| yaml_bool(config, "complete")) == Some(true)
        {
            let text = config
                .and_then(|config| yaml_text(config, "text"))
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| "a complete persona must declare non-empty text".to_owned())?;
            catalog.complete_system = Some(text.to_owned());
        }
        catalog.tools.extend(model_tools_for_plugin(name, config));
        if let Some(children) = yaml_value(row, "config").and_then(serde_yaml::Value::as_sequence) {
            collect_model_catalog(children, catalog)?;
        }
    }
    Ok(())
}

fn model_tools_for_plugin(plugin: &str, config: Option<&serde_yaml::Mapping>) -> Vec<String> {
    let names: &[&str] = match plugin {
        "@deepseek-ai/dsh-tool-bash" | "@deepseek-ai/dsh-tool-bash-persistent" => &["bash"],
        "@deepseek-ai/dsh-tool-str-replace-editor" => &["str_replace_editor"],
        "@deepseek-ai/dsh-tool-fs" => &["read", "write", "edit", "read_image"],
        "@deepseek-ai/dsh-tool-fs-search" => &["glob", "grep"],
        "@deepseek-ai/dsh-tool-jobs" => &["jobs.kill", "jobs.list", "jobs.read", "jobs.wait"],
        "@deepseek-ai/dsh-tool-skill" => &["skill"],
        "@deepseek-ai/dsh-tool-goal" => &["create_goal", "get_goal", "update_goal"],
        "@deepseek-ai/dsh-plan-mode" => &["exit_plan_mode"],
        "@deepseek-ai/dsh-tool-subagent-control" => &["interrupt_agent", "send_message"],
        "@deepseek-ai/dsh-tool-subagent-control/list-agents" => &["list_agents"],
        "@deepseek-ai/dsh-tool-workflow" => &["workflow"],
        "@deepseek-ai/dsh-tool-ralph" => &["ralph"],
        "@deepseek-ai/dsh-tool-ask-user" => &["ask_user_question"],
        "@deepseek-ai/dsh-tool-todo" => &["todo_write"],
        "@deepseek-ai/dsh-tool-web" => {
            if config.and_then(|config| yaml_bool(config, "fetch")) == Some(true) {
                &["web_search", "web_fetch"]
            } else {
                &["web_search"]
            }
        }
        "@deepseek-ai/dsh-tool-subagent" => {
            return config
                .and_then(|config| yaml_text(config, "toolName"))
                .filter(|name| native_tool_name(name))
                .map(ToOwned::to_owned)
                .into_iter()
                .collect();
        }
        _ => &[],
    };
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn native_tool_name(name: &str) -> bool {
    matches!(
        name,
        "ask_user_question"
            | "bash"
            | "cordis_define"
            | "cordis_inspect_list"
            | "cordis_inspect_query"
            | "cordis_inspect_self"
            | "cordis_run"
            | "cordis_stop"
            | "create_goal"
            | "edit"
            | "exit_plan_mode"
            | "get_goal"
            | "glob"
            | "grep"
            | "interrupt_agent"
            | "jobs.kill"
            | "jobs.list"
            | "jobs.read"
            | "jobs.wait"
            | "list_agents"
            | "ralph"
            | "read"
            | "read_image"
            | "schedule_create"
            | "schedule_delete"
            | "schedule_list"
            | "send_message"
            | "skill"
            | "str_replace_editor"
            | "subagent"
            | "subagent_fork"
            | "todo_write"
            | "update_goal"
            | "web_fetch"
            | "web_search"
            | "workflow"
            | "write"
    )
}

fn yaml_value<'a>(row: &'a serde_yaml::Mapping, field: &str) -> Option<&'a serde_yaml::Value> {
    row.get(serde_yaml::Value::String(field.into()))
}

fn yaml_text<'a>(row: &'a serde_yaml::Mapping, field: &str) -> Option<&'a str> {
    yaml_value(row, field).and_then(serde_yaml::Value::as_str)
}

fn yaml_bool(row: &serde_yaml::Mapping, field: &str) -> Option<bool> {
    yaml_value(row, field).and_then(serde_yaml::Value::as_bool)
}

async fn write_metadata(
    directory: &Path,
    name: Option<String>,
    description: Option<String>,
) -> Result<(), Value> {
    let name = name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned);
    let metadata_path = directory.join(METADATA_FILE);
    if name.is_none() && description.is_none() {
        if let Err(error) = fs::remove_file(metadata_path).await {
            if error.kind() != io::ErrorKind::NotFound {
                return Err(internal(error));
            }
        }
        return Ok(());
    }
    let mut metadata = serde_json::Map::new();
    if let Some(name) = name {
        metadata.insert("name".into(), Value::String(name));
    }
    if let Some(description) = description {
        metadata.insert("description".into(), Value::String(description));
    }
    fs::write(
        metadata_path,
        serde_yaml::to_string(&Value::Object(metadata)).map_err(internal)?,
    )
    .await
    .map_err(internal)
}

async fn copy_tree(source: &Path, target: &Path) -> io::Result<()> {
    enum Pending {
        Visit(PathBuf, PathBuf),
        Leave(PathBuf),
    }

    let mut active = BTreeSet::new();
    let mut pending = vec![Pending::Visit(source.to_path_buf(), target.to_path_buf())];
    while let Some(step) = pending.pop() {
        let (source, target) = match step {
            Pending::Visit(source, target) => (source, target),
            Pending::Leave(directory) => {
                active.remove(&directory);
                continue;
            }
        };
        let canonical = fs::canonicalize(&source).await?;
        if !active.insert(canonical.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "preset copy contains a directory symlink cycle",
            ));
        }
        fs::create_dir_all(&target).await?;
        pending.push(Pending::Leave(canonical));
        let mut entries = fs::read_dir(source).await?;
        while let Some(entry) = entries.next_entry().await? {
            let source = entry.path();
            let destination = target.join(entry.file_name());
            let metadata = fs::metadata(&source).await?;
            if metadata.is_dir() {
                pending.push(Pending::Visit(source, destination));
            } else if metadata.is_file() {
                fs::copy(source, destination).await?;
            } else {
                return Err(io::Error::other(
                    "preset copy contains an unsupported file type",
                ));
            }
        }
    }
    Ok(())
}

async fn tighten_modes(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
        let mut entries = fs::read_dir(directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if entry.file_type().await?.is_dir() {
                Box::pin(tighten_modes(&path)).await?;
            } else {
                let executable = fs::metadata(&path).await?.permissions().mode() & 0o100 != 0;
                fs::set_permissions(
                    path,
                    std::fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
                )
                .await?;
            }
        }
    }
    Ok(())
}

pub fn validate_id_public(id: &str) -> Result<(), &'static str> {
    validate_id(id)
}
fn validate_id(id: &str) -> Result<(), &'static str> {
    if id.len() > MAX_ID_BYTES
        || !id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err("identifier must match [a-z0-9][a-z0-9-]*");
    }
    Ok(())
}
fn rejected(code: &str, id: &str, reason: &str) -> Value {
    envelope(
        code,
        "agent preset operation was rejected",
        json!({"agentPreset": id, "reason": reason}),
    )
}
fn internal(error: impl ToString) -> Value {
    envelope("agent-preset-internal", &error.to_string(), json!({}))
}
fn envelope(code: &str, message: &str, details: Value) -> Value {
    json!({"code": code, "message": message, "details": details})
}
pub fn error_parts(error: &Value) -> (String, String, Value) {
    (
        error
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("internal")
            .into(),
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("agent preset operation failed")
            .into(),
        error.get("details").cloned().unwrap_or_else(|| json!({})),
    )
}
pub fn selected_event_data(id: &str) -> Value {
    json!({"agentPreset": id})
}
pub fn default_paths(data_dir: &Path) -> (PathBuf, PathBuf) {
    (
        data_dir.join("agent-presets"),
        data_dir.join(".agent-presets"),
    )
}

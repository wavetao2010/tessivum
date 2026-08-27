//! Immutable native agent-mode declarations and strict custom-mode discovery.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Map, Value};

use crate::TessivumError;

pub const MODE_SCHEMA_VERSION: u32 = 1;
pub const MODE_FILE_NAME: &str = "mode.toml";
pub const MAX_MODE_ID_BYTES: usize = 64;
pub const MAX_MODE_DOCUMENT_BYTES: usize = 256 * 1024;
pub const MAX_MODES_PER_ROOT: usize = 256;
pub const MAX_MODE_PLUGINS: usize = 32;
pub const MAX_MODE_TOOLS: usize = 64;
pub const MAX_MODE_PLUGIN_SOURCE_BYTES: usize = 1_024;
pub const MAX_MODE_PLUGIN_CONFIG_BYTES: usize = 64 * 1024;

/// A stable, path-safe agent-mode identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AgentModeId(String);

impl AgentModeId {
    pub fn new(value: impl Into<String>) -> Result<Self, TessivumError> {
        let value = value.into();
        validate_mode_id(&value)?;
        Ok(Self(value))
    }

    pub fn standard() -> Self {
        Self("standard".into())
    }
    pub fn ptc() -> Self {
        Self("ptc".into())
    }
    pub fn minimal() -> Self {
        Self("minimal".into())
    }
    pub fn composition() -> Self {
        Self("composition".into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn into_string(self) -> String {
        self.0
    }
    pub fn is_builtin(&self) -> bool {
        matches!(
            self.as_str(),
            "standard" | "ptc" | "minimal" | "composition"
        )
    }
}

impl AsRef<str> for AgentModeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl fmt::Display for AgentModeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl FromStr for AgentModeId {
    type Err = TessivumError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}
impl Serialize for AgentModeId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for AgentModeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentModeTrust {
    Builtin,
    System,
    User,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentModeRoot {
    pub path: PathBuf,
    pub trust: AgentModeTrust,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentModeSummary {
    pub id: AgentModeId,
    pub trust: AgentModeTrust,
    pub name: String,
    pub description: String,
    pub built_in: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentModeDocument {
    pub id: AgentModeId,
    pub trust: AgentModeTrust,
    pub content: String,
    pub name: String,
    pub description: String,
    pub built_in: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromptPolicy {
    pub complete: bool,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolPresentation {
    Direct,
    Programmatic,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionPolicy {
    Standard,
}

/// Session capabilities which do not independently create model-visible tools.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct ModeCapabilities {
    pub skills: bool,
    pub planning: bool,
    pub compaction: bool,
    pub bun: bool,
    pub persistent_shell: bool,
    pub composition: bool,
}

/// Stable IDs resolved here, rather than in each future runtime consumer.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ToolCapabilityId {
    FsRead,
    FsWrite,
    FsEdit,
    FsStrReplaceEditor,
    FsReadImage,
    SearchGlob,
    SearchGrep,
    ShellBash,
    JobsKill,
    JobsList,
    JobsRead,
    JobsWait,
    SkillLoad,
    GoalCreate,
    GoalGet,
    GoalUpdate,
    PlanExit,
    PlanTodo,
    SubagentInterrupt,
    SubagentSendMessage,
    SubagentList,
    SubagentSpawn,
    SubagentFork,
    Ralph,
    WorkflowRun,
    WebSearch,
    WebFetch,
    QuestionAsk,
    ScheduleCreate,
    ScheduleList,
    ScheduleDelete,
    CompositionInspect,
    CompositionDefine,
    CompositionValidate,
    CompositionRun,
    CompositionStop,
}

impl ToolCapabilityId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::FsEdit => "fs.edit",
            Self::FsStrReplaceEditor => "fs.str-replace-editor",
            Self::FsReadImage => "fs.read-image",
            Self::SearchGlob => "search.glob",
            Self::SearchGrep => "search.grep",
            Self::ShellBash => "shell.bash",
            Self::JobsKill => "jobs.kill",
            Self::JobsList => "jobs.list",
            Self::JobsRead => "jobs.read",
            Self::JobsWait => "jobs.wait",
            Self::SkillLoad => "skill.load",
            Self::GoalCreate => "goal.create",
            Self::GoalGet => "goal.get",
            Self::GoalUpdate => "goal.update",
            Self::PlanExit => "plan.exit",
            Self::PlanTodo => "plan.todo",
            Self::SubagentInterrupt => "subagent.interrupt",
            Self::SubagentSendMessage => "subagent.send-message",
            Self::SubagentList => "subagent.list",
            Self::SubagentSpawn => "subagent.spawn",
            Self::SubagentFork => "subagent.fork",
            Self::Ralph => "ralph.run",
            Self::WorkflowRun => "workflow.run",
            Self::WebSearch => "web.search",
            Self::WebFetch => "web.fetch",
            Self::QuestionAsk => "question.ask",
            Self::ScheduleCreate => "schedule.create",
            Self::ScheduleList => "schedule.list",
            Self::ScheduleDelete => "schedule.delete",
            Self::CompositionInspect => "composition.inspect",
            Self::CompositionDefine => "composition.define",
            Self::CompositionValidate => "composition.validate",
            Self::CompositionRun => "composition.run",
            Self::CompositionStop => "composition.stop",
        }
    }

    pub fn native_tools(self) -> &'static [&'static str] {
        match self {
            Self::FsRead => &["read"],
            Self::FsWrite => &["write"],
            Self::FsEdit => &["edit"],
            Self::FsStrReplaceEditor => &["str_replace_editor"],
            Self::FsReadImage => &["read_image"],
            Self::SearchGlob => &["glob"],
            Self::SearchGrep => &["grep"],
            Self::ShellBash => &["bash"],
            Self::JobsKill => &["jobs.kill"],
            Self::JobsList => &["jobs.list"],
            Self::JobsRead => &["jobs.read"],
            Self::JobsWait => &["jobs.wait"],
            Self::SkillLoad => &["skill"],
            Self::GoalCreate => &["create_goal"],
            Self::GoalGet => &["get_goal"],
            Self::GoalUpdate => &["update_goal"],
            Self::PlanExit => &["exit_plan_mode"],
            Self::PlanTodo => &["todo_write"],
            Self::SubagentInterrupt => &["interrupt_agent"],
            Self::SubagentSendMessage => &["send_message"],
            Self::SubagentList => &["list_agents"],
            Self::SubagentSpawn => &["subagent"],
            Self::SubagentFork => &["subagent_fork"],
            Self::Ralph => &["ralph"],
            Self::WorkflowRun => &["workflow"],
            Self::WebSearch => &["web_search"],
            Self::WebFetch => &["web_fetch"],
            Self::QuestionAsk => &["ask_user_question"],
            Self::ScheduleCreate => &["schedule_create"],
            Self::ScheduleList => &["schedule_list"],
            Self::ScheduleDelete => &["schedule_delete"],
            Self::CompositionInspect => &["composition_inspect"],
            Self::CompositionDefine => &["composition_define"],
            Self::CompositionValidate => &["composition_validate"],
            Self::CompositionRun => &["composition_run"],
            Self::CompositionStop => &["composition_stop"],
        }
    }

    pub fn parse(value: &str) -> Result<Self, TessivumError> {
        let tool = match value {
            "fs.read" => Self::FsRead,
            "fs.write" => Self::FsWrite,
            "fs.edit" => Self::FsEdit,
            "fs.str-replace-editor" => Self::FsStrReplaceEditor,
            "fs.read-image" => Self::FsReadImage,
            "search.glob" => Self::SearchGlob,
            "search.grep" => Self::SearchGrep,
            "shell.bash" => Self::ShellBash,
            "jobs.kill" => Self::JobsKill,
            "jobs.list" => Self::JobsList,
            "jobs.read" => Self::JobsRead,
            "jobs.wait" => Self::JobsWait,
            "skill.load" => Self::SkillLoad,
            "goal.create" => Self::GoalCreate,
            "goal.get" => Self::GoalGet,
            "goal.update" => Self::GoalUpdate,
            "plan.exit" => Self::PlanExit,
            "plan.todo" => Self::PlanTodo,
            "subagent.interrupt" => Self::SubagentInterrupt,
            "subagent.send-message" => Self::SubagentSendMessage,
            "subagent.list" => Self::SubagentList,
            "subagent.spawn" => Self::SubagentSpawn,
            "subagent.fork" => Self::SubagentFork,
            "ralph.run" => Self::Ralph,
            "workflow.run" => Self::WorkflowRun,
            "web.search" => Self::WebSearch,
            "web.fetch" => Self::WebFetch,
            "question.ask" => Self::QuestionAsk,
            "schedule.create" => Self::ScheduleCreate,
            "schedule.list" => Self::ScheduleList,
            "schedule.delete" => Self::ScheduleDelete,
            "composition.inspect" => Self::CompositionInspect,
            "composition.define" => Self::CompositionDefine,
            "composition.validate" => Self::CompositionValidate,
            "composition.run" => Self::CompositionRun,
            "composition.stop" => Self::CompositionStop,
            _ => {
                return Err(mode_error(
                    "MODE_UNKNOWN_TOOL_CAPABILITY",
                    "mode declares an unknown tool capability",
                    json!({"capability": value}),
                ))
            }
        };
        Ok(tool)
    }
}
impl fmt::Display for ToolCapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl Serialize for ToolCapabilityId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for ToolCapabilityId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModePluginRuntime {
    Native,
    Wasm,
    LegacyNode,
}
impl ModePluginRuntime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Wasm => "wasm",
            Self::LegacyNode => "legacy-node",
        }
    }
    fn parse(value: &str) -> Result<Self, TessivumError> {
        match value {
            "native" => Ok(Self::Native),
            "wasm" => Ok(Self::Wasm),
            "legacy-node" => Ok(Self::LegacyNode),
            "browser" => Err(mode_error(
                "MODE_BROWSER_RUNTIME_UNSUPPORTED",
                "browser plugins cannot be attached to an agent mode",
                json!({"runtime": value}),
            )),
            _ => Err(mode_error(
                "MODE_UNKNOWN_PLUGIN_RUNTIME",
                "mode plugin runtime is unknown",
                json!({"runtime": value}),
            )),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModePluginRef {
    pub id: String,
    pub runtime: ModePluginRuntime,
    pub source: String,
    pub config: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentModeSpec {
    pub id: AgentModeId,
    pub name: String,
    pub description: String,
    pub prompt: PromptPolicy,
    pub presentation: ToolPresentation,
    pub tools: Vec<ToolCapabilityId>,
    pub skills: bool,
    pub planning: bool,
    pub compaction: Option<CompactionPolicy>,
    pub plugins: Vec<ModePluginRef>,
    pub capabilities: ModeCapabilities,
}
impl AgentModeSpec {
    pub fn validate(&self) -> Result<(), TessivumError> {
        validate_spec(self)
    }
    /// Emits the one schema-1 TOML form accepted by custom-mode discovery.
    pub fn normalized_toml(&self) -> Result<String, TessivumError> {
        self.validate()?;
        toml::to_string_pretty(&ModeManifest::from_spec(self)).map_err(|error| {
            mode_error(
                "MODE_TOML_SERIALIZATION_FAILED",
                "mode.toml could not be serialized",
                json!({"reason": error.to_string()}),
            )
        })
    }
    pub fn native_tools(&self) -> Result<Vec<String>, TessivumError> {
        resolve_tool_capabilities(&self.tools)
    }
}

/// The immutable startup snapshot. Runtime code must not reopen a mode file per turn.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMode {
    pub spec: AgentModeSpec,
    pub trust: AgentModeTrust,
    pub resolved_tools: Vec<String>,
    /// The restricted catalog used inside a programmatic tool call.
    pub nested_tools: Vec<String>,
    pub resolved_plugins: Vec<ModePluginRef>,
    pub source_dir: Option<PathBuf>,
    pub plugin_sources: BTreeMap<String, PathBuf>,
}
impl ResolvedMode {
    pub fn plugin_source(&self, id: &str) -> Option<&Path> {
        self.plugin_sources.get(id).map(PathBuf::as_path)
    }
}

#[derive(Clone, Debug)]
pub struct AgentModeRegistry {
    roots: Vec<AgentModeRoot>,
    authoring_root: Option<PathBuf>,
}

impl AgentModeRegistry {
    pub fn new(system_root: impl Into<PathBuf>, user_root: impl Into<PathBuf>) -> Self {
        let system_root = system_root.into();
        let user_root = user_root.into();
        Self::with_roots(
            vec![
                AgentModeRoot {
                    path: system_root,
                    trust: AgentModeTrust::System,
                },
                AgentModeRoot {
                    path: user_root.clone(),
                    trust: AgentModeTrust::User,
                },
            ],
            Some(user_root),
        )
    }
    pub fn with_roots(roots: Vec<AgentModeRoot>, authoring_root: Option<PathBuf>) -> Self {
        Self {
            roots,
            authoring_root,
        }
    }
    pub fn authorable(&self) -> bool {
        self.authoring_root.is_some()
    }
    pub fn builtins() -> Vec<AgentModeSpec> {
        builtin_specs()
    }

    pub fn list(&self) -> Result<Vec<AgentModeSummary>, TessivumError> {
        self.locations().map(|locations| {
            locations
                .into_iter()
                .map(|location| AgentModeSummary {
                    id: location.resolved.spec.id.clone(),
                    trust: location.resolved.trust,
                    name: location.resolved.spec.name.clone(),
                    description: location.resolved.spec.description.clone(),
                    built_in: location.resolved.trust == AgentModeTrust::Builtin,
                })
                .collect()
        })
    }
    pub fn resolve(&self, id: impl AsRef<str>) -> Result<ResolvedMode, TessivumError> {
        Ok(self.location(id)?.resolved)
    }
    pub fn read(&self, id: impl AsRef<str>) -> Result<AgentModeDocument, TessivumError> {
        let location = self.location(id)?;
        let content = match &location.document_path {
            Some(path) => read_mode_document(path)?,
            None => location.resolved.spec.normalized_toml()?,
        };
        Ok(AgentModeDocument {
            id: location.resolved.spec.id.clone(),
            trust: location.resolved.trust,
            content,
            name: location.resolved.spec.name.clone(),
            description: location.resolved.spec.description.clone(),
            built_in: location.resolved.trust == AgentModeTrust::Builtin,
        })
    }
    pub fn normalized_toml(&self, id: impl AsRef<str>) -> Result<String, TessivumError> {
        self.location(id)?.resolved.spec.normalized_toml()
    }
    /// Custom modes return their `mode.toml`; built-ins have no source file.
    pub fn path(&self, id: impl AsRef<str>) -> Result<Option<PathBuf>, TessivumError> {
        Ok(self.location(id)?.document_path)
    }

    pub fn copy(
        &self,
        from: impl AsRef<str>,
        target: impl AsRef<str>,
        name: Option<String>,
    ) -> Result<AgentModeId, TessivumError> {
        let source = self.location(from)?;
        let target = AgentModeId::new(target.as_ref())?;
        reject_builtin_id(&target)?;
        if self
            .locations()?
            .iter()
            .any(|location| location.resolved.spec.id == target)
        {
            return Err(mode_error(
                "MODE_DUPLICATE_ID",
                "an agent mode with this id already exists",
                json!({"id": target.as_str()}),
            ));
        }
        let root = self.authoring_root()?;
        let requested = root.join(target.as_str());
        if requested.exists() {
            return Err(mode_error(
                "MODE_DUPLICATE_ID",
                "an agent mode with this id already exists",
                json!({"id": target.as_str()}),
            ));
        }
        fs::create_dir(&requested)
            .map_err(|error| io_error("creating mode directory", &requested, error))?;
        let destination = match confined_directory(&root, &requested) {
            Ok(path) => path,
            Err(error) => {
                let _ = fs::remove_dir_all(&requested);
                return Err(error);
            }
        };
        let mut spec = source.resolved.spec.clone();
        spec.id = target.clone();
        if let Some(name) = name {
            spec.name = name.trim().to_owned();
        }
        let result = (|| {
            spec.validate()?;
            copy_wasm_sources(&source.resolved, &destination)?;
            fs::write(destination.join(MODE_FILE_NAME), spec.normalized_toml()?)
                .map_err(|error| io_error("writing mode.toml", &destination, error))?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&destination);
            return Err(error);
        }
        Ok(target)
    }

    pub fn remove(&self, id: impl AsRef<str>) -> Result<(), TessivumError> {
        let location = self.location(id)?;
        if location.resolved.trust == AgentModeTrust::Builtin {
            return Err(immutable_error(&location.resolved.spec.id));
        }
        let root = self.authoring_root()?;
        let directory = location.resolved.source_dir.as_ref().ok_or_else(|| {
            mode_error(
                "MODE_READ_ONLY",
                "agent mode does not have a removable source directory",
                json!({"id": location.resolved.spec.id.as_str()}),
            )
        })?;
        if location.resolved.trust != AgentModeTrust::User
            || directory != &root.join(location.resolved.spec.id.as_str())
        {
            return Err(mode_error(
                "MODE_READ_ONLY",
                "agent mode is outside the writable mode root",
                json!({"id": location.resolved.spec.id.as_str()}),
            ));
        }
        fs::remove_dir_all(directory)
            .map_err(|error| io_error("removing mode directory", directory, error))
    }

    fn location(&self, id: impl AsRef<str>) -> Result<ModeLocation, TessivumError> {
        let id = AgentModeId::new(id.as_ref())?;
        self.locations()?
            .into_iter()
            .find(|location| location.resolved.spec.id == id)
            .ok_or_else(|| self.not_found(&id))
    }
    fn locations(&self) -> Result<Vec<ModeLocation>, TessivumError> {
        let mut locations = builtin_specs()
            .into_iter()
            .map(|spec| ModeLocation {
                resolved: resolve_spec(spec, AgentModeTrust::Builtin, None, BTreeMap::new())
                    .expect("built-in mode specs are valid"),
                document_path: None,
            })
            .collect::<Vec<_>>();
        let mut seen = locations
            .iter()
            .map(|location| location.resolved.spec.id.clone())
            .collect::<BTreeSet<_>>();
        for root in &self.roots {
            if root.trust == AgentModeTrust::Builtin {
                return Err(mode_error(
                    "MODE_ROOT_INVALID",
                    "a mode root cannot use built-in trust",
                    json!({"root": display_path(&root.path)}),
                ));
            }
            for location in scan_root(root)? {
                if location.resolved.spec.id.is_builtin() {
                    return Err(immutable_error(&location.resolved.spec.id));
                }
                if seen.insert(location.resolved.spec.id.clone()) {
                    locations.push(location);
                }
            }
        }
        Ok(locations)
    }
    fn authoring_root(&self) -> Result<PathBuf, TessivumError> {
        let root = self.authoring_root.as_ref().ok_or_else(|| {
            mode_error(
                "MODE_AUTHORING_UNAVAILABLE",
                "agent mode authoring is unavailable",
                json!({}),
            )
        })?;
        let root = canonical_root(root)?;
        if !self.roots.iter().any(|configured| {
            configured.trust == AgentModeTrust::User
                && canonical_root(&configured.path).ok().as_ref() == Some(&root)
        }) {
            return Err(mode_error(
                "MODE_AUTHORING_ROOT_INVALID",
                "the authoring root is not a configured user mode root",
                json!({"root": display_path(&root)}),
            ));
        }
        Ok(root)
    }
    fn not_found(&self, id: &AgentModeId) -> TessivumError {
        let available = self
            .locations()
            .map(|locations| {
                locations
                    .into_iter()
                    .map(|location| location.resolved.spec.id.into_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        mode_error(
            "MODE_NOT_FOUND",
            "agent mode was not found",
            json!({"id": id.as_str(), "available": available}),
        )
    }
}

#[derive(Clone, Debug)]
struct ModeLocation {
    resolved: ResolvedMode,
    document_path: Option<PathBuf>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModeManifest {
    schema: u32,
    id: AgentModeId,
    name: String,
    description: String,
    prompt: PromptPolicy,
    tools: ModeToolsManifest,
    capabilities: ModeCapabilities,
    #[serde(default)]
    plugins: Vec<ModePluginManifest>,
}
impl ModeManifest {
    fn into_spec(self) -> Result<AgentModeSpec, TessivumError> {
        if self.schema != MODE_SCHEMA_VERSION {
            return Err(mode_error(
                "MODE_SCHEMA_UNSUPPORTED",
                "mode schema is unsupported",
                json!({"schema": self.schema, "supported": MODE_SCHEMA_VERSION}),
            ));
        }
        let tools = self
            .tools
            .enabled
            .iter()
            .map(|id| ToolCapabilityId::parse(id))
            .collect::<Result<Vec<_>, _>>()?;
        let plugins = self
            .plugins
            .into_iter()
            .map(ModePluginManifest::into_ref)
            .collect::<Result<Vec<_>, _>>()?;
        let spec = AgentModeSpec {
            id: self.id,
            name: self.name,
            description: self.description,
            prompt: self.prompt,
            presentation: self.tools.presentation,
            tools,
            skills: self.capabilities.skills,
            planning: self.capabilities.planning,
            compaction: self
                .capabilities
                .compaction
                .then_some(CompactionPolicy::Standard),
            plugins,
            capabilities: self.capabilities,
        };
        spec.validate()?;
        Ok(spec)
    }
    fn from_spec(spec: &AgentModeSpec) -> Self {
        Self {
            schema: MODE_SCHEMA_VERSION,
            id: spec.id.clone(),
            name: spec.name.clone(),
            description: spec.description.clone(),
            prompt: spec.prompt.clone(),
            tools: ModeToolsManifest {
                presentation: spec.presentation,
                enabled: spec.tools.iter().map(ToString::to_string).collect(),
            },
            capabilities: spec.capabilities.clone(),
            plugins: spec
                .plugins
                .iter()
                .map(|plugin| ModePluginManifest {
                    id: plugin.id.clone(),
                    runtime: plugin.runtime.as_str().into(),
                    source: plugin.source.clone(),
                    config: plugin.config.clone(),
                })
                .collect(),
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModeToolsManifest {
    presentation: ToolPresentation,
    enabled: Vec<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModePluginManifest {
    id: String,
    runtime: String,
    source: String,
    #[serde(
        default = "empty_json_object",
        skip_serializing_if = "is_empty_json_object"
    )]
    config: Value,
}
impl ModePluginManifest {
    fn into_ref(self) -> Result<ModePluginRef, TessivumError> {
        Ok(ModePluginRef {
            id: self.id,
            runtime: ModePluginRuntime::parse(&self.runtime)?,
            source: self.source,
            config: self.config,
        })
    }
}

fn builtin_specs() -> Vec<AgentModeSpec> {
    let standard_tools = standard_tool_capabilities();
    vec![
        AgentModeSpec {
            id: AgentModeId::standard(),
            name: "标准模式".into(),
            description: "功能完整的编码 Agent，支持文件编辑、Shell、文件与网页检索、Skills、计划、目标、子代理和工作流。".into(),
            prompt: PromptPolicy {
                complete: false,
                text: "Use the additive Tessivum persona, workspace instructions, and runtime context.".into(),
            },
            presentation: ToolPresentation::Direct,
            tools: standard_tools.clone(),
            skills: true,
            planning: true,
            compaction: Some(CompactionPolicy::Standard),
            plugins: Vec::new(),
            capabilities: ModeCapabilities {
                skills: true,
                planning: true,
                compaction: true,
                ..ModeCapabilities::default()
            },
        },
        AgentModeSpec {
            id: AgentModeId::ptc(),
            name: "PTC 模式".into(),
            description: "具备标准模式的全部能力，并通过 run_code 让模型用一个 JavaScript 程序组合多步操作。".into(),
            prompt: PromptPolicy {
                complete: false,
                text: concat!(
                    "## Writing code for run_code\n",
                    "Use the `run_code` tool for model-directed actions. Its JavaScript program receives the native tool ",
                    "SDK as `declare const tools`; call `await tools.<name>(arguments)` for each operation and return a ",
                    "JSON-serializable result.",
                ).into(),
            },
            presentation: ToolPresentation::Programmatic,
            tools: standard_tools.clone(),
            skills: true,
            planning: true,
            compaction: Some(CompactionPolicy::Standard),
            plugins: Vec::new(),
            capabilities: ModeCapabilities {
                skills: true,
                planning: true,
                compaction: true,
                bun: true,
                ..ModeCapabilities::default()
            },
        },
        AgentModeSpec {
            id: AgentModeId::minimal(),
            name: "极简模式".into(),
            description: "仅提供持久 bash 与 str_replace_editor 的双工具编码 Agent。".into(),
            prompt: PromptPolicy {
                complete: true,
                text: "You are a helpful software engineer assistant.\n\nUse only bash and str_replace_editor to complete the task.".into(),
            },
            presentation: ToolPresentation::Direct,
            tools: vec![
                ToolCapabilityId::ShellBash,
                ToolCapabilityId::FsStrReplaceEditor,
            ],
            skills: false,
            planning: false,
            compaction: None,
            plugins: Vec::new(),
            capabilities: ModeCapabilities {
                persistent_shell: true,
                ..ModeCapabilities::default()
            },
        },
        AgentModeSpec {
            id: AgentModeId::composition(),
            name: "创造模式".into(),
            description: "具备标准模式的全部能力，并提供 Native、WASM 与 Legacy Entry 组合工具。".into(),
            prompt: PromptPolicy {
                complete: false,
                text: "Use composition tools to define, validate, run, inspect, and stop typed Native, WASM, or Legacy entries. Never execute arbitrary source code.".into(),
            },
            presentation: ToolPresentation::Direct,
            tools: standard_tools
                .into_iter()
                .chain([
                    ToolCapabilityId::CompositionInspect,
                    ToolCapabilityId::CompositionDefine,
                    ToolCapabilityId::CompositionValidate,
                    ToolCapabilityId::CompositionRun,
                    ToolCapabilityId::CompositionStop,
                ])
                .collect(),
            skills: true,
            planning: true,
            compaction: Some(CompactionPolicy::Standard),
            plugins: Vec::new(),
            capabilities: ModeCapabilities {
                skills: true,
                planning: true,
                compaction: true,
                composition: true,
                ..ModeCapabilities::default()
            },
        },
    ]
}

fn standard_tool_capabilities() -> Vec<ToolCapabilityId> {
    vec![
        ToolCapabilityId::QuestionAsk,
        ToolCapabilityId::ShellBash,
        ToolCapabilityId::GoalCreate,
        ToolCapabilityId::GoalGet,
        ToolCapabilityId::GoalUpdate,
        ToolCapabilityId::FsRead,
        ToolCapabilityId::FsWrite,
        ToolCapabilityId::FsEdit,
        ToolCapabilityId::FsStrReplaceEditor,
        ToolCapabilityId::FsReadImage,
        ToolCapabilityId::SearchGlob,
        ToolCapabilityId::SearchGrep,
        ToolCapabilityId::JobsKill,
        ToolCapabilityId::JobsList,
        ToolCapabilityId::JobsRead,
        ToolCapabilityId::JobsWait,
        ToolCapabilityId::PlanExit,
        ToolCapabilityId::PlanTodo,
        ToolCapabilityId::SkillLoad,
        ToolCapabilityId::SubagentInterrupt,
        ToolCapabilityId::SubagentSendMessage,
        ToolCapabilityId::SubagentList,
        ToolCapabilityId::SubagentSpawn,
        ToolCapabilityId::SubagentFork,
        ToolCapabilityId::Ralph,
        ToolCapabilityId::ScheduleCreate,
        ToolCapabilityId::ScheduleList,
        ToolCapabilityId::ScheduleDelete,
        ToolCapabilityId::WebSearch,
        ToolCapabilityId::WebFetch,
        ToolCapabilityId::WorkflowRun,
    ]
}

fn scan_root(root: &AgentModeRoot) -> Result<Vec<ModeLocation>, TessivumError> {
    let root_path = canonical_root(&root.path)?;
    let mut entries = fs::read_dir(&root_path)
        .map_err(|error| io_error("reading mode root", &root_path, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io_error("reading mode root", &root_path, error))?;
    if entries.len() > MAX_MODES_PER_ROOT {
        return Err(mode_error(
            "MODE_LIMIT_EXCEEDED",
            "mode root contains too many entries",
            json!({"root": display_path(&root_path), "maximum": MAX_MODES_PER_ROOT}),
        ));
    }
    entries.sort_by_key(|entry| entry.file_name());
    let mut locations = Vec::new();
    for entry in entries {
        let candidate = entry.path();
        if !fs::metadata(&candidate)
            .map_err(|error| io_error("reading mode root entry", &candidate, error))?
            .is_dir()
        {
            continue;
        }
        let directory = confined_directory(&root_path, &candidate)?;
        let document_path = directory.join(MODE_FILE_NAME);
        if !document_path.exists() {
            continue;
        }
        let document_path = confined_file(&directory, &document_path)?;
        let directory_id = entry.file_name().to_string_lossy().into_owned();
        validate_mode_id(&directory_id)?;
        let spec = parse_mode_document(&document_path)?;
        if spec.id.as_str() != directory_id {
            return Err(mode_error(
                "MODE_ID_MISMATCH",
                "mode id does not match its directory",
                json!({"id": spec.id.as_str(), "directory": directory_id}),
            ));
        }
        let plugin_sources = resolve_plugin_sources(&spec, &directory)?;
        locations.push(ModeLocation {
            resolved: resolve_spec(spec, root.trust, Some(directory), plugin_sources)?,
            document_path: Some(document_path),
        });
    }
    Ok(locations)
}

fn parse_mode_document(path: &Path) -> Result<AgentModeSpec, TessivumError> {
    toml::from_str::<ModeManifest>(&read_mode_document(path)?)
        .map_err(|error| {
            mode_error(
                "MODE_TOML_INVALID",
                "mode.toml is invalid",
                json!({"path": display_path(path), "reason": error.to_string()}),
            )
        })?
        .into_spec()
}
fn read_mode_document(path: &Path) -> Result<String, TessivumError> {
    let bytes = fs::read(path).map_err(|error| io_error("reading mode.toml", path, error))?;
    if bytes.len() > MAX_MODE_DOCUMENT_BYTES {
        return Err(mode_error(
            "MODE_DOCUMENT_TOO_LARGE",
            "mode.toml exceeds the document size limit",
            json!({"path": display_path(path), "maximum": MAX_MODE_DOCUMENT_BYTES}),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        mode_error(
            "MODE_DOCUMENT_INVALID_UTF8",
            "mode.toml must be UTF-8",
            json!({"path": display_path(path)}),
        )
    })
}
fn resolve_spec(
    spec: AgentModeSpec,
    trust: AgentModeTrust,
    source_dir: Option<PathBuf>,
    plugin_sources: BTreeMap<String, PathBuf>,
) -> Result<ResolvedMode, TessivumError> {
    spec.validate()?;
    let nested_tools = spec.native_tools()?;
    let resolved_tools = match spec.presentation {
        ToolPresentation::Direct => nested_tools.clone(),
        ToolPresentation::Programmatic => vec!["run_code".into()],
    };
    Ok(ResolvedMode {
        resolved_plugins: spec.plugins.clone(),
        spec,
        trust,
        resolved_tools,
        nested_tools,
        source_dir,
        plugin_sources,
    })
}

fn validate_spec(spec: &AgentModeSpec) -> Result<(), TessivumError> {
    validate_mode_id(spec.id.as_str())?;
    validate_text("name", &spec.name, false)?;
    validate_text("description", &spec.description, true)?;
    validate_text("prompt.text", &spec.prompt.text, false)?;
    if spec.tools.len() > MAX_MODE_TOOLS {
        return Err(mode_error(
            "MODE_TOOL_LIMIT_EXCEEDED",
            "mode declares too many tool capabilities",
            json!({"maximum": MAX_MODE_TOOLS}),
        ));
    }
    let mut capabilities = BTreeSet::new();
    for capability in &spec.tools {
        if !capabilities.insert(*capability) {
            return Err(mode_error(
                "MODE_DUPLICATE_TOOL_CAPABILITY",
                "mode declares a duplicate tool capability",
                json!({"capability": capability.as_str()}),
            ));
        }
    }
    let _ = resolve_tool_capabilities(&spec.tools)?;
    if spec.plugins.len() > MAX_MODE_PLUGINS {
        return Err(mode_error(
            "MODE_PLUGIN_LIMIT_EXCEEDED",
            "mode declares too many plugins",
            json!({"maximum": MAX_MODE_PLUGINS}),
        ));
    }
    let mut plugin_ids = BTreeSet::new();
    for plugin in &spec.plugins {
        validate_plugin(plugin)?;
        if !plugin_ids.insert(plugin.id.as_str()) {
            return Err(mode_error(
                "MODE_DUPLICATE_PLUGIN_ID",
                "mode declares a duplicate plugin id",
                json!({"plugin": plugin.id}),
            ));
        }
    }
    if spec.skills != spec.capabilities.skills || spec.planning != spec.capabilities.planning {
        return Err(mode_error(
            "MODE_CAPABILITIES_INCONSISTENT",
            "mode capability flags do not match the declared policy",
            json!({}),
        ));
    }
    if spec.compaction.is_some() != spec.capabilities.compaction {
        return Err(mode_error(
            "MODE_COMPACTION_POLICY_INCONSISTENT",
            "mode compaction policy does not match the compaction capability",
            json!({}),
        ));
    }
    if spec.presentation == ToolPresentation::Programmatic && !spec.capabilities.bun {
        return Err(mode_error(
            "MODE_PROGRAMMATIC_RUNTIME_REQUIRED",
            "programmatic tool presentation requires the Bun capability",
            json!({}),
        ));
    }
    if spec.capabilities.persistent_shell && !capabilities.contains(&ToolCapabilityId::ShellBash) {
        return Err(mode_error(
            "MODE_PERSISTENT_SHELL_REQUIRES_BASH",
            "persistent-shell capability requires shell.bash",
            json!({}),
        ));
    }
    let composition_tool = spec.tools.iter().any(|tool| {
        matches!(
            tool,
            ToolCapabilityId::CompositionInspect
                | ToolCapabilityId::CompositionDefine
                | ToolCapabilityId::CompositionValidate
                | ToolCapabilityId::CompositionRun
                | ToolCapabilityId::CompositionStop
        )
    });
    if composition_tool && !spec.capabilities.composition {
        return Err(mode_error(
            "MODE_COMPOSITION_CAPABILITY_REQUIRED",
            "composition tools require the composition capability",
            json!({}),
        ));
    }
    if spec.capabilities.skills && !capabilities.contains(&ToolCapabilityId::SkillLoad) {
        return Err(mode_error(
            "MODE_SKILLS_TOOL_REQUIRED",
            "skills capability requires skill.load",
            json!({}),
        ));
    }
    if spec.capabilities.planning
        && (!capabilities.contains(&ToolCapabilityId::PlanExit)
            || !capabilities.contains(&ToolCapabilityId::PlanTodo))
    {
        return Err(mode_error(
            "MODE_PLANNING_TOOLS_REQUIRED",
            "planning capability requires plan.exit and plan.todo",
            json!({}),
        ));
    }
    Ok(())
}

/// The sole capability-ID to native-tool mapping used by future runtime code.
pub fn resolve_tool_capabilities(
    capabilities: &[ToolCapabilityId],
) -> Result<Vec<String>, TessivumError> {
    let mut seen_capabilities = BTreeSet::new();
    let mut seen_tools = BTreeSet::new();
    let mut tools = Vec::new();
    for capability in capabilities {
        if !seen_capabilities.insert(*capability) {
            return Err(mode_error(
                "MODE_DUPLICATE_TOOL_CAPABILITY",
                "mode declares a duplicate tool capability",
                json!({"capability": capability.as_str()}),
            ));
        }
        for tool in capability.native_tools() {
            if !seen_tools.insert(*tool) {
                return Err(mode_error(
                    "MODE_DUPLICATE_NATIVE_TOOL",
                    "mode capability resolution produced a duplicate native tool",
                    json!({"tool": tool, "capability": capability.as_str()}),
                ));
            }
            tools.push((*tool).into());
        }
    }
    Ok(tools)
}

fn validate_plugin(plugin: &ModePluginRef) -> Result<(), TessivumError> {
    if plugin.id.trim().is_empty() || plugin.id.len() > MAX_MODE_ID_BYTES {
        return Err(mode_error(
            "MODE_PLUGIN_ID_INVALID",
            "mode plugin id is invalid",
            json!({"plugin": plugin.id}),
        ));
    }
    if plugin.source.trim().is_empty() || plugin.source.len() > MAX_MODE_PLUGIN_SOURCE_BYTES {
        return Err(mode_error(
            "MODE_PLUGIN_SOURCE_INVALID",
            "mode plugin source is invalid",
            json!({"plugin": plugin.id, "maximum": MAX_MODE_PLUGIN_SOURCE_BYTES}),
        ));
    }
    if !plugin.config.is_object() {
        return Err(mode_error(
            "MODE_PLUGIN_CONFIG_INVALID",
            "mode plugin config must be an object",
            json!({"plugin": plugin.id}),
        ));
    }
    let config_bytes = serde_json::to_vec(&plugin.config).map_err(|error| {
        mode_error(
            "MODE_PLUGIN_CONFIG_INVALID",
            "mode plugin config cannot be encoded",
            json!({"plugin": plugin.id, "reason": error.to_string()}),
        )
    })?;
    if config_bytes.len() > MAX_MODE_PLUGIN_CONFIG_BYTES {
        return Err(mode_error(
            "MODE_PLUGIN_CONFIG_TOO_LARGE",
            "mode plugin config exceeds the size limit",
            json!({"plugin": plugin.id, "maximum": MAX_MODE_PLUGIN_CONFIG_BYTES}),
        ));
    }
    Ok(())
}
fn resolve_plugin_sources(
    spec: &AgentModeSpec,
    directory: &Path,
) -> Result<BTreeMap<String, PathBuf>, TessivumError> {
    let mut sources = BTreeMap::new();
    for plugin in &spec.plugins {
        if plugin.runtime != ModePluginRuntime::Wasm {
            continue;
        }
        let relative = Path::new(&plugin.source);
        if !is_relative_path(relative) {
            return Err(mode_error(
                "MODE_PLUGIN_SOURCE_INVALID",
                "WASM plugin source must be a confined relative path",
                json!({"plugin": plugin.id, "source": plugin.source}),
            ));
        }
        let source = fs::canonicalize(directory.join(relative)).map_err(|error| {
            mode_error(
                "MODE_PLUGIN_SOURCE_MISSING",
                "WASM plugin source does not exist",
                json!({"plugin": plugin.id, "source": plugin.source, "error": error.to_string()}),
            )
        })?;
        if !source.starts_with(directory) {
            return Err(mode_error(
                "MODE_PATH_OUTSIDE_ROOT",
                "mode plugin source escapes its mode directory",
                json!({"plugin": plugin.id, "source": plugin.source}),
            ));
        }
        if !fs::metadata(&source)
            .map_err(|error| io_error("reading plugin source", &source, error))?
            .is_file()
        {
            return Err(mode_error(
                "MODE_PLUGIN_SOURCE_INVALID",
                "WASM plugin source must be a regular file",
                json!({"plugin": plugin.id, "source": plugin.source}),
            ));
        }
        sources.insert(plugin.id.clone(), source);
    }
    Ok(sources)
}
fn copy_wasm_sources(source: &ResolvedMode, destination: &Path) -> Result<(), TessivumError> {
    for plugin in &source.spec.plugins {
        if plugin.runtime != ModePluginRuntime::Wasm {
            continue;
        }
        let input = source.plugin_sources.get(&plugin.id).ok_or_else(|| {
            mode_error(
                "MODE_PLUGIN_SOURCE_MISSING",
                "WASM plugin source was not resolved",
                json!({"plugin": plugin.id}),
            )
        })?;
        let relative = Path::new(&plugin.source);
        let output = destination.join(relative);
        let parent = output.parent().ok_or_else(|| {
            mode_error(
                "MODE_PLUGIN_SOURCE_INVALID",
                "WASM plugin source has no parent directory",
                json!({"plugin": plugin.id}),
            )
        })?;
        fs::create_dir_all(parent)
            .map_err(|error| io_error("creating plugin source directory", parent, error))?;
        let parent = confined_directory(destination, parent)?;
        let output = parent.join(output.file_name().ok_or_else(|| {
            mode_error(
                "MODE_PLUGIN_SOURCE_INVALID",
                "WASM plugin source has no file name",
                json!({"plugin": plugin.id}),
            )
        })?);
        fs::copy(input, &output)
            .map_err(|error| io_error("copying plugin source", &output, error))?;
    }
    Ok(())
}

fn canonical_root(path: &Path) -> Result<PathBuf, TessivumError> {
    let path = fs::canonicalize(path).map_err(|error| {
        mode_error(
            "MODE_ROOT_MISSING",
            "agent mode root does not exist",
            json!({"root": display_path(path), "error": error.to_string()}),
        )
    })?;
    if !fs::metadata(&path)
        .map_err(|error| io_error("reading mode root", &path, error))?
        .is_dir()
    {
        return Err(mode_error(
            "MODE_ROOT_INVALID",
            "agent mode root is not a directory",
            json!({"root": display_path(&path)}),
        ));
    }
    Ok(path)
}
fn confined_directory(root: &Path, path: &Path) -> Result<PathBuf, TessivumError> {
    let path = fs::canonicalize(path)
        .map_err(|error| io_error("canonicalizing mode directory", path, error))?;
    if !path.starts_with(root) {
        return Err(mode_error(
            "MODE_PATH_OUTSIDE_ROOT",
            "mode directory escapes its configured root",
            json!({"root": display_path(root), "path": display_path(&path)}),
        ));
    }
    if !fs::metadata(&path)
        .map_err(|error| io_error("reading mode directory", &path, error))?
        .is_dir()
    {
        return Err(mode_error(
            "MODE_PATH_INVALID",
            "mode path is not a directory",
            json!({"path": display_path(&path)}),
        ));
    }
    Ok(path)
}
fn confined_file(root: &Path, path: &Path) -> Result<PathBuf, TessivumError> {
    let path = fs::canonicalize(path)
        .map_err(|error| io_error("canonicalizing mode.toml", path, error))?;
    if !path.starts_with(root) {
        return Err(mode_error(
            "MODE_PATH_OUTSIDE_ROOT",
            "mode.toml escapes its mode directory",
            json!({"root": display_path(root), "path": display_path(&path)}),
        ));
    }
    if !fs::metadata(&path)
        .map_err(|error| io_error("reading mode.toml", &path, error))?
        .is_file()
    {
        return Err(mode_error(
            "MODE_PATH_INVALID",
            "mode.toml is not a regular file",
            json!({"path": display_path(&path)}),
        ));
    }
    Ok(path)
}
fn validate_mode_id(id: &str) -> Result<(), TessivumError> {
    let valid = !id.is_empty()
        && id.len() <= MAX_MODE_ID_BYTES
        && id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !id.ends_with('-')
        && !id.contains("--");
    if valid {
        Ok(())
    } else {
        Err(mode_error(
            "MODE_INVALID_ID",
            "mode id must match [a-z][a-z0-9-]* without repeated or trailing hyphens",
            json!({"id": id, "maximum": MAX_MODE_ID_BYTES}),
        ))
    }
}
fn reject_builtin_id(id: &AgentModeId) -> Result<(), TessivumError> {
    if id.is_builtin() {
        Err(immutable_error(id))
    } else {
        Ok(())
    }
}
fn immutable_error(id: &AgentModeId) -> TessivumError {
    mode_error(
        "MODE_IMMUTABLE",
        "built-in agent modes are immutable",
        json!({"id": id.as_str()}),
    )
}
fn validate_text(field: &str, value: &str, allow_empty: bool) -> Result<(), TessivumError> {
    if value.len() <= MAX_MODE_DOCUMENT_BYTES
        && (allow_empty || !value.trim().is_empty())
        && !value.contains('\0')
    {
        Ok(())
    } else {
        Err(mode_error(
            "MODE_TEXT_INVALID",
            "mode text is invalid",
            json!({"field": field}),
        ))
    }
}
fn is_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::CurDir | Component::Normal(_)))
}
fn empty_json_object() -> Value {
    Value::Object(Map::new())
}
fn is_empty_json_object(value: &Value) -> bool {
    value.as_object().is_some_and(Map::is_empty)
}
fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
fn io_error(operation: &str, path: &Path, error: std::io::Error) -> TessivumError {
    mode_error(
        "MODE_IO",
        "agent mode storage operation failed",
        json!({"operation": operation, "path": display_path(path), "error": error.to_string()}),
    )
}
fn mode_error(code: &str, message: &str, details: Value) -> TessivumError {
    TessivumError::new(code, message, "agent-mode", details)
}

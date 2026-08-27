use std::{
    fs,
    path::{Path, PathBuf},
};

use tessivum::agent_mode::{
    AgentModeId, AgentModeRegistry, ModePluginRuntime, ToolCapabilityId, ToolPresentation,
};
use uuid::Uuid;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("tessivum-agent-mode-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn directory(&self, name: &str) -> PathBuf {
        let path = self.path().join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn mode(&self, root: &Path, id: &str, document: &str) -> PathBuf {
        let directory = root.join(id);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("mode.toml");
        fs::write(&path, document).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn basic_mode(id: &str, name: &str) -> String {
    format!(
        r#"schema = 1
id = "{id}"
name = "{name}"
description = "Focused repository maintenance mode"

[prompt]
complete = false
text = "Make the smallest correct change."

[tools]
presentation = "direct"
enabled = ["fs.read", "fs.edit", "search.glob", "search.grep", "shell.bash"]

[capabilities]
skills = false
planning = false
compaction = false
"#
    )
}

#[test]
fn builtins_are_the_complete_native_roster() {
    let registry = AgentModeRegistry::with_roots(Vec::new(), None);
    let summaries = registry.list().unwrap();
    assert_eq!(
        summaries
            .iter()
            .map(|summary| summary.id.as_str())
            .collect::<Vec<_>>(),
        vec!["standard", "ptc", "minimal", "composition"]
    );

    let standard = registry.resolve("standard").unwrap();
    assert_eq!(standard.spec.presentation, ToolPresentation::Direct);
    assert!(standard.spec.capabilities.skills);
    assert!(standard.spec.capabilities.planning);
    assert!(standard.spec.capabilities.compaction);
    assert!(!standard.resolved_tools.contains(&"run_code".into()));
    assert!(!standard
        .resolved_tools
        .iter()
        .any(|tool| tool.starts_with("composition_")));

    let ptc = registry.resolve("ptc").unwrap();
    assert_eq!(ptc.spec.presentation, ToolPresentation::Programmatic);
    assert_eq!(ptc.resolved_tools, vec!["run_code"]);
    assert!(ptc.spec.capabilities.bun);
    assert!(!ptc.nested_tools.contains(&"run_code".into()));

    let minimal = registry.resolve("minimal").unwrap();
    assert!(minimal.spec.prompt.complete);
    assert_eq!(minimal.resolved_tools, vec!["bash", "str_replace_editor"]);
    assert!(minimal.spec.capabilities.persistent_shell);
    assert!(minimal
        .spec
        .normalized_toml()
        .unwrap()
        .lines()
        .any(|line| line == "persistent-shell = true"));
    assert!(!minimal.spec.capabilities.skills);
    assert!(!minimal.spec.capabilities.planning);
    assert!(!minimal.spec.capabilities.compaction);

    let composition = registry.resolve("composition").unwrap();
    assert!(composition.spec.capabilities.composition);
    assert_eq!(
        composition
            .resolved_tools
            .iter()
            .filter(|tool| tool.starts_with("composition_"))
            .count(),
        5
    );
    assert!(!composition
        .resolved_tools
        .iter()
        .any(|tool| tool.starts_with("cordis_")));
}

#[test]
fn custom_modes_are_strict_and_precedence_is_root_order() {
    let temp = TempDir::new("strict-precedence");
    let system = temp.directory("system");
    let user = temp.directory("user");
    temp.mode(
        &system,
        "maintainer",
        &basic_mode("maintainer", "System mode"),
    );
    temp.mode(&user, "maintainer", &basic_mode("maintainer", "User mode"));
    let registry = AgentModeRegistry::new(&system, &user);
    assert_eq!(
        registry.resolve("maintainer").unwrap().spec.name,
        "System mode"
    );

    temp.mode(
        &user,
        "unknown-field",
        &format!(
            "{}unknown = true\n",
            basic_mode("unknown-field", "Unknown field")
        ),
    );
    let error = registry.list().unwrap_err();
    assert_eq!(error.code, "MODE_TOML_INVALID");
}

#[test]
fn copy_remove_and_normalized_round_trip_are_confined() {
    let temp = TempDir::new("authoring");
    let system = temp.directory("system");
    let user = temp.directory("user");
    let registry = AgentModeRegistry::new(&system, &user);

    let copied = registry
        .copy("standard", "my-standard", Some("My Standard".into()))
        .unwrap();
    assert_eq!(copied, AgentModeId::new("my-standard").unwrap());
    let path = registry.path("my-standard").unwrap().unwrap();
    assert_eq!(
        path,
        user.join("my-standard")
            .join("mode.toml")
            .canonicalize()
            .unwrap()
    );
    assert!(registry
        .read("my-standard")
        .unwrap()
        .content
        .starts_with("schema = 1\n"));
    assert_eq!(
        registry.resolve("my-standard").unwrap().spec.name,
        "My Standard"
    );

    registry.remove("my-standard").unwrap();
    assert!(!path.exists());
    assert_eq!(
        registry.remove("standard").unwrap_err().code,
        "MODE_IMMUTABLE"
    );
    assert_eq!(
        registry
            .copy("standard", "standard", None)
            .unwrap_err()
            .code,
        "MODE_IMMUTABLE"
    );
}

#[test]
fn manifest_rejects_path_escape_browser_runtime_unknown_capabilities_and_duplicates() {
    let temp = TempDir::new("rejections");
    let system = temp.directory("system");
    let user = temp.directory("user");
    let registry = AgentModeRegistry::new(&system, &user);

    temp.mode(
        &user,
        "escape",
        &format!(
            "{}\n[[plugins]]\nid = \"plugin\"\nruntime = \"wasm\"\nsource = \"../outside\"\n",
            basic_mode("escape", "Escape")
        ),
    );
    assert_eq!(
        registry.resolve("escape").unwrap_err().code,
        "MODE_PLUGIN_SOURCE_INVALID"
    );
    fs::remove_dir_all(user.join("escape")).unwrap();

    temp.mode(
        &user,
        "browser",
        &format!(
            "{}\n[[plugins]]\nid = \"plugin\"\nruntime = \"browser\"\nsource = \"plugin\"\n",
            basic_mode("browser", "Browser")
        ),
    );
    assert_eq!(
        registry.resolve("browser").unwrap_err().code,
        "MODE_BROWSER_RUNTIME_UNSUPPORTED"
    );
    fs::remove_dir_all(user.join("browser")).unwrap();

    temp.mode(
        &user,
        "unknown-capability",
        &basic_mode("unknown-capability", "Unknown").replace("\"fs.read\"", "\"not.a.capability\""),
    );
    assert_eq!(
        registry.resolve("unknown-capability").unwrap_err().code,
        "MODE_UNKNOWN_TOOL_CAPABILITY"
    );
    fs::remove_dir_all(user.join("unknown-capability")).unwrap();

    temp.mode(
        &user,
        "duplicates",
        &basic_mode("duplicates", "Duplicates").replace("\"fs.edit\", ", "\"fs.read\", "),
    );
    assert_eq!(
        registry.resolve("duplicates").unwrap_err().code,
        "MODE_DUPLICATE_TOOL_CAPABILITY"
    );
    fs::remove_dir_all(user.join("duplicates")).unwrap();
    temp.mode(
        &user,
        "duplicate-plugins",
        &format!(
            "{}\n[[plugins]]\nid = \"same\"\nruntime = \"native\"\nsource = \"entry\"\n\n[[plugins]]\nid = \"same\"\nruntime = \"native\"\nsource = \"entry\"\n",
            basic_mode("duplicate-plugins", "Duplicate plugins")
        ),
    );
    assert_eq!(
        registry.resolve("duplicate-plugins").unwrap_err().code,
        "MODE_DUPLICATE_PLUGIN_ID"
    );
}

#[test]
fn immutable_ids_and_plugin_sources_are_checked_before_resolution() {
    let temp = TempDir::new("immutable");
    let system = temp.directory("system");
    let user = temp.directory("user");
    let registry = AgentModeRegistry::new(&system, &user);
    temp.mode(&user, "standard", &basic_mode("standard", "Override"));
    assert_eq!(registry.list().unwrap_err().code, "MODE_IMMUTABLE");
    fs::remove_dir_all(user.join("standard")).unwrap();

    let plugin_dir = user.join("wasm-mode").join("plugins");
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(plugin_dir.join("plugin.json"), "{}").unwrap();
    fs::write(
        user.join("wasm-mode").join("mode.toml"),
        format!(
            "{}\n[[plugins]]\nid = \"wasm-plugin\"\nruntime = \"wasm\"\nsource = \"./plugins/plugin.json\"\n",
            basic_mode("wasm-mode", "Wasm")
        ),
    ).unwrap();
    let resolved = registry.resolve("wasm-mode").unwrap();
    assert_eq!(
        resolved.plugin_source("wasm-plugin").unwrap(),
        plugin_dir.join("plugin.json").canonicalize().unwrap()
    );
    assert_eq!(
        resolved.resolved_plugins[0].runtime,
        ModePluginRuntime::Wasm
    );
    assert_eq!(
        ToolCapabilityId::parse("missing").unwrap_err().code,
        "MODE_UNKNOWN_TOOL_CAPABILITY"
    );
}

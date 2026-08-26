use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::json;
use tessivum::{
    cli::{parse_cli, resolve_data_root, CliCommand},
    plugin_manager::{load_plugin_entries, plugin_profile_root},
};
use tessivum_core::RuntimeKind;

#[test]
fn plugin_profile_loads_plain_cordis_packages_and_bundle_insertions() {
    let temp = TempDir::new();
    let profile = temp.0.join("plugins");
    fs::create_dir_all(profile.join("node_modules")).unwrap();
    write_json(
        &profile.join("package.json"),
        json!({
            "name": "test-profile",
            "private": true,
            "dependencies": {"plain-plugin": "1.0.0", "plugin-bundle": "1.0.0"}
        }),
    );
    write_package(&profile, "plain-plugin", json!({}));
    write_package(
        &profile,
        "plugin-bundle",
        json!({"dsh": {"bundle": {"patch": "./cordis.patch.yml"}}}),
    );
    write_package(&profile, "nested-plugin", json!({}));
    fs::write(
        profile.join("node_modules/plugin-bundle/cordis.patch.yml"),
        "- insert:\n    - id: nested\n      name: nested-plugin\n      config:\n        answer: 42\n",
    )
    .unwrap();

    let tree = load_plugin_entries(&profile).unwrap().unwrap();
    let entries = tree.entries();
    assert_eq!(entries.len(), 2);
    let plain = entries
        .iter()
        .find(|entry| entry.options.name.as_deref() == Some("plain-plugin"))
        .unwrap();
    let nested = entries
        .iter()
        .find(|entry| entry.options.id.as_str() == "nested")
        .unwrap();
    assert_eq!(plain.options.runtime, RuntimeKind::LegacyNode);
    assert_eq!(nested.options.runtime, RuntimeKind::LegacyNode);
    assert_eq!(nested.options.config, json!({"answer": 42}));
}

#[test]
fn web_and_plugin_resolve_the_same_explicit_data_root() {
    let temp = TempDir::new();
    let data_dir = temp.0.join("shared-data");
    let data_dir_arg = data_dir.to_str().expect("temporary path is UTF-8");

    let CliCommand::Web(web) = parse_cli(["tessivum", "web", "--data-dir", data_dir_arg])
        .expect("web invocation parses")
        .command
    else {
        panic!("expected web command");
    };
    let CliCommand::Plugin(plugin) = parse_cli([
        "tessivum",
        "plugin",
        "--data-dir",
        data_dir_arg,
        "add",
        "example-plugin",
    ])
    .expect("plugin invocation parses")
    .command
    else {
        panic!("expected plugin command");
    };

    let web_root = resolve_data_root(web.data_dir)
        .expect("web data root resolves")
        .data_dir;
    let plugin_root = resolve_data_root(plugin.data_dir)
        .expect("plugin data root resolves")
        .data_dir;
    assert_eq!(web_root, data_dir);
    assert_eq!(plugin_root, web_root);
    assert_eq!(
        plugin_profile_root(plugin_root),
        plugin_profile_root(web_root)
    );
}

fn write_package(profile: &Path, name: &str, extra: serde_json::Value) {
    let root = profile.join("node_modules").join(name);
    fs::create_dir_all(root.join("lib")).unwrap();
    let mut manifest = json!({
        "name": name,
        "version": "1.0.0",
        "type": "module",
        "main": "./lib/index.js"
    });
    manifest
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    write_json(&root.join("package.json"), manifest);
    fs::write(root.join("lib/index.js"), "export default () => {}\n").unwrap();
}

fn write_json(path: &Path, value: serde_json::Value) {
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("tessivum-plugin-profile-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::json;
use tessivum::plugin_manager::load_plugin_entries;
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

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use serde_json::json;
use tessivum::{
    cli::{parse_cli, resolve_data_root, CliCommand},
    plugin_manager::{
        enabled_client_plugin_names, load_plugin_entries, plugin_profile_root,
    },
};
use tessivum_core::RuntimeKind;

#[test]
fn plugin_profile_loads_enabled_bundle_insertions_without_plain_dependencies() {
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
    write_package(
        &profile,
        "nested-plugin",
        json!({"dsh": {"client": {"platform": "web"}}}),
    );
    fs::write(
        profile.join("node_modules/plugin-bundle/cordis.patch.yml"),
        "- insert:\n    - id: nested\n      name: nested-plugin\n      config:\n        answer: 42\n",
    )
    .unwrap();

    let tree = load_plugin_entries(&profile).unwrap().unwrap();
    let entries = tree.entries();
    assert_eq!(entries.len(), 1);
    assert!(entries
        .iter()
        .all(|entry| entry.options.name.as_deref() != Some("plain-plugin")));
    let nested = entries
        .iter()
        .find(|entry| entry.options.id.as_str() == "nested")
        .unwrap();
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

#[test]
fn absent_bundles_migrate_once_without_losing_unrelated_manifest_fields() {
    let temp = TempDir::new();
    let profile = temp.0.join("plugins");
    fs::create_dir_all(profile.join("node_modules")).unwrap();
    write_json(
        &profile.join("package.json"),
        json!({
            "name": "test-profile",
            "private": true,
            "preserved": {"answer": 42},
            "dependencies": {
                "a-bundle": "1.0.0",
                "client-only": "1.0.0",
                "plain": "1.0.0",
                "z-bundle": "1.0.0"
            }
        }),
    );
    write_bundle(&profile, "a-bundle", false, "[]\n");
    write_bundle(&profile, "z-bundle", false, "[]\n");
    write_package(
        &profile,
        "client-only",
        json!({"dsh": {"client": {"platform": "web"}}}),
    );
    write_package(&profile, "plain", json!({}));

    assert!(load_plugin_entries(&profile).unwrap().is_none());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(profile.join("package.json")).unwrap()).unwrap();
    assert_eq!(manifest["preserved"], json!({"answer": 42}));
    assert_eq!(manifest.pointer("/dsh/profile/bundles"), Some(&json!(["a-bundle", "z-bundle"])));
    assert_eq!(
        enabled_client_plugin_names(&profile).unwrap(),
        BTreeSet::from(["client-only".to_owned()])
    );
}

#[test]
fn explicit_empty_bundles_disable_host_bundles_but_keep_true_client_only_packages() {
    let temp = TempDir::new();
    let profile = temp.0.join("plugins");
    fs::create_dir_all(profile.join("node_modules")).unwrap();
    write_json(
        &profile.join("package.json"),
        json!({
            "dependencies": {"bundle-client": "1.0.0", "client-only": "1.0.0"},
            "dsh": {"profile": {"bundles": []}}
        }),
    );
    write_bundle(&profile, "bundle-client", true, "[]\n");
    write_package(
        &profile,
        "client-only",
        json!({"dsh": {"client": {"platform": "web"}}}),
    );

    assert!(load_plugin_entries(&profile).unwrap().is_none());
    assert_eq!(
        enabled_client_plugin_names(&profile).unwrap(),
        BTreeSet::from(["client-only".to_owned()])
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(profile.join("package.json")).unwrap()).unwrap();
    assert_eq!(manifest.pointer("/dsh/profile/bundles"), Some(&json!([])));
}

#[test]
fn malformed_duplicate_unknown_and_invalid_bundle_profiles_fail_loudly() {
    let malformed = TempDir::new();
    let malformed_profile = malformed.0.join("plugins");
    fs::create_dir_all(&malformed_profile).unwrap();
    write_json(
        &malformed_profile.join("package.json"),
        json!({"dependencies": {}, "dsh": {"profile": {"bundles": [42]}}}),
    );
    assert!(load_plugin_entries(&malformed_profile).is_err());

    let duplicate = TempDir::new();
    let duplicate_profile = duplicate.0.join("plugins");
    fs::create_dir_all(duplicate_profile.join("node_modules")).unwrap();
    write_json(
        &duplicate_profile.join("package.json"),
        json!({"dependencies": {"bundle": "1.0.0"}, "dsh": {"profile": {"bundles": ["bundle", "bundle"]}}}),
    );
    write_bundle(&duplicate_profile, "bundle", false, "[]\n");
    assert!(load_plugin_entries(&duplicate_profile).is_err());

    let unknown = TempDir::new();
    let unknown_profile = unknown.0.join("plugins");
    fs::create_dir_all(&unknown_profile).unwrap();
    write_json(
        &unknown_profile.join("package.json"),
        json!({"dependencies": {}, "dsh": {"profile": {"bundles": ["missing"]}}}),
    );
    assert!(load_plugin_entries(&unknown_profile).is_err());

    let missing_declaration = TempDir::new();
    let missing_profile = missing_declaration.0.join("plugins");
    fs::create_dir_all(missing_profile.join("node_modules")).unwrap();
    write_json(
        &missing_profile.join("package.json"),
        json!({"dependencies": {"plain": "1.0.0"}, "dsh": {"profile": {"bundles": ["plain"]}}}),
    );
    write_package(&missing_profile, "plain", json!({}));
    assert!(load_plugin_entries(&missing_profile).is_err());

    let escaping_patch = TempDir::new();
    let escaping_profile = escaping_patch.0.join("plugins");
    fs::create_dir_all(escaping_profile.join("node_modules")).unwrap();
    write_json(
        &escaping_profile.join("package.json"),
        json!({"dependencies": {"bundle": "1.0.0"}, "dsh": {"profile": {"bundles": ["bundle"]}}}),
    );
    write_package(
        &escaping_profile,
        "bundle",
        json!({"dsh": {"bundle": {"patch": "../escape.yml"}}}),
    );
    assert!(load_plugin_entries(&escaping_profile).is_err());

    let invalid_patch = TempDir::new();
    let invalid_profile = invalid_patch.0.join("plugins");
    fs::create_dir_all(invalid_profile.join("node_modules")).unwrap();
    write_json(
        &invalid_profile.join("package.json"),
        json!({"dependencies": {"bundle": "1.0.0"}, "dsh": {"profile": {"bundles": ["bundle"]}}}),
    );
    write_bundle(&invalid_profile, "bundle", false, "not: [valid\n");
    assert!(load_plugin_entries(&invalid_profile).is_err());
}

#[test]
fn bundle_order_controls_entries_and_duplicate_guard_avoids_a_second_active_mount() {
    let temp = TempDir::new();
    let profile = temp.0.join("plugins");
    fs::create_dir_all(profile.join("node_modules")).unwrap();
    write_json(
        &profile.join("package.json"),
        json!({
            "dependencies": {
                "bundle-one": "1.0.0",
                "bundle-two": "1.0.0",
                "inactive": "1.0.0",
                "shared": "1.0.0"
            },
            "dsh": {"profile": {"bundles": ["bundle-one", "bundle-two"]}}
        }),
    );
    write_package(&profile, "shared", json!({}));
    write_package(&profile, "inactive", json!({}));
    let guard_one = "disabled: !!js \"[...ctx.loader.entries()].some((e) => e.options.name === 'shared' && e.options.id !== 'one' && !e.disabled)\"";
    let guard_two = "disabled: !!js \"[...ctx.loader.entries()].some((e) => e.options.name === 'shared' && e.options.id !== 'two' && !e.disabled)\"";
    write_bundle(
        &profile,
        "bundle-one",
        false,
        &format!("- insert:\n    - id: one\n      name: shared\n      {guard_one}\n"),
    );
    write_bundle(
        &profile,
        "bundle-two",
        false,
        &format!("- insert:\n    - id: two\n      name: shared\n      {guard_two}\n"),
    );

    let tree = load_plugin_entries(&profile).unwrap().unwrap();
    let entries = tree.entries();
    assert_eq!(entries.iter().map(|entry| entry.options.id.as_str()).collect::<Vec<_>>(), ["one", "two"]);
    assert!(!entries[0].options.disabled);
    assert!(entries[1].options.disabled);
    assert!(entries
        .iter()
        .all(|entry| entry.options.name.as_deref() != Some("inactive")));
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

fn write_bundle(profile: &Path, name: &str, client: bool, patch: &str) {
    let mut dsh = json!({"bundle": {"patch": "./cordis.patch.yml"}});
    if client {
        dsh.as_object_mut()
            .unwrap()
            .insert("client".into(), json!({"platform": "web"}));
    }
    write_package(profile, name, json!({"dsh": dsh}));
    fs::write(
        profile.join("node_modules").join(name).join("cordis.patch.yml"),
        patch,
    )
    .unwrap();
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

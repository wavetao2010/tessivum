use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use parking_lot::{Mutex, MutexGuard};
#[cfg(unix)]
use std::{env, ffi::OsString, sync::LazyLock};

use serde_json::json;
#[cfg(unix)]
use sha2::{Digest, Sha256};
#[cfg(unix)]
use tessivum::plugin_manager::install_first_party_market;
use tessivum::{
    cli::{parse_cli, resolve_data_root, CliCommand},
    plugin_manager::{enabled_client_plugin_names, load_plugin_entries, plugin_profile_root},
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
    assert_eq!(
        manifest.pointer("/dsh/profile/bundles"),
        Some(&json!(["a-bundle", "z-bundle"]))
    );
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
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.options.id.as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
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
        profile
            .join("node_modules")
            .join(name)
            .join("cordis.patch.yml"),
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

#[cfg(unix)]
#[test]
fn first_party_market_installs_fresh_at_the_stable_artifact_path_and_is_idempotent() {
    let temp = TempDir::new();
    let (tarball, checksum, package) = market_release(&temp, valid_market_patch());
    let log = temp.0.join("pnpm.log");
    let _pnpm = FakePnpm::new(&temp, &package, false, &log);

    install_first_party_market(&temp.0, &tarball, &checksum).unwrap();
    let profile = temp.0.join("plugins");
    let artifact = stable_market_artifact(&temp.0);
    let manifest = read_json_file(&profile.join("package.json"));
    assert_eq!(
        manifest.pointer("/dependencies/tessivum-market"),
        Some(&json!(format!("file:{}", artifact.display())))
    );
    assert_eq!(
        manifest.pointer("/dsh/profile/bundles"),
        Some(&json!(["tessivum-market"]))
    );
    assert_eq!(fs::read(&artifact).unwrap(), fs::read(&tarball).unwrap());

    fs::write(&log, "").unwrap();
    install_first_party_market(&temp.0, &tarball, &checksum).unwrap();
    assert_eq!(fs::read_to_string(&log).unwrap(), "");
}

#[cfg(unix)]
#[test]
fn first_party_market_replaces_the_legacy_bundle_in_place_and_preserves_state_groups_and_disabled()
{
    let temp = TempDir::new();
    let (profile, _, _, state) =
        legacy_market_profile(&temp, json!(["before", "dshmarket", "after"]));
    let (tarball, checksum, package) = market_release(&temp, valid_market_patch());
    let _pnpm = FakePnpm::new(&temp, &package, false, &temp.0.join("pnpm.log"));

    install_first_party_market(&temp.0, &tarball, &checksum).unwrap();
    let manifest = read_json_file(&profile.join("package.json"));
    assert_eq!(
        manifest.pointer("/dsh/profile/bundles"),
        Some(&json!(["before", "tessivum-market", "after"]))
    );
    assert_eq!(
        manifest.pointer("/dsh/profile/groups/market"),
        Some(&json!({"disabled": true}))
    );
    assert!(manifest.pointer("/dependencies/dshmarket").is_none());
    assert_eq!(
        fs::read(profile.join(".dsh-market/state.json")).unwrap(),
        state
    );
}

#[cfg(unix)]
#[test]
fn first_party_market_respects_an_explicit_empty_bundle_authority() {
    let temp = TempDir::new();
    let (profile, _, _, state) = legacy_market_profile(&temp, json!([]));
    let (tarball, checksum, package) = market_release(&temp, valid_market_patch());
    let _pnpm = FakePnpm::new(&temp, &package, false, &temp.0.join("pnpm.log"));

    install_first_party_market(&temp.0, &tarball, &checksum).unwrap();
    let manifest = read_json_file(&profile.join("package.json"));
    assert_eq!(manifest.pointer("/dsh/profile/bundles"), Some(&json!([])));
    assert_eq!(
        fs::read(profile.join(".dsh-market/state.json")).unwrap(),
        state
    );
}

#[cfg(unix)]
#[test]
fn first_party_market_rejects_an_incorrect_hash_before_profile_mutation() {
    let temp = TempDir::new();
    let profile = temp.0.join("plugins");
    fs::create_dir_all(&profile).unwrap();
    let manifest = b"{\n  \"dependencies\": {}\n}\n";
    fs::write(profile.join("package.json"), manifest).unwrap();
    let (tarball, _, _) = market_release(&temp, valid_market_patch());

    assert!(install_first_party_market(&temp.0, &tarball, "00".repeat(32)).is_err());
    assert_eq!(fs::read(profile.join("package.json")).unwrap(), manifest);
    assert!(!stable_market_artifact(&temp.0).exists());
}

#[cfg(unix)]
#[test]
fn first_party_market_rejects_duplicate_legacy_identities_without_mutation() {
    let temp = TempDir::new();
    let profile = temp.0.join("plugins");
    fs::create_dir_all(&profile).unwrap();
    let manifest = json!({
        "dependencies": {"dshmarket": "file:/old-one.tgz", "dsh-market": "file:/old-two.tgz"},
        "dsh": {"profile": {"bundles": []}}
    });
    write_json(&profile.join("package.json"), manifest);
    let original = fs::read(profile.join("package.json")).unwrap();
    let (tarball, checksum, _) = market_release(&temp, valid_market_patch());

    assert!(install_first_party_market(&temp.0, &tarball, &checksum).is_err());
    assert_eq!(fs::read(profile.join("package.json")).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn first_party_market_composition_failure_restores_exact_legacy_source_documents_and_state() {
    let temp = TempDir::new();
    let (profile, manifest, lock, state) = legacy_market_profile(&temp, json!(["dshmarket"]));
    let (tarball, checksum, package) = market_release(&temp, "not: [valid\n");
    let log = temp.0.join("pnpm.log");
    let _pnpm = FakePnpm::new(&temp, &package, false, &log);

    assert!(install_first_party_market(&temp.0, &tarball, &checksum).is_err());
    assert_eq!(fs::read(profile.join("package.json")).unwrap(), manifest);
    assert_eq!(fs::read(profile.join("pnpm-lock.yaml")).unwrap(), lock);
    assert_eq!(
        fs::read(profile.join(".dsh-market/state.json")).unwrap(),
        state
    );
    let restored = read_json_file(&profile.join("package.json"));
    assert_eq!(
        restored.pointer("/dependencies/dshmarket"),
        Some(&json!("file:/previous/dshmarket-1.29.2.tgz"))
    );
    assert_eq!(fs::read_to_string(&log).unwrap(), "add\ninstall\n");
}

#[cfg(unix)]
#[test]
fn first_party_market_pnpm_failure_rolls_back_the_legacy_profile() {
    let temp = TempDir::new();
    let (profile, manifest, lock, state) = legacy_market_profile(&temp, json!(["dshmarket"]));
    let (tarball, checksum, package) = market_release(&temp, valid_market_patch());
    let log = temp.0.join("pnpm.log");
    let _pnpm = FakePnpm::new(&temp, &package, true, &log);

    assert!(install_first_party_market(&temp.0, &tarball, &checksum).is_err());
    assert_eq!(fs::read(profile.join("package.json")).unwrap(), manifest);
    assert_eq!(fs::read(profile.join("pnpm-lock.yaml")).unwrap(), lock);
    assert_eq!(
        fs::read(profile.join(".dsh-market/state.json")).unwrap(),
        state
    );
    assert_eq!(fs::read_to_string(&log).unwrap(), "add\ninstall\n");
}

#[cfg(unix)]
fn valid_market_patch() -> &'static str {
    "- insert:\n    - id: tessivum-market\n      name: tessivum-market\n"
}

#[cfg(unix)]
fn market_release(temp: &TempDir, patch: &str) -> (PathBuf, String, PathBuf) {
    let tarball = temp.0.join("release.tgz");
    let bytes = b"tessivum market release";
    fs::write(&tarball, bytes).unwrap();
    let package = temp.0.join("market-package");
    fs::create_dir_all(package.join("lib")).unwrap();
    write_json(
        &package.join("package.json"),
        json!({
            "name": "tessivum-market",
            "version": "0.1.0-alpha.18",
            "type": "module",
            "main": "./lib/index.js",
            "dsh": {"bundle": {"patch": "./cordis.patch.yml"}}
        }),
    );
    fs::write(package.join("lib/index.js"), "export default () => {}\n").unwrap();
    fs::write(package.join("cordis.patch.yml"), patch).unwrap();
    (tarball, format!("{:x}", Sha256::digest(bytes)), package)
}

#[cfg(unix)]
fn stable_market_artifact(data_dir: &Path) -> PathBuf {
    data_dir.join("artifacts/market/0.1.0-alpha.18/tessivum-market-0.1.0-alpha.18.tgz")
}

#[cfg(unix)]
fn legacy_market_profile(
    temp: &TempDir,
    bundles: serde_json::Value,
) -> (PathBuf, Vec<u8>, Vec<u8>, Vec<u8>) {
    let profile = temp.0.join("plugins");
    fs::create_dir_all(profile.join("node_modules")).unwrap();
    write_json(
        &profile.join("package.json"),
        json!({
            "name": "profile",
            "dependencies": {
                "before": "1.0.0",
                "dshmarket": "file:/previous/dshmarket-1.29.2.tgz",
                "after": "1.0.0"
            },
            "dsh": {"profile": {"bundles": bundles, "groups": {"market": {"disabled": true}}}}
        }),
    );
    write_bundle(&profile, "before", false, "[]\n");
    write_bundle(
        &profile,
        "dshmarket",
        false,
        "- insert:\n    - id: old-market\n      name: dshmarket\n",
    );
    write_bundle(&profile, "after", false, "[]\n");
    let lock = b"lockfileVersion: '9.0'\n".to_vec();
    fs::write(profile.join("pnpm-lock.yaml"), &lock).unwrap();
    let state = br#"{"disabled":["dshmarket"],"groups":{"dshmarket":"market"}}"#.to_vec();
    fs::create_dir_all(profile.join(".dsh-market")).unwrap();
    fs::write(profile.join(".dsh-market/state.json"), &state).unwrap();
    let manifest = fs::read(profile.join("package.json")).unwrap();
    (profile, manifest, lock, state)
}

#[cfg(unix)]
fn read_json_file(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[cfg(unix)]
struct FakePnpm {
    _gate: MutexGuard<'static, ()>,
    path: Option<OsString>,
    market: Option<OsString>,
    fail_add: Option<OsString>,
    log: Option<OsString>,
}

#[cfg(unix)]
impl FakePnpm {
    fn new(temp: &TempDir, market: &Path, fail_add: bool, log: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;

        static GATE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        let gate = GATE.lock();
        let bin = temp.0.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let pnpm = bin.join("pnpm");
        // pnpm 11 rejects the add/install-only --ignore-scripts flag on remove.
        fs::write(
            &pnpm,
            "#!/bin/sh\nif [ -n \"$FAKE_PNPM_LOG\" ]; then echo \"$1\" >> \"$FAKE_PNPM_LOG\"; fi\ncase \"$1\" in\n  add)\n    mkdir -p \"$PWD/node_modules\"\n    rm -rf \"$PWD/node_modules/tessivum-market\"\n    cp -R \"$FAKE_PNPM_MARKET\" \"$PWD/node_modules/tessivum-market\"\n    [ \"$FAKE_PNPM_FAIL_ADD\" = 1 ] && exit 23\n    ;;\n  remove) case \" $* \" in *\" --ignore-scripts \"*) exit 24;; esac; rm -rf \"$PWD/node_modules/$2\" ;;\n  install) rm -rf \"$PWD/node_modules/tessivum-market\" ;;\nesac\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&pnpm, fs::Permissions::from_mode(0o755)).unwrap();
        let path = env::var_os("PATH");
        let mut replacement = OsString::from(bin);
        if let Some(previous) = &path {
            replacement.push(":");
            replacement.push(previous);
        }
        let market_previous = env::var_os("FAKE_PNPM_MARKET");
        let fail_add_previous = env::var_os("FAKE_PNPM_FAIL_ADD");
        let log_previous = env::var_os("FAKE_PNPM_LOG");
        env::set_var("PATH", replacement);
        env::set_var("FAKE_PNPM_MARKET", market);
        env::set_var("FAKE_PNPM_FAIL_ADD", if fail_add { "1" } else { "0" });
        env::set_var("FAKE_PNPM_LOG", log);
        Self {
            _gate: gate,
            path,
            market: market_previous,
            fail_add: fail_add_previous,
            log: log_previous,
        }
    }
}

#[cfg(unix)]
impl Drop for FakePnpm {
    fn drop(&mut self) {
        restore_environment("PATH", self.path.take());
        restore_environment("FAKE_PNPM_MARKET", self.market.take());
        restore_environment("FAKE_PNPM_FAIL_ADD", self.fail_add.take());
        restore_environment("FAKE_PNPM_LOG", self.log.take());
    }
}

#[cfg(unix)]
fn restore_environment(name: &str, value: Option<OsString>) {
    if let Some(value) = value {
        env::set_var(name, value);
    } else {
        env::remove_var(name);
    }
}

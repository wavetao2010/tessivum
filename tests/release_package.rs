use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const TAG: &str = "v0.1.0-alpha.11";
const TARGET: &str = "x86_64-unknown-linux-gnu";
const DSH_SETTINGS_ENTRIES: &[&str] = &["lib/index.js"];
const SCHEMASTRY_ENTRIES: &[&str] = &["lib/index.mjs", "lib/index.cjs"];

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "tessivum-release-package-{}-{nonce}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    _temp: TempDir,
    binary: PathBuf,
    compat_host: PathBuf,
    host_modules: PathBuf,
    vendor: PathBuf,
    agent_presets: PathBuf,
    output: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new();
        let root = temp.path();
        let binary = root.join("bin/tessivum");
        write(
            &binary,
            "#!/usr/bin/env sh\ncase \"${1:-}\" in\n  --version) printf 'tessivum 0.1.0-alpha.11\\n' ;;\n  --host-module-root) printf '%s\\n' \"$TESSIVUM_HOST_MODULE_ROOT\" ;;\nesac\n",
        );
        make_executable(&binary);

        let compat_host = root.join("compat-host");
        write(compat_host.join("package.json"), "{}\n");
        write(compat_host.join("bun.lock"), "{}\n");
        write(compat_host.join("src/index.ts"), "export {}\n");

        let vendor = root.join("vendor");
        for package in ["cordis", "cosmokit", "loader"] {
            write(vendor.join(package).join("lib/index.js"), "export {}\n");
        }

        let agent_presets = root.join("deepseek/apps/cli/config/agent-presets");
        for preset in ["standard", "code", "minimal", "cordis"] {
            write(
                agent_presets.join(preset).join("agent.cordis.yml"),
                "plugins: []\n",
            );
            write(
                agent_presets.join(preset).join("preset.yml"),
                "name: fixture\n",
            );
        }
        write(root.join("deepseek/LICENSE"), "MIT fixture license\n");

        let host_modules = root.join("host-modules");
        for (name, version, runtime_entries) in [
            ("dsh-settings", "0.1.0-rc.7", DSH_SETTINGS_ENTRIES),
            ("schemastery", "3.18.1", SCHEMASTRY_ENTRIES),
        ] {
            write_module(&host_modules, name, version, runtime_entries);
        }
        write_inventory(&host_modules);

        let output = root.join("dist");
        fs::create_dir_all(&output).unwrap();
        Self {
            _temp: temp,
            binary,
            compat_host,
            host_modules,
            vendor,
            agent_presets,
            output,
        }
    }

    fn package(&self) -> Output {
        Command::new("bash")
            .arg(repository_root().join("scripts/package_release.sh"))
            .arg(TAG)
            .arg(TARGET)
            .arg(&self.binary)
            .arg(&self.compat_host)
            .arg(&self.host_modules)
            .arg(&self.vendor)
            .arg(&self.agent_presets)
            .arg(&self.output)
            .current_dir(repository_root())
            .output()
            .unwrap()
    }

    fn archive(&self) -> PathBuf {
        self.output
            .join("tessivum-0.1.0-alpha.11-x86_64-unknown-linux-gnu.tar.gz")
    }

    fn stage(&self) -> PathBuf {
        self.output
            .join("tessivum-0.1.0-alpha.11-x86_64-unknown-linux-gnu")
    }
}

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_module(root: &Path, name: &str, version: &str, runtime_entries: &[&str]) {
    let module = root.join("@deepseek-ai").join(name);
    write(
        module.join("package.json"),
        serde_json::to_vec(&json!({
            "name": format!("@deepseek-ai/{name}"),
            "version": version,
            "license": "MIT",
        }))
        .unwrap(),
    );
    for entry in runtime_entries {
        write(
            module.join(*entry),
            format!("export const fixture = {name:?};\n"),
        );
    }
    write(module.join("LICENSE"), "MIT fixture license\n");
}

fn sha256(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

fn inventory_row(
    root: &Path,
    name: &str,
    version: &str,
    url: &str,
    sri: &str,
    runtime_entries: &[&str],
) -> Value {
    let module = root.join("@deepseek-ai").join(name);
    let mut paths = vec!["LICENSE", "package.json"];
    paths.extend_from_slice(runtime_entries);
    paths.sort_unstable();
    let files = paths
        .into_iter()
        .map(|path| json!({"path": path, "sha256": sha256(&module.join(path))}))
        .collect::<Vec<_>>();
    json!({
        "name": format!("@deepseek-ai/{name}"),
        "version": version,
        "url": url,
        "sri": sri,
        "license": "MIT",
        "licenseFile": "LICENSE",
        "runtimeEntries": runtime_entries,
        "files": files,
    })
}

fn write_inventory(root: &Path) {
    write(
        root.join("INVENTORY.json"),
        serde_json::to_vec_pretty(&json!({
            "format": 1,
            "packages": [
                inventory_row(
                    root,
                    "dsh-settings",
                    "0.1.0-rc.7",
                    "https://registry.npmjs.org/@deepseek-ai/dsh-settings/-/dsh-settings-0.1.0-rc.7.tgz",
                    "sha512-cS1+StK2xIVykrskw+KrO+hKJxYaVVgY2Jex2we5zASTNLHYCj74WQluPlaFjFgnXoZ1RYz8HNXL5/Ho6h1Ynw==",
                    DSH_SETTINGS_ENTRIES,
                ),
                inventory_row(
                    root,
                    "schemastery",
                    "3.18.1",
                    "https://registry.npmjs.org/@deepseek-ai/schemastery/-/schemastery-3.18.1.tgz",
                    "sha512-Qn0FCSwCQnpnj6SB31I6i2sIKgKWnkbJM8O0EU91Gv2UsYVvtZTl6IA0sCwk2e2MZf5S8w5hpq9QkeVvK9qwxg==",
                    SCHEMASTRY_ENTRIES,
                ),
            ],
        }))
        .unwrap(),
    );
}

fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "package_release.sh failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn release_archive_contains_the_verified_host_modules_and_licenses() {
    let fixture = Fixture::new();
    assert_success(fixture.package());

    let listing = Command::new("tar")
        .args(["-tzf"])
        .arg(fixture.archive())
        .output()
        .unwrap();
    assert!(listing.status.success());
    let listing = String::from_utf8(listing.stdout).unwrap();
    for path in [
        "share/tessivum/host-modules/INVENTORY.json",
        "share/tessivum/host-modules/@deepseek-ai/dsh-settings/package.json",
        "share/tessivum/host-modules/@deepseek-ai/dsh-settings/lib/index.js",
        "share/tessivum/host-modules/@deepseek-ai/dsh-settings/LICENSE",
        "share/tessivum/host-modules/@deepseek-ai/schemastery/lib/index.cjs",
        "share/tessivum/host-modules/@deepseek-ai/schemastery/lib/index.mjs",
        "share/tessivum/host-modules/@deepseek-ai/schemastery/LICENSE",
        "share/tessivum/host-modules/node_modules/@deepseek-ai/cordis",
        "share/tessivum/host-modules/node_modules/@deepseek-ai/cosmokit",
        "share/licenses/@deepseek-ai-dsh-settings-0.1.0-rc.7/LICENSE",
        "share/licenses/@deepseek-ai-schemastery-3.18.1/LICENSE",
    ] {
        assert!(
            listing.lines().any(|entry| entry.ends_with(path)),
            "archive is missing {path}"
        );
    }

    let launcher = fixture.stage().join("bin/tessivum");
    let launcher_text = fs::read_to_string(&launcher).unwrap();
    assert!(launcher_text.contains("TESSIVUM_HOST_MODULE_ROOT"));
    assert!(launcher_text.contains("$root/share/tessivum/host-modules"));
    let host_modules = fixture.stage().join("share/tessivum/host-modules");
    let launcher_env = Command::new(&launcher)
        .arg("--host-module-root")
        .output()
        .unwrap();
    assert!(launcher_env.status.success());
    assert_eq!(
        String::from_utf8(launcher_env.stdout).unwrap(),
        format!("{}\n", host_modules.display()),
    );
    for (package, target) in [
        ("cordis", "../../../vendor/cordis"),
        ("cosmokit", "../../../vendor/cosmokit"),
    ] {
        let link = host_modules.join("node_modules/@deepseek-ai").join(package);
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from(target));
        assert_eq!(
            link.canonicalize().unwrap(),
            fixture
                .stage()
                .join("share/tessivum/vendor")
                .join(package)
                .canonicalize()
                .unwrap(),
        );
    }
}

fn corrupt_file_hash(fixture: &Fixture) {
    write(
        fixture
            .host_modules
            .join("@deepseek-ai/dsh-settings/lib/index.js"),
        "tampered\n",
    );
}

fn corrupt_package_version(fixture: &Fixture) {
    write(
        fixture
            .host_modules
            .join("@deepseek-ai/dsh-settings/package.json"),
        serde_json::to_vec(&json!({
            "name": "@deepseek-ai/dsh-settings",
            "version": "0.0.0",
            "license": "MIT",
            "main": "lib/index.js",
        }))
        .unwrap(),
    );
}

fn corrupt_inventory_path(fixture: &Fixture) {
    let path = fixture.host_modules.join("INVENTORY.json");
    let mut inventory: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    inventory["packages"][0]["files"][0]["path"] = json!("missing.js");
    write(path, serde_json::to_vec_pretty(&inventory).unwrap());
}

#[test]
fn release_package_rejects_invalid_host_module_provenance_before_archiving() {
    for corrupt in [
        corrupt_file_hash as fn(&Fixture),
        corrupt_package_version,
        corrupt_inventory_path,
    ] {
        let fixture = Fixture::new();
        corrupt(&fixture);
        let output = fixture.package();
        assert!(
            !output.status.success(),
            "package_release.sh accepted invalid host modules:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            !fixture.archive().exists(),
            "invalid host modules produced an archive: {}",
            fixture.archive().display(),
        );
    }
}

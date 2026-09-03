use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const TAG: &str = "v0.1.0-alpha.23";
const TARGET: &str = "x86_64-unknown-linux-gnu";
const MARKET_VERSION: &str = "0.1.0-alpha.23";
const MARKET_FILENAME: &str = "tessivum-market-0.1.0-alpha.23.tgz";
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
    market_root: PathBuf,
    market_tgz: PathBuf,
    output: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new();
        let root = temp.path();
        let binary = root.join("bin/tessivum");
        write(
            &binary,
            "#!/usr/bin/env sh\ncase \"${1:-}\" in\n  --version) printf 'tessivum 0.1.0-alpha.23\\n' ;;\n  --help) printf 'Tessivum fixture help\\n' ;;\n  --host-module-root) printf '%s\\n' \"$TESSIVUM_HOST_MODULE_ROOT\" ;;\n  --market-tarball) printf '%s\\n' \"$TESSIVUM_MARKET_TARBALL\" ;;\n  --market-sha256-file) printf '%s\\n' \"$TESSIVUM_MARKET_SHA256_FILE\" ;;\n  --market-source-file) printf '%s\\n' \"$TESSIVUM_MARKET_SOURCE_FILE\" ;;\nesac\n",
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

        write(root.join("LICENSE"), "MIT fixture license\n");

        let host_modules = root.join("host-modules");
        for (name, version, runtime_entries) in [
            ("dsh-settings", "0.1.0-rc.7", DSH_SETTINGS_ENTRIES),
            ("schemastery", "3.18.1", SCHEMASTRY_ENTRIES),
        ] {
            write_module(&host_modules, name, version, runtime_entries);
        }
        write_inventory(&host_modules);

        let market_root = root.join("market");
        let market_tgz = root.join(MARKET_FILENAME);
        write_market_tgz(
            &market_root,
            &market_tgz,
            MARKET_VERSION,
            market_provenance(),
        );

        let output = root.join("dist");
        fs::create_dir_all(&output).unwrap();
        Self {
            _temp: temp,
            binary,
            compat_host,
            host_modules,
            vendor,
            market_root,
            market_tgz,
            output,
        }
    }

    fn package(&self) -> Output {
        self.package_with_market(&self.market_tgz)
    }

    fn package_with_market(&self, market_tgz: &Path) -> Output {
        Command::new("bash")
            .arg(repository_root().join("scripts/package_release.sh"))
            .arg(TAG)
            .arg(TARGET)
            .arg(&self.binary)
            .arg(&self.compat_host)
            .arg(&self.host_modules)
            .arg(&self.vendor)
            .arg(market_tgz)
            .arg(&self.output)
            .current_dir(repository_root())
            .output()
            .unwrap()
    }

    fn rewrite_market(&self, version: &str, provenance: Value) {
        write_market_tgz(&self.market_root, &self.market_tgz, version, provenance);
    }

    fn archive(&self) -> PathBuf {
        self.output
            .join("tessivum-0.1.0-alpha.23-x86_64-unknown-linux-gnu.tar.gz")
    }

    fn stage(&self) -> PathBuf {
        self.output
            .join("tessivum-0.1.0-alpha.23-x86_64-unknown-linux-gnu")
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

fn market_provenance() -> Value {
    json!({
        "repository": "https://github.com/dsh-market/dsh-market",
        "version": "1.38.1",
        "commit": "df2a16b1ed2dfaf1f2505e184e738c0d6d428945",
        "tarballIntegrity": "sha512-Z9VleLtCXwk5OlbSJKayWtbMaKACL8JUMyb/JHpErS4N3q//GJS+cgOhhxNkZYmXxB8/lv9IbhX1CBzlMhJeJg==",
        "license": "MIT",
    })
}

fn write_market_tgz(root: &Path, tarball: &Path, version: &str, provenance: Value) {
    let package = root.join("package");
    let upstream = provenance.clone();
    let _ = fs::remove_dir_all(root);
    write(
        package.join("package.json"),
        serde_json::to_vec(&json!({
            "name": "tessivum-market",
            "version": version,
            "license": "MIT",
            "tessivum": {"provenance": provenance},
        }))
        .unwrap(),
    );
    write(
        package.join("UPSTREAM.json"),
        serde_json::to_vec(&upstream).unwrap(),
    );
    write(
        package.join("LICENSE.upstream"),
        fs::read(repository_root().join("packaging/licenses/dsh-market/LICENSE")).unwrap(),
    );
    let output = Command::new("tar")
        .arg("-C")
        .arg(root)
        .args(["-czf"])
        .arg(tarball)
        .arg("package")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture market archive failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    write(
        PathBuf::from(format!("{}.sha256", tarball.display())),
        format!("{}  {MARKET_FILENAME}\n", sha256(tarball)),
    );
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
fn release_archive_contains_compatibility_assets_without_legacy_preset_assets() {
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
        "bin/tessivum",
        "bin/tsv",
        "libexec/tessivum",
        "share/tessivum/plugins/tessivum-market-0.1.0-alpha.23.tgz",
        "share/tessivum/plugins/tessivum-market-0.1.0-alpha.23.tgz.sha256",
        "share/tessivum/plugins/tessivum-market-0.1.0-alpha.23.tgz.source.json",
        "share/licenses/tessivum-market-0.1.0-alpha.23/LICENSE",
        "share/tessivum/host-modules/INVENTORY.json",
        "share/tessivum/host-modules/@deepseek-ai/dsh-settings/package.json",
        "share/tessivum/host-modules/@deepseek-ai/dsh-settings/lib/index.js",
        "share/tessivum/host-modules/@deepseek-ai/dsh-settings/LICENSE",
        "share/tessivum/host-modules/@deepseek-ai/schemastery/lib/index.cjs",
        "share/tessivum/host-modules/@deepseek-ai/schemastery/lib/index.mjs",
        "share/tessivum/host-modules/@deepseek-ai/schemastery/LICENSE",
        "share/tessivum/host-modules/@deepseek-ai/dsh-tools/index.js",
        "share/tessivum/host-modules/@deepseek-ai/dsh-llm/index.js",
        "share/tessivum/host-modules/@deepseek-ai/dsh-subagent/descriptor.js",
        "share/tessivum/host-modules/node_modules/@deepseek-ai/cordis",
        "share/tessivum/host-modules/node_modules/@deepseek-ai/cosmokit",
        "share/licenses/@deepseek-ai-dsh-settings-0.1.0-rc.7/LICENSE",
        "share/licenses/@deepseek-ai-schemastery-3.18.1/LICENSE",
        "share/licenses/deepseek-harness/LICENSE",
    ] {
        assert!(
            listing.lines().any(|entry| entry.ends_with(path)),
            "archive is missing {path}"
        );
    }
    for obsolete in [
        ".agent-presets",
        "agent-presets",
        "agent.cordis.yml",
        "preset.yml",
    ] {
        assert!(
            !listing.lines().any(|entry| entry.contains(obsolete)),
            "archive still contains legacy preset asset {obsolete}"
        );
    }
    assert!(
        !listing.lines().any(|entry| entry.ends_with("bin/dsh")),
        "archive contains obsolete dsh launcher"
    );

    let launcher = fixture.stage().join("bin/tessivum");
    let alias = fixture.stage().join("bin/tsv");
    assert!(fs::symlink_metadata(&alias)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(&alias).unwrap(), PathBuf::from("tessivum"));
    assert_eq!(
        alias.canonicalize().unwrap(),
        launcher.canonicalize().unwrap()
    );
    assert!(fs::symlink_metadata(fixture.stage().join("bin/dsh")).is_err());
    let mut regular_launchers = fs::read_dir(fixture.stage().join("bin"))
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    regular_launchers.sort();
    assert_eq!(regular_launchers, vec![launcher.clone()]);
    let payload = fixture.stage().join("libexec/tessivum");
    let mut executable_payloads = fs::read_dir(fixture.stage().join("libexec"))
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    executable_payloads.sort();
    assert_eq!(executable_payloads, vec![payload.clone()]);
    assert_eq!(
        fs::read(&payload).unwrap(),
        fs::read(&fixture.binary).unwrap()
    );
    let market_tgz = fixture
        .stage()
        .join("share/tessivum/plugins/tessivum-market-0.1.0-alpha.23.tgz");
    let market_checksum = PathBuf::from(format!("{}.sha256", market_tgz.display()));
    let market_source = PathBuf::from(format!("{}.source.json", market_tgz.display()));
    assert_eq!(
        fs::read(&market_tgz).unwrap(),
        fs::read(&fixture.market_tgz).unwrap()
    );
    assert_eq!(
        fs::read(&market_checksum).unwrap(),
        fs::read(PathBuf::from(format!(
            "{}.sha256",
            fixture.market_tgz.display()
        )))
        .unwrap(),
    );
    assert_eq!(
        fs::read(
            fixture
                .stage()
                .join("share/tessivum/plugins/tessivum-market-0.1.0-alpha.23.tgz.source.json",)
        )
        .unwrap(),
        fs::read(repository_root().join("packaging/market-source.json")).unwrap(),
    );
    assert_eq!(
        fs::read(
            fixture
                .stage()
                .join("share/licenses/tessivum-market-0.1.0-alpha.23/LICENSE"),
        )
        .unwrap(),
        fs::read(repository_root().join("packaging/licenses/dsh-market/LICENSE")).unwrap(),
    );
    let launcher_text = fs::read_to_string(&launcher).unwrap();
    assert!(launcher_text.contains("TESSIVUM_HOST_MODULE_ROOT"));
    assert!(launcher_text.contains("$root/share/tessivum/host-modules"));
    assert!(launcher_text.contains("TESSIVUM_MARKET_TARBALL"));
    assert!(
        launcher_text.contains("$root/share/tessivum/plugins/tessivum-market-0.1.0-alpha.23.tgz")
    );
    assert!(launcher_text.contains("TESSIVUM_MARKET_SHA256_FILE"));
    assert!(launcher_text.contains("tessivum-market-0.1.0-alpha.23.tgz.sha256"));
    assert!(launcher_text.contains("TESSIVUM_MARKET_SOURCE_FILE"));
    assert!(launcher_text.contains("tessivum-market-0.1.0-alpha.23.tgz.source.json"));
    assert!(!launcher_text.contains("TESSIVUM_AGENT_PRESET_ROOT"));
    assert!(!launcher_text.contains("agent-presets"));
    for argument in ["--version", "--help"] {
        let launcher_output = Command::new(&launcher).arg(argument).output().unwrap();
        let alias_output = Command::new(&alias).arg(argument).output().unwrap();
        assert!(
            launcher_output.status.success(),
            "launcher failed for {argument}"
        );
        assert!(alias_output.status.success(), "alias failed for {argument}");
        assert_eq!(
            launcher_output.stdout, alias_output.stdout,
            "stdout differs for {argument}"
        );
        assert_eq!(
            launcher_output.stderr, alias_output.stderr,
            "stderr differs for {argument}"
        );
    }
    let readme = fs::read_to_string(fixture.stage().join("README.txt")).unwrap();
    assert!(readme.contains("Native modes are built into Tessivum"));
    assert!(readme.contains("under modes/"));
    let host_modules = fixture.stage().join("share/tessivum/host-modules");
    for command in [&launcher, &alias] {
        for (argument, expected) in [
            ("--host-module-root", &host_modules),
            ("--market-tarball", &market_tgz),
            ("--market-sha256-file", &market_checksum),
            ("--market-source-file", &market_source),
        ] {
            let launcher_env = Command::new(command).arg(argument).output().unwrap();
            assert!(launcher_env.status.success());
            assert_eq!(
                String::from_utf8(launcher_env.stdout).unwrap(),
                format!("{}\n", expected.display()),
            );
        }
    }
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

fn corrupt_market_hash(fixture: &Fixture) {
    write(&fixture.market_tgz, "tampered\n");
}

fn corrupt_market_version(fixture: &Fixture) {
    fixture.rewrite_market("0.0.0", market_provenance());
}

fn corrupt_market_source(fixture: &Fixture) {
    let mut provenance = market_provenance();
    provenance["commit"] = json!("0000000000000000000000000000000000000000");
    fixture.rewrite_market(MARKET_VERSION, provenance);
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

#[test]
fn release_package_rejects_tampered_market_provenance_before_archiving() {
    for corrupt in [
        corrupt_market_hash as fn(&Fixture),
        corrupt_market_version,
        corrupt_market_source,
    ] {
        let fixture = Fixture::new();
        corrupt(&fixture);
        let output = fixture.package();
        assert!(
            !output.status.success(),
            "package_release.sh accepted invalid market package:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            !fixture.archive().exists(),
            "invalid market package produced an archive: {}",
            fixture.archive().display(),
        );
    }
}

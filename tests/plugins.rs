use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use tessivum::plugins::{
    CompatibilityClass, PluginInspectionLimits, PluginRouter, PluginRuntime,
    DEFAULT_MAX_MANIFEST_BYTES, DEFAULT_MAX_RECURSIVE_DEPTH, DEFAULT_MAX_SOURCE_BYTES,
};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tessivum-plugin-routing-{name}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, contents: &str) {
        let path = self.0.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn sparse(&self, name: &str, bytes: u64) {
        let path = self.0.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::File::create(path).unwrap().set_len(bytes).unwrap();
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn package_json(name: &str, additions: &str) -> String {
    format!(r#"{{"name":"{name}","version":"1.2.3","license":"MIT"{additions}}}"#)
}

#[test]
fn route_priority_is_explicit_then_versioned_then_wasm_then_npm() {
    let router = PluginRouter::new();
    let declared = Fixture::new("declared");
    declared.write(
        "package.json",
        &package_json(
            "declared",
            r#", "cordis":{"plugin":{"schemaVersion":"cordis.plugin/v1","runtime":"wasm","entry":"plugin.wasm"}}"#,
        ),
    );
    declared.write("plugin.wasm", "not executed");
    declared.write("index.js", "export const apply = () => 1");

    let explicit = router
        .inspect(declared.path(), Some(PluginRuntime::LegacyNode))
        .unwrap();
    assert_eq!(explicit.selected_runtime, PluginRuntime::LegacyNode);
    assert_eq!(explicit.selected_rule, "explicit-entry-runtime");

    let manifest = router.inspect(declared.path(), None).unwrap();
    assert_eq!(manifest.selected_runtime, PluginRuntime::Wasm);
    assert!(manifest
        .selected_rule
        .starts_with("versioned-runtime-declaration:"));

    let wasm = Fixture::new("wasm");
    wasm.write("only.wasm", "not executed");
    let wasm = router.inspect(wasm.path(), None).unwrap();
    assert_eq!(wasm.selected_runtime, PluginRuntime::Wasm);
    assert!(wasm.selected_rule.starts_with("wasm-artifact:"));

    let npm = Fixture::new("npm");
    npm.write("package.json", &package_json("npm-plugin", ""));
    npm.write("index.js", "export const apply = () => 1");
    let npm = router.inspect(npm.path(), None).unwrap();
    assert_eq!(npm.selected_runtime, PluginRuntime::LegacyNode);
    assert_eq!(npm.selected_rule, "npm-cordis-default-legacy-node");
}

#[test]
fn malformed_and_conflicting_manifests_are_diagnostics_not_fallbacks() {
    let router = PluginRouter::new();
    let malformed = Fixture::new("malformed");
    malformed.write("plugin.yaml", "runtime: wasm\n");
    let error = router.inspect(malformed.path(), None).unwrap_err();
    assert_eq!(error.diagnostic().code, "PLUGIN_MANIFEST_INVALID");

    let ambiguous = Fixture::new("ambiguous");
    ambiguous.write(
        "package.json",
        &package_json(
            "ambiguous",
            r#", "cordis":{"plugin":{"schemaVersion":"cordis.plugin/v1","runtime":"wasm","entry":"x.wasm"}}, "tessivum":{"plugin":{"schemaVersion":"cordis.plugin/v1","runtime":"legacy-node"}}"#,
        ),
    );
    ambiguous.write("x.wasm", "not executed");
    let error = router.inspect(ambiguous.path(), None).unwrap_err();
    assert_eq!(error.diagnostic().code, "PLUGIN_RUNTIME_AMBIGUOUS");
}

#[test]
fn explicit_runtime_never_silently_falls_back() {
    let router = PluginRouter::new();
    let package = Fixture::new("explicit-validation");
    package.write("package.json", &package_json("legacy", ""));
    let error = router
        .inspect(package.path(), Some(PluginRuntime::Wasm))
        .unwrap_err();
    assert_eq!(error.diagnostic().code, "PLUGIN_RUNTIME_INVALID");
    assert!(error.to_string().contains("not allowed to fall back"));
}

#[test]
fn browser_package_routes_to_client_host_and_redacts_client_metadata() {
    let router = PluginRouter::new();
    let package = Fixture::new("browser");
    package.write(
        "package.json",
        &package_json(
            "browser-plugin",
            r#", "dsh":{"client":{"platform":"web","inject":["logger"],"immediately":true,"entry":{"url":"dist/client.js","hash":"sha256-safe"},"apiToken":"do-not-report"}}"#,
        ),
    );
    package.write(
        "index.js",
        "export const apply = (ctx) => window.document.title",
    );
    let report = router.inspect(package.path(), None).unwrap();
    assert_eq!(report.selected_runtime, PluginRuntime::Browser);
    assert_eq!(report.compatibility, CompatibilityClass::Browser);
    let client = report.dsh_client.unwrap();
    assert_eq!(client["platform"], "web");
    assert_eq!(client["inject"], serde_json::json!(["logger"]));
    assert_eq!(client["entry"]["url"], "dist/client.js");
    assert_eq!(client["entry"]["hash"], "sha256-safe");
    assert!(client.get("apiToken").is_none());
    assert!(report
        .static_markers
        .dom_markers
        .contains(&"window.".into()));
}

#[test]
fn static_scan_reports_node_native_dom_and_service_proxy_requirements() {
    let router = PluginRouter::new();
    let legacy = Fixture::new("legacy");
    legacy.write(
        "package.json",
        &package_json(
            "legacy-plugin",
            r#", "main":"lib/index.js", "dependencies":{"bindings":"^1.5.0"}"#,
        ),
    );
    legacy.write(
        "lib/index.js",
        r#"
          import fs from 'node:fs'
          export const inject = ['tools', 'logger', 'timer']
          export function apply(ctx) { ctx.provide('example'); ctx.on('ready', () => ctx.tools.register({})) }
          export default class Plugin extends Service {}
          require('bindings')
        "#,
    );
    let route = router.resolve(legacy.path(), None).unwrap();
    assert_eq!(
        route.artifact,
        Some(fs::canonicalize(legacy.path().join("lib/index.js")).unwrap())
    );
    let report = router.inspect(legacy.path(), None).unwrap();
    assert_eq!(report.compatibility, CompatibilityClass::NeedsProxy);
    assert_eq!(
        report.stable_cross_runtime_services,
        vec!["logger@1", "timers@1", "tools@1"]
    );
    assert_eq!(report.static_markers.node_builtins, vec!["fs"]);
    assert!(report
        .static_markers
        .native_addon_markers
        .contains(&"bindings".into()));
    assert_eq!(report.inject, vec!["logger", "timer", "tools"]);
    assert_eq!(report.provide, vec!["example"]);
    assert_eq!(report.events, vec!["ready"]);
    assert!(report.exports.apply && report.exports.class && report.exports.service);

    let dom = Fixture::new("unsupported-dom");
    dom.write("package.json", &package_json("dom-server-plugin", ""));
    dom.write(
        "index.js",
        "export function apply() { return document.createElement('div') }",
    );
    let report = router.inspect(dom.path(), None).unwrap();
    assert_eq!(report.compatibility, CompatibilityClass::Unsupported);
    assert!(report
        .reasons
        .iter()
        .any(|reason| reason.contains("dsh.client")));
}

#[test]
fn report_is_deterministic_and_never_emits_source_or_client_secrets() {
    let router = PluginRouter::new();
    let package = Fixture::new("deterministic");
    package.write(
        "package.json",
        &package_json(
            "safe-report",
            r#", "dsh":{"client":{"platform":"web","entry":"dist/client.js","config":{"password":"super-secret"},"accessKey":"access-secret","privateKey":"private-secret","api-key":"api-secret"}}"#,
        ),
    );
    package.write(
        "index.js",
        "const sourceSecret = 'super-secret'; export const apply = () => 1",
    );

    let package_debug = format!("{:?}", router.package(package.path()).unwrap());
    let first = serde_json::to_string(&router.inspect(package.path(), None).unwrap()).unwrap();
    let second = serde_json::to_string(&router.inspect(package.path(), None).unwrap()).unwrap();
    assert_eq!(first, second);
    for secret in [
        "super-secret",
        "accessKey",
        "access-secret",
        "privateKey",
        "private-secret",
        "api-key",
        "api-secret",
    ] {
        assert!(!package_debug.contains(secret));
        assert!(!first.contains(secret));
    }
    assert!(first.contains("advisory only"));
    assert!(first.contains("never performs or claims automatic migration"));
}

#[test]
fn resolver_uses_only_the_selected_runtime_artifact() {
    let router = PluginRouter::new();
    let package = Fixture::new("selected-artifact");
    package.write(
        "package.json",
        &package_json(
            "dual-runtime",
            r#", "main":"npm-main.js", "cordis":{"plugin":{"schemaVersion":"cordis.plugin/v1","runtime":"legacy-node","entry":"declared-legacy.js"}}, "tessivum":{"plugin":{"schemaVersion":"cordis.plugin/v1","runtime":"wasm","entry":"plugin.wasm"}}"#,
        ),
    );
    package.write("npm-main.js", "export const apply = () => 'npm main'");
    package.write(
        "declared-legacy.js",
        "export const apply = () => 'declared legacy'",
    );
    package.write("plugin.wasm", "not executed");
    package.write("other.wasm", "not executed");

    let legacy = router
        .resolve(package.path(), Some(PluginRuntime::LegacyNode))
        .unwrap();
    assert_eq!(
        legacy.artifact,
        Some(fs::canonicalize(package.path().join("declared-legacy.js")).unwrap())
    );
    assert_eq!(
        router
            .inspect(package.path(), Some(PluginRuntime::LegacyNode))
            .unwrap()
            .selected_runtime,
        PluginRuntime::LegacyNode
    );

    let wasm = router
        .resolve(package.path(), Some(PluginRuntime::Wasm))
        .unwrap();
    assert_eq!(
        wasm.artifact,
        Some(fs::canonicalize(package.path().join("plugin.wasm")).unwrap())
    );
    assert_eq!(
        router
            .inspect(package.path(), Some(PluginRuntime::Wasm))
            .unwrap()
            .selected_runtime,
        PluginRuntime::Wasm
    );
    assert_eq!(
        router
            .resolve(package.path(), None)
            .unwrap_err()
            .diagnostic()
            .code,
        "PLUGIN_RUNTIME_AMBIGUOUS"
    );
}

#[test]
fn wasm_resolution_rejects_multiple_unselected_artifacts() {
    let router = PluginRouter::new();
    let package = Fixture::new("ambiguous-wasm-artifacts");
    package.write(
        "package.json",
        &package_json("ambiguous-wasm", r#", "main":"index.js""#),
    );
    package.write("index.js", "export const apply = () => 1");
    package.write("first.wasm", "not executed");
    package.write("second.wasm", "not executed");

    let error = router
        .resolve(package.path(), Some(PluginRuntime::Wasm))
        .unwrap_err();
    assert_eq!(error.diagnostic().code, "PLUGIN_RUNTIME_AMBIGUOUS");
}

#[test]
fn inspection_limits_reject_huge_sparse_deep_and_total_packages() {
    let router = PluginRouter::new();
    let huge_manifest = Fixture::new("huge-manifest");
    huge_manifest.write("package.json", &" ".repeat(DEFAULT_MAX_MANIFEST_BYTES + 1));
    assert_eq!(
        router
            .inspect(huge_manifest.path(), None)
            .unwrap_err()
            .diagnostic()
            .code,
        "PLUGIN_INSPECTION_LIMIT_EXCEEDED"
    );

    let sparse_manifest = Fixture::new("sparse-manifest");
    sparse_manifest.sparse("package.json", (DEFAULT_MAX_MANIFEST_BYTES + 1) as u64);
    assert_eq!(
        router
            .inspect(sparse_manifest.path(), None)
            .unwrap_err()
            .diagnostic()
            .code,
        "PLUGIN_INSPECTION_LIMIT_EXCEEDED"
    );

    let huge_source = Fixture::new("huge-source");
    huge_source.write(
        "package.json",
        &package_json("huge-source", r#", "main":"index.js""#),
    );
    huge_source.write("index.js", &"x".repeat(DEFAULT_MAX_SOURCE_BYTES + 1));
    assert_eq!(
        router
            .inspect(huge_source.path(), None)
            .unwrap_err()
            .diagnostic()
            .code,
        "PLUGIN_INSPECTION_LIMIT_EXCEEDED"
    );

    let deep = Fixture::new("deep");
    deep.write(
        "package.json",
        &package_json("deep", r#", "main":"index.js""#),
    );
    deep.write("index.js", "export const apply = () => 1");
    deep.write(
        &format!(
            "{}/marker.txt",
            "nested/".repeat(DEFAULT_MAX_RECURSIVE_DEPTH + 1)
        ),
        "",
    );
    assert_eq!(
        router
            .inspect(deep.path(), None)
            .unwrap_err()
            .diagnostic()
            .code,
        "PLUGIN_INSPECTION_LIMIT_EXCEEDED"
    );

    let total = Fixture::new("total");
    total.write(
        "package.json",
        &package_json("total", r#", "main":"index.js""#),
    );
    total.write("index.js", "x".repeat(48).as_str());
    total.write("other.js", "x".repeat(48).as_str());
    let router = PluginRouter::with_limits(PluginInspectionLimits {
        max_manifest_bytes: 256,
        max_source_bytes: 64,
        max_total_bytes: 120,
        max_recursive_entries: 16,
        max_recursive_depth: 4,
    });
    assert_eq!(
        router
            .inspect(total.path(), None)
            .unwrap_err()
            .diagnostic()
            .code,
        "PLUGIN_INSPECTION_LIMIT_EXCEEDED"
    );
}

#[test]
fn inspection_limits_bound_recursive_entry_count() {
    let package = Fixture::new("entry-count");
    package.write(
        "package.json",
        &package_json("entry-count", r#", "main":"index.js""#),
    );
    package.write("index.js", "export const apply = () => 1");
    package.write("extra.js", "export const extra = () => 1");
    let router = PluginRouter::with_limits(PluginInspectionLimits {
        max_manifest_bytes: 256,
        max_source_bytes: 256,
        max_total_bytes: 1024,
        max_recursive_entries: 2,
        max_recursive_depth: 4,
    });
    assert_eq!(
        router
            .inspect(package.path(), None)
            .unwrap_err()
            .diagnostic()
            .code,
        "PLUGIN_INSPECTION_LIMIT_EXCEEDED"
    );
}

#[test]
fn wasm_resolution_rejects_conflicting_declared_entries() {
    let router = PluginRouter::new();
    let package = Fixture::new("conflicting-wasm-entries");
    package.write(
        "package.json",
        &package_json(
            "conflicting-wasm",
            r#", "cordis":{"plugin":{"schemaVersion":"cordis.plugin/v1","runtime":"wasm","entry":"first.wasm"}}, "tessivum":{"plugin":{"schemaVersion":"cordis.plugin/v1","runtime":"wasm","entry":"second.wasm"}}"#,
        ),
    );
    package.write("first.wasm", "not executed");
    package.write("second.wasm", "not executed");

    let error = router.resolve(package.path(), None).unwrap_err();
    assert_eq!(error.diagnostic().code, "PLUGIN_RUNTIME_AMBIGUOUS");
}

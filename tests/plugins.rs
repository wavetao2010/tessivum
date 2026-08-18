use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use tessivum::plugins::{
    CompatibilityClass, PluginInspectionLimits, PluginPackage, PluginRouter, PluginRuntime,
    ServiceMethodPermission, DEFAULT_MAX_MANIFEST_BYTES, DEFAULT_MAX_RECURSIVE_DEPTH,
    DEFAULT_MAX_SOURCE_BYTES,
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

#[derive(Clone, Copy)]
enum DeclarationLocation {
    External,
    Cordis,
    Tessivum,
}

const DECLARATION_LOCATIONS: [DeclarationLocation; 3] = [
    DeclarationLocation::External,
    DeclarationLocation::Cordis,
    DeclarationLocation::Tessivum,
];

fn wasm_product(id: &str, entry: &str, permissions: &str, service_permissions: &str) -> String {
    format!(
        r#"{{"schemaVersion":"cordis.plugin/v1","id":"{id}","version":"1.2.3","runtime":"wasm","entry":"{entry}","abi":"cordis.plugin/v1","inject":[],"permissions":{permissions},"servicePermissions":{service_permissions},"configSchema":{{"type":"object","additionalProperties":false}},"exports":["cordis_init","cordis_call","cordis_event","cordis_update","cordis_stop"]}}"#
    )
}

fn write_wasm_product(
    fixture: &Fixture,
    location: DeclarationLocation,
    id: &str,
    declaration: &str,
) {
    match location {
        DeclarationLocation::External => fixture.write("cordis.plugin.json", declaration),
        DeclarationLocation::Cordis => fixture.write(
            "package.json",
            &package_json(id, &format!(r#", "cordis":{{"plugin":{declaration}}}"#)),
        ),
        DeclarationLocation::Tessivum => fixture.write(
            "package.json",
            &package_json(id, &format!(r#", "tessivum":{{"plugin":{declaration}}}"#)),
        ),
    }
    fixture.write("plugin.wasm", "not executed");
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

#[test]
fn wasm_product_declaration_projects_exact_permissions_from_every_location() {
    let permissions = r#"["cordis.log","cordis.service.call"]"#;
    let service_permissions = r#"[
        {"service":"tools@1","methods":["schemas"]},
        {"service":"logger@1","methods":["log"]},
        {"service":"credentials@1","methods":["describe"]},
        {"service":"settings@1","methods":["describe"]}
    ]"#;
    let expected = BTreeSet::from([
        ServiceMethodPermission {
            service: "credentials@1".into(),
            method: "describe".into(),
        },
        ServiceMethodPermission {
            service: "logger@1".into(),
            method: "log".into(),
        },
        ServiceMethodPermission {
            service: "settings@1".into(),
            method: "describe".into(),
        },
        ServiceMethodPermission {
            service: "tools@1".into(),
            method: "schemas".into(),
        },
    ]);

    for (index, location) in DECLARATION_LOCATIONS.into_iter().enumerate() {
        let fixture = Fixture::new(&format!("wasm-product-{index}"));
        let id = format!("com.example.product-{index}");
        write_wasm_product(
            &fixture,
            location,
            &id,
            &wasm_product(&id, "plugin.wasm", permissions, service_permissions),
        );

        let product = PluginPackage::inspect(fixture.path())
            .unwrap()
            .wasm_product_declaration()
            .unwrap();
        assert_eq!(product.manifest.id, id);
        assert_eq!(product.manifest.entry, "plugin.wasm");
        assert_eq!(product.service_permissions, expected);
        assert_eq!(
            product.entry,
            fs::canonicalize(fixture.path().join("plugin.wasm")).unwrap()
        );
        assert_eq!(product.root, fs::canonicalize(fixture.path()).unwrap());

        let report = PluginRouter::new().inspect(fixture.path(), None).unwrap();
        assert_eq!(report.service_permissions[0].service, "credentials@1");
        assert_eq!(report.service_permissions[3].service, "tools@1");
        let report = serde_json::to_string(&report).unwrap();
        assert!(!report.contains("configSchema"));
    }
}

#[test]
fn empty_service_permissions_need_no_service_call_capability() {
    for (index, location) in DECLARATION_LOCATIONS.into_iter().enumerate() {
        let fixture = Fixture::new(&format!("empty-service-permissions-{index}"));
        let id = format!("com.example.empty-{index}");
        write_wasm_product(
            &fixture,
            location,
            &id,
            &wasm_product(&id, "plugin.wasm", r#"["cordis.log"]"#, "[]"),
        );
        let product = PluginPackage::inspect(fixture.path())
            .unwrap()
            .wasm_product_declaration()
            .unwrap();
        assert!(product.service_permissions.is_empty());
    }
}

#[test]
fn service_permission_validation_rejects_every_invalid_edge_in_every_location() {
    let cases = [
        (
            "blank-service",
            r#"["cordis.service.call"]"#,
            r#"[{"service":"","methods":["log"]}]"#,
        ),
        (
            "blank-method",
            r#"["cordis.service.call"]"#,
            r#"[{"service":"logger@1","methods":[""]}]"#,
        ),
        (
            "empty-methods",
            r#"["cordis.service.call"]"#,
            r#"[{"service":"logger@1","methods":[]}]"#,
        ),
        (
            "duplicate-method",
            r#"["cordis.service.call"]"#,
            r#"[{"service":"logger@1","methods":["log","log"]}]"#,
        ),
        (
            "duplicate-service",
            r#"["cordis.service.call"]"#,
            r#"[{"service":"logger@1","methods":["log"]},{"service":"logger@1","methods":["log"]}]"#,
        ),
        (
            "wildcard-service",
            r#"["cordis.service.call"]"#,
            r#"[{"service":"logger@*","methods":["log"]}]"#,
        ),
        (
            "pattern-method",
            r#"["cordis.service.call"]"#,
            r#"[{"service":"logger@1","methods":["log*"]}]"#,
        ),
        (
            "unknown-service",
            r#"["cordis.service.call"]"#,
            r#"[{"service":"llm@1","methods":["generate"]}]"#,
        ),
        (
            "unknown-method",
            r#"["cordis.service.call"]"#,
            r#"[{"service":"tools@1","methods":["execute"]}]"#,
        ),
        (
            "missing-capability",
            r#"["cordis.log"]"#,
            r#"[{"service":"logger@1","methods":["log"]}]"#,
        ),
    ];
    for (case_name, permissions, service_permissions) in cases {
        for (index, location) in DECLARATION_LOCATIONS.into_iter().enumerate() {
            let fixture = Fixture::new(&format!("permission-{case_name}-{index}"));
            let id = format!("com.example.{case_name}-{index}");
            write_wasm_product(
                &fixture,
                location,
                &id,
                &wasm_product(&id, "plugin.wasm", permissions, service_permissions),
            );
            assert_eq!(
                PluginPackage::inspect(fixture.path())
                    .unwrap_err()
                    .diagnostic()
                    .code,
                "MANIFEST_PERMISSION_INVALID",
                "{case_name} at declaration location {index}"
            );
        }
    }
}

#[test]
fn wasm_product_declaration_requires_complete_confined_core_manifest() {
    for (index, location) in DECLARATION_LOCATIONS.into_iter().enumerate() {
        let sparse = Fixture::new(&format!("sparse-wasm-product-{index}"));
        let id = format!("com.example.sparse-{index}");
        write_wasm_product(
            &sparse,
            location,
            &id,
            r#"{"schemaVersion":"cordis.plugin/v1","runtime":"wasm","entry":"plugin.wasm"}"#,
        );
        assert_eq!(
            PluginPackage::inspect(sparse.path())
                .unwrap()
                .wasm_product_declaration()
                .unwrap_err()
                .diagnostic()
                .code,
            "PLUGIN_MANIFEST_INVALID"
        );

        let traversal = Fixture::new(&format!("traversal-wasm-product-{index}"));
        write_wasm_product(
            &traversal,
            location,
            &id,
            &wasm_product(&id, "../plugin.wasm", r#"["cordis.log"]"#, "[]"),
        );
        assert_eq!(
            PluginPackage::inspect(traversal.path())
                .unwrap()
                .wasm_product_declaration()
                .unwrap_err()
                .diagnostic()
                .code,
            "PLUGIN_MANIFEST_INVALID"
        );
    }
}

#[test]
fn wasm_product_declaration_requires_every_core_projection_field() {
    for field in [
        "id",
        "version",
        "entry",
        "abi",
        "inject",
        "permissions",
        "configSchema",
        "exports",
    ] {
        let id = format!("com.example.missing-{field}");
        let mut declaration: serde_json::Value =
            serde_json::from_str(&wasm_product(&id, "plugin.wasm", r#"["cordis.log"]"#, "[]"))
                .unwrap();
        declaration.as_object_mut().unwrap().remove(field);
        let declaration = serde_json::to_string(&declaration).unwrap();
        for (index, location) in DECLARATION_LOCATIONS.into_iter().enumerate() {
            let fixture = Fixture::new(&format!("missing-{field}-{index}"));
            write_wasm_product(&fixture, location, &id, &declaration);
            assert_eq!(
                PluginPackage::inspect(fixture.path())
                    .unwrap()
                    .wasm_product_declaration()
                    .unwrap_err()
                    .diagnostic()
                    .code,
                "PLUGIN_MANIFEST_INVALID",
                "missing {field} at declaration location {index}"
            );
        }
    }
}

#[test]
fn wasm_product_declaration_rejects_duplicate_or_conflicting_declarations() {
    let declaration = wasm_product(
        "com.example.duplicate",
        "plugin.wasm",
        r#"["cordis.log"]"#,
        "[]",
    );
    let external_and_package = Fixture::new("external-and-package-wasm");
    write_wasm_product(
        &external_and_package,
        DeclarationLocation::External,
        "com.example.duplicate",
        &declaration,
    );
    external_and_package.write(
        "package.json",
        &package_json(
            "duplicate",
            &format!(r#", "tessivum":{{"plugin":{declaration}}}"#),
        ),
    );
    assert_eq!(
        PluginPackage::inspect(external_and_package.path())
            .unwrap()
            .wasm_product_declaration()
            .unwrap_err()
            .diagnostic()
            .code,
        "PLUGIN_RUNTIME_AMBIGUOUS"
    );

    let conflicting = Fixture::new("conflicting-package-wasm");
    let first = wasm_product(
        "com.example.conflicting",
        "first.wasm",
        r#"["cordis.log"]"#,
        "[]",
    );
    let second = wasm_product(
        "com.example.conflicting",
        "second.wasm",
        r#"["cordis.log"]"#,
        "[]",
    );
    conflicting.write(
        "package.json",
        &package_json(
            "conflicting",
            &format!(r#", "cordis":{{"plugin":{first}}}, "tessivum":{{"plugin":{second}}}"#),
        ),
    );
    conflicting.write("first.wasm", "not executed");
    conflicting.write("second.wasm", "not executed");
    assert_eq!(
        PluginPackage::inspect(conflicting.path())
            .unwrap()
            .wasm_product_declaration()
            .unwrap_err()
            .diagnostic()
            .code,
        "PLUGIN_RUNTIME_AMBIGUOUS"
    );
}

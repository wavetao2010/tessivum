use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::json;
use sha2::{Digest, Sha256};
use tessivum::{
    agent::AgentRegistry,
    bridge::{BridgeServices, DomainBridge, WasmEffectivePolicy, WasmPolicyRegistry},
    host::{HostConfig, HostRuntime},
    legacy::{product_loader, ProductPackageResolver, WasmProductRuntime},
    llm::LlmRuntime,
    plugins::PluginPackage,
    session::{MemorySessionPersistence, SessionStore},
    system_prompt::SystemPrompt,
    tools::ToolRuntime,
};
use tessivum_core::{
    ContextHandle, Entry, EntryId, EntryOptions, EntryTree, LoaderFuture, PackageResolver, Patch,
    ResolvedPackage, RuntimeKind,
};
use tessivum_extism::{
    Capability, CapabilityHandler, CapabilityRegistry, ExtismGuestEngine, ResourceLimits,
    WasmPackage, WasmPluginInstance,
};
use uuid::Uuid;

const PLUGIN_ID: &str = "com.tessivum.fixture.rust-minimal";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tessivum-wasm-{}", Uuid::new_v4()));
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

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture() -> PathBuf {
    repository_root().join("fixtures/wasm/rust-minimal")
}

fn entry() -> Entry {
    entry_for(&fixture(), "rust-minimal")
}

fn entry_for(package: &Path, id: &str) -> Entry {
    Entry::new(
        package.to_string_lossy(),
        EntryOptions {
            id: EntryId::new(id).unwrap(),
            name: Some(id.into()),
            runtime: RuntimeKind::Wasm,
            config: json!({}),
            inject: Vec::new(),
            isolate: Vec::new(),
            intercept: json!({}),
            disabled: false,
            group: None,
        },
    )
}

fn bridge_services() -> BridgeServices {
    let sessions = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let agents = AgentRegistry::new(sessions.clone());
    BridgeServices::new(
        ToolRuntime::new(),
        SystemPrompt::new(),
        LlmRuntime::new(),
        sessions,
        agents,
    )
}

fn capabilities(policies: WasmPolicyRegistry) -> Arc<CapabilityRegistry> {
    let bridge = DomainBridge::with_policy_registry(bridge_services(), policies).unwrap();
    let capabilities = Arc::new(CapabilityRegistry::new());
    capabilities
        .register(Capability::ServiceCall, move |request| {
            CapabilityHandler::call(&bridge, request)
        })
        .unwrap();
    capabilities.grant(Capability::ServiceCall);
    capabilities
}

struct LogicalResolver(PathBuf);

impl PackageResolver for LogicalResolver {
    fn resolve<'a>(
        &'a self,
        specifier: &'a str,
        _: RuntimeKind,
    ) -> LoaderFuture<'a, ResolvedPackage> {
        let location = self.0.to_string_lossy().into_owned();
        Box::pin(async move {
            Ok(ResolvedPackage {
                specifier: specifier.into(),
                location,
            })
        })
    }
}

fn copy_fixture(root: &TempDir) -> PathBuf {
    let package = root.path().join("plugin");
    fs::create_dir_all(&package).unwrap();
    for file in ["plugin.json", "plugin.wasm"] {
        fs::copy(fixture().join(file), package.join(file)).unwrap();
    }
    package
}

#[tokio::test(flavor = "multi_thread")]
async fn host_loads_and_unloads_real_wasm_without_legacy_node() {
    let temp = TempDir::new();
    let mut config = HostConfig::new(repository_root(), temp.path().join("data"));
    config.entries = Some(EntryTree {
        entries: vec![entry()],
        groups: Vec::new(),
    });

    let host = HostRuntime::boot(config).await.unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn product_loader_installs_exact_policy_and_revokes_it_on_unload() {
    let policies = WasmPolicyRegistry::new();
    let capabilities = capabilities(policies.clone());
    let wasm = Arc::new(WasmProductRuntime::new(
        capabilities,
        policies.clone(),
        ResourceLimits::default(),
    ));
    let resolver: Arc<dyn PackageResolver> = Arc::new(
        ProductPackageResolver::new()
            .confine_to(repository_root())
            .unwrap(),
    );
    let root = ContextHandle::root();
    let mut loader = product_loader(None, resolver, wasm)
        .unwrap()
        .with_context(root.clone());

    loader
        .load(EntryTree {
            entries: vec![entry()],
            groups: Vec::new(),
        })
        .await
        .unwrap();
    let first = policies.active_instances(PLUGIN_ID);
    assert_eq!(first.len(), 1);
    loader
        .update(&[Patch::UpdateConfig {
            id: EntryId::new("rust-minimal").unwrap(),
            config: json!({}),
        }])
        .await
        .unwrap();
    let second = policies.active_instances(PLUGIN_ID);
    assert_eq!(second.len(), 1);
    assert_ne!(first, second, "candidate receives a fresh authority");

    loader.unload().await.unwrap();
    assert!(policies.active_instances(PLUGIN_ID).is_empty());
    root.scope().dispose().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn logical_resolver_loads_the_resolved_wasm_artifact() {
    let policies = WasmPolicyRegistry::new();
    let wasm = Arc::new(WasmProductRuntime::new(
        capabilities(policies.clone()),
        policies,
        ResourceLimits::default(),
    ));
    let resolver: Arc<dyn PackageResolver> =
        Arc::new(LogicalResolver(fixture().join("plugin.wasm")));
    let root = ContextHandle::root();
    let mut loader = product_loader(None, resolver, wasm)
        .unwrap()
        .with_context(root.clone());
    loader
        .load(EntryTree {
            entries: vec![entry_for(Path::new("com.example.rust-minimal"), "logical")],
            groups: Vec::new(),
        })
        .await
        .unwrap();
    loader.unload().await.unwrap();
    root.scope().dispose().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn manifest_only_reload_uses_the_new_identity_and_contract() {
    let temp = TempDir::new();
    let package = copy_fixture(&temp);
    let policies = WasmPolicyRegistry::new();
    let wasm = Arc::new(WasmProductRuntime::new(
        capabilities(policies.clone()),
        policies.clone(),
        ResourceLimits::default(),
    ));
    let resolver: Arc<dyn PackageResolver> = Arc::new(
        ProductPackageResolver::new()
            .confine_to(temp.path())
            .unwrap(),
    );
    let root = ContextHandle::root();
    let mut loader = product_loader(None, resolver, wasm)
        .unwrap()
        .with_context(root.clone());
    let tree = EntryTree {
        entries: vec![entry_for(&package, "mutable")],
        groups: Vec::new(),
    };

    loader.load(tree.clone()).await.unwrap();
    loader.unload().await.unwrap();
    let next_id = "com.tessivum.fixture.rust-minimal-next";
    let manifest_path = package.join("plugin.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["id"] = json!(next_id);
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    loader.load(tree).await.unwrap();
    assert!(policies.active_instances(PLUGIN_ID).is_empty());
    assert_eq!(policies.active_instances(next_id).len(), 1);
    loader.unload().await.unwrap();
    root.scope().dispose().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_manifest_id_across_entries_fails_closed() {
    let policies = WasmPolicyRegistry::new();
    let wasm = Arc::new(WasmProductRuntime::new(
        capabilities(policies.clone()),
        policies.clone(),
        ResourceLimits::default(),
    ));
    let resolver: Arc<dyn PackageResolver> = Arc::new(
        ProductPackageResolver::new()
            .confine_to(repository_root())
            .unwrap(),
    );
    let root = ContextHandle::root();
    let mut loader = product_loader(None, resolver, wasm)
        .unwrap()
        .with_context(root.clone());

    assert!(loader
        .load(EntryTree {
            entries: vec![entry_for(&fixture(), "one"), entry_for(&fixture(), "two")],
            groups: Vec::new(),
        })
        .await
        .is_err());
    assert!(policies.active_instances(PLUGIN_ID).is_empty());
    root.scope().dispose().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn real_guest_denies_undeclared_service_and_traps_deterministically() {
    let product = PluginPackage::inspect(fixture())
        .unwrap()
        .wasm_product_declaration()
        .unwrap();
    let rebuilt = env::var_os("TESSIVUM_WASM_GUEST");
    let wasm = match &rebuilt {
        Some(path) => fs::read(path).unwrap(),
        None => fs::read(&product.entry).unwrap(),
    };
    if rebuilt.is_none() {
        let expected = fs::read_to_string(fixture().join("plugin.wasm.sha256")).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&wasm)),
            expected.split_whitespace().next().unwrap()
        );
    }

    let policies = WasmPolicyRegistry::new();
    let instance_id = "direct-real-guest";
    let registration = policies
        .install(WasmEffectivePolicy::new(
            product.manifest.id.clone(),
            instance_id,
            "direct-real-entry",
            product.service_permissions.clone(),
        ))
        .unwrap();
    let capabilities = capabilities(policies);
    let package = WasmPackage::from_bytes(product.manifest, wasm).unwrap();
    let instance = WasmPluginInstance::instantiate_with_instance_id(
        package,
        Arc::new(ExtismGuestEngine),
        capabilities,
        ResourceLimits::default(),
        json!({}),
        instance_id,
    )
    .unwrap();

    assert_eq!(instance.init(json!({})).unwrap()["initialized"], true);
    assert_eq!(
        instance.call(json!({}), json!({"mode": "denied"})).unwrap(),
        json!({"denial": {"code": "SERVICE_PERMISSION_DENIED"}})
    );
    let trap = instance
        .call(json!({}), json!({"mode": "trap"}))
        .unwrap_err();
    assert_eq!(trap.code, "GUEST_TRAP");
    assert_eq!(trap.phase, "call");

    instance.stop().unwrap();
    registration.revoke();
    registration.drain(Duration::from_secs(1)).unwrap();
}

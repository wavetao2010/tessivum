use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::json;
use sha2::{Digest, Sha256};
use tessivum::{
    agent::AgentRegistry,
    bridge::BridgeServices,
    composition::{
        CompositionDescriptor, CompositionEntryReference, CompositionRegistry, CompositionRuntime,
    },
    legacy::{LegacyProfile, ProductPackageResolver},
    llm::LlmRuntime,
    plugins::PluginRouter,
    session::{MemorySessionPersistence, SessionStore},
    system_prompt::SystemPrompt,
    tools::ToolRuntime,
    SessionId,
};
use tessivum_core::{ContextHandle, LoaderRuntime, PackageResolver};
use tessivum_node_bridge::{ClientConfig, FrameKind, HostCommand};

const TIMER_PACKAGE_HASH: &str = "ecb8ac09dfd326400c1b9893415cbc92077ce8409b0cb8cdcd45dc3ac9f1b0bc";
const TIMER_INDEX_HASH: &str = "cbf60311b58210f6f2c6ee1bbd438039806f5ee6b268a46906a949ad926cca81";
const HTTP_PACKAGE_HASH: &str = "fb11c50a758bbff911a5059b69286f4696fc79dc9dfadd1e835ee1044117f395";
const HTTP_INDEX_HASH: &str = "fde73d75e8a2962292f5644b9c9704e923d546b7a001f7a0d0c969cf20dff2da";
const PINNED_VENDOR_FILES: [&str; 3] = [
    "cordis/src/index.ts",
    "cosmokit/src/index.ts",
    "loader/src/index.ts",
];

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scharness_root() -> PathBuf {
    workspace()
        .parent()
        .expect("tessivum checkout has the SCHarness repository as its parent")
        .canonicalize()
        .expect("SCHarness repository root is readable")
}

fn assert_portable_report(report: &serde_json::Value) {
    let checkout_root = scharness_root();
    assert!(
        !report
            .to_string()
            .contains(checkout_root.to_string_lossy().as_ref()),
        "generated compatibility report must not embed its local checkout root"
    );
}

fn community_package(name: &str) -> PathBuf {
    workspace().join("fixtures/community").join(name)
}

fn assert_published_file(path: impl AsRef<Path>, expected: &str) {
    let bytes = fs::read(path.as_ref()).expect("vendored published artifact is readable");
    assert_eq!(
        format!("{:x}", Sha256::digest(bytes)),
        expected,
        "{} differs from the published npm artifact",
        path.as_ref().display(),
    );
}

#[test]
fn community_packages_are_published_and_routed_without_source_rewrites() {
    let timer = community_package("timer");
    let http = community_package("http");
    assert_published_file(timer.join("package.json"), TIMER_PACKAGE_HASH);
    assert_published_file(timer.join("lib/index.js"), TIMER_INDEX_HASH);
    assert_published_file(http.join("package.json"), HTTP_PACKAGE_HASH);
    assert_published_file(http.join("lib/index.js"), HTTP_INDEX_HASH);

    let router = PluginRouter::new();
    let timer_report = serde_json::to_value(
        router
            .inspect(&timer, None)
            .expect("published timer package is inspectable"),
    )
    .expect("timer compatibility report serializes");
    assert_portable_report(&timer_report);
    assert_eq!(timer_report["package"], "@cordisjs/plugin-timer");
    assert_eq!(timer_report["version"], "1.1.2");
    assert_eq!(timer_report["selectedRuntime"], "legacy-node");
    assert_eq!(timer_report["compatibility"], "direct-legacy");

    let http_report = serde_json::to_value(
        router
            .inspect(&http, None)
            .expect("published HTTP package is inspectable"),
    )
    .expect("HTTP compatibility report serializes");
    assert_portable_report(&http_report);
    assert_eq!(http_report["package"], "@cordisjs/plugin-http");
    assert_eq!(http_report["version"], "1.5.1");
    assert_eq!(http_report["selectedRuntime"], "legacy-node");
    assert_eq!(http_report["compatibility"], "needs-proxy");
    let services = http_report["stableCrossRuntimeServices"]
        .as_array()
        .expect("HTTP report includes stable cross-runtime service evidence");
    assert!(
        services.iter().any(|service| service == "logger@1"),
        "HTTP report identifies its logger proxy need: {http_report}",
    );
    let node_builtins = http_report["staticMarkers"]["nodeBuiltins"]
        .as_array()
        .expect("HTTP report includes Node API evidence");
    assert!(
        node_builtins.iter().any(|marker| marker == "module"),
        "HTTP report identifies its Node module/network client boundary: {http_report}",
    );
}

fn write_module_alias(directory: &Path, name: &str, target: &Path) {
    fs::create_dir_all(directory).expect("test-local package alias directory exists");
    fs::write(
        directory.join("package.json"),
        json!({ "name": name, "type": "module", "exports": "./index.mjs" }).to_string(),
    )
    .expect("test-local package alias manifest is written");
    fs::write(
        directory.join("index.mjs"),
        format!(
            "export * from {};\n",
            serde_json::to_string(target.to_str().expect("pinned vendor entry is valid UTF-8"),)
                .expect("vendor entry is a valid JavaScript specifier"),
        ),
    )
    .expect("test-local package alias re-exports the pinned vendor entry");
}

struct TemporaryTimerPackage {
    root: PathBuf,
}

impl TemporaryTimerPackage {
    fn new(vendor: &Path) -> Self {
        let root = std::env::temp_dir().join(format!(
            "tessivum-community-timer-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time is after the epoch")
                .as_nanos(),
        ));
        let timer = root.join("timer");
        fs::create_dir_all(timer.join("lib")).expect("temporary timer package directory exists");
        fs::copy(
            community_package("timer").join("package.json"),
            timer.join("package.json"),
        )
        .expect("published timer manifest copies byte-for-byte");
        fs::copy(
            community_package("timer").join("lib/index.js"),
            timer.join("lib/index.js"),
        )
        .expect("published timer entry copies byte-for-byte");
        assert_published_file(timer.join("package.json"), TIMER_PACKAGE_HASH);
        assert_published_file(timer.join("lib/index.js"), TIMER_INDEX_HASH);
        let node_modules = root.join("node_modules");
        write_module_alias(
            &node_modules.join("cordis"),
            "cordis",
            &vendor.join("cordis/src/index.ts"),
        );
        write_module_alias(
            &node_modules.join("@deepseek-ai/cosmokit"),
            "@deepseek-ai/cosmokit",
            &vendor.join("cosmokit/src/index.ts"),
        );

        Self { root }
    }

    fn entry(&self) -> PathBuf {
        self.root.join("timer/lib/index.js")
    }
}

impl Drop for TemporaryTimerPackage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn core_root() -> PathBuf {
    if let Some(configured) = env::var_os("TESSIVUM_CORE_SOURCE").map(PathBuf::from) {
        assert!(
            configured.join("node/compat-host/src/index.ts").is_file(),
            "TESSIVUM_CORE_SOURCE must expose the Bun compatibility host"
        );
        return configured;
    }
    let local = scharness_root().join("tessivum-core");
    if local.join("node/compat-host/src/index.ts").is_file() {
        return local;
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")))
        .expect("Cargo home is available");
    let mut candidates = fs::read_dir(cargo_home.join("git/checkouts"))
        .expect("pinned tessivum-core checkout exists")
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("tessivum-core-")
        })
        .flat_map(|entry| fs::read_dir(entry.path()).into_iter().flatten().flatten())
        .map(|entry| entry.path())
        .filter(|path| path.join("node/compat-host/src/index.ts").is_file())
        .collect::<Vec<_>>();
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .expect("pinned tessivum-core exposes its sibling Bun compat host")
}

fn vendor_root() -> PathBuf {
    let candidates = [
        env::var_os("TESSIVUM_DEEPSEEK_VENDOR").map(PathBuf::from),
        Some(scharness_root().join("upstream/deepseek-harness/vendor")),
    ];
    let root = candidates
        .into_iter()
        .flatten()
        .find(|root| {
            root.is_dir()
                && PINNED_VENDOR_FILES
                    .iter()
                    .all(|file| root.join(file).is_file())
        })
        .expect("pinned DeepSeek vendor or installed npm source exists")
        .canonicalize()
        .expect("pinned vendor root is readable");
    for file in PINNED_VENDOR_FILES {
        let file = root
            .join(file)
            .canonicalize()
            .expect("pinned vendor source exists");
        assert!(file.starts_with(&root));
    }
    root
}

fn bridge_services() -> BridgeServices {
    let sessions = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    BridgeServices::new(
        ToolRuntime::new(),
        SystemPrompt::new(),
        LlmRuntime::new(),
        sessions.clone(),
        AgentRegistry::new(sessions),
    )
}

#[tokio::test]
async fn vendored_timer_loads_unchanged_through_the_legacy_profile_and_reaps_after_disconnect() {
    let core = core_root();
    let vendor = vendor_root();
    let package = TemporaryTimerPackage::new(&vendor);
    let command = HostCommand::new("bun")
        .arg("run")
        .arg(core.join("node/compat-host/src/index.ts"))
        .current_dir(core.join("node/compat-host"))
        .env("CORDIS_VENDOR_ROOT", &vendor);
    let client_config = ClientConfig {
        handshake_timeout: Duration::from_secs(30),
        ..ClientConfig::default()
    };
    let profile = LegacyProfile::new(command, client_config, bridge_services())
        .expect("legacy profile accepts its explicit Bun/vendor environment");
    profile.start().expect("Bun compat host starts");
    let runtime = profile
        .runtime()
        .expect("started profile exposes its legacy runtime");
    let client = runtime.client();
    let entry = package.entry().to_string_lossy().into_owned();

    let first_load = client.request(
        FrameKind::PluginLoad,
        json!({
            "pluginId": "published-timer",
            "package": { "specifier": entry.clone(), "location": entry.clone() },
            "config": {},
        }),
        Duration::from_secs(5),
    );
    let disposed = first_load.as_ref().ok().and_then(|_| {
        client
            .request(
                FrameKind::PluginDispose,
                json!({ "pluginId": "published-timer" }),
                Duration::from_secs(5),
            )
            .ok()
    });
    let second_load = client.request(
        FrameKind::PluginLoad,
        json!({
            "pluginId": "published-timer-crash-cleanup",
            "package": { "specifier": entry.clone(), "location": entry },
            "config": {},
        }),
        Duration::from_secs(5),
    );

    // Closing the framed transport leaves the second live plugin to the profile's
    // generation cleanup; shutdown must still reap the Bun process tree.
    client.close();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if format!("{:?}", profile.health()).contains("Stopped") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("profile observes the disconnected Bun host and cleans its generation");
    let shutdown = profile.shutdown().await;

    assert_eq!(
        first_load.expect("published timer activates unchanged")["state"],
        "ACTIVE"
    );
    assert_eq!(
        disposed.expect("timer's async Cordis disposer completes")["disposed"],
        true
    );
    assert_eq!(
        second_load.expect("timer can load again before disconnect cleanup")["state"],
        "ACTIVE"
    );
    shutdown.expect("profile cleans up the active generation after host disconnect");
}

#[tokio::test]
async fn legacy_compositions_with_the_same_descriptor_id_are_session_isolated() {
    let core = core_root();
    let vendor = vendor_root();
    let package = TemporaryTimerPackage::new(&vendor);
    let noop = package.root.join("noop");
    fs::create_dir_all(&noop).unwrap();
    fs::write(
        noop.join("package.json"),
        r#"{"name":"legacy-composition-noop","type":"module","main":"index.js"}"#,
    )
    .unwrap();
    fs::write(noop.join("index.js"), "export default function noop() {}\n").unwrap();
    let command = HostCommand::new("bun")
        .arg("run")
        .arg(core.join("node/compat-host/src/index.ts"))
        .current_dir(core.join("node/compat-host"))
        .env("CORDIS_VENDOR_ROOT", &vendor);
    let profile = LegacyProfile::new(
        command,
        ClientConfig {
            handshake_timeout: Duration::from_secs(30),
            ..ClientConfig::default()
        },
        bridge_services(),
    )
    .unwrap();
    profile.start().unwrap();
    let runtime: Arc<dyn LoaderRuntime> = Arc::new(profile.runtime().unwrap());
    let resolver: Arc<dyn PackageResolver> = Arc::new(
        ProductPackageResolver::new()
            .confine_to(&package.root)
            .unwrap(),
    );
    let registry = CompositionRegistry::new(resolver, [runtime]).unwrap();
    let roots = [ContextHandle::root(), ContextHandle::root()];
    let owners = [SessionId::from("left"), SessionId::from("right")];
    let source = noop.to_string_lossy().into_owned();

    for (owner, root) in owners.iter().zip(&roots) {
        registry
            .attach_session(owner.clone(), root.clone())
            .unwrap();
        registry
            .define(
                owner,
                CompositionDescriptor {
                    id: "shared-plugin".into(),
                    entry: CompositionEntryReference {
                        runtime: CompositionRuntime::Legacy,
                        package: source.clone(),
                    },
                    config: json!({}),
                },
            )
            .await
            .unwrap();
        registry.validate(owner, "shared-plugin").await.unwrap();
    }
    for owner in &owners {
        let running = registry.run(owner, "shared-plugin").await.unwrap();
        assert_eq!(running.descriptor.id, "shared-plugin");
        assert_eq!(running.core.entry.options.id.as_str(), "shared-plugin");
    }
    for owner in &owners {
        registry.stop(owner, "shared-plugin").await.unwrap();
        registry.dispose_session(owner).await.unwrap();
    }
    for root in roots {
        root.scope().dispose().await.unwrap();
    }
    profile.shutdown().await.unwrap();
}

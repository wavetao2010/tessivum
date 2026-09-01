use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{json, Value};
use tessivum::{
    agent::AgentRegistry,
    bridge::{BridgeServices, DomainBridge, WasmPolicyRegistry},
    legacy::{product_loader, ProductPackageResolver, WasmProductRuntime},
    llm::LlmRuntime,
    plugins::{PluginRouter, PluginRuntime},
    session::{MemorySessionPersistence, SessionStore},
    system_prompt::SystemPrompt,
    tools::{
        ToolDefinition, ToolHandler, ToolHandlerResult, ToolOutput, ToolRunContext, ToolRuntime,
    },
    ContentBlock, SessionId, ToolCallId,
};
use tessivum_core::{
    ContextHandle, Entry, EntryId, EntryOptions, EntryTree, PackageResolver, RuntimeKind,
};
use tessivum_extism::{Capability, CapabilityHandler, CapabilityRegistry, ResourceLimits};
use tessivum_node_bridge::{ClientConfig, FrameKind, HostCommand, NodeSupervisor};
use uuid::Uuid;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("tessivum-package-interop-{}", Uuid::new_v4()));
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

struct NativeEcho;

#[async_trait]
impl ToolHandler for NativeEcho {
    async fn run(&self, _: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        Ok(ToolOutput::new(
            vec![ContentBlock::Text {
                text: arguments["value"].as_str().unwrap().to_owned(),
            }],
            false,
            json!({"runtime": "native"}),
        ))
    }
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repository_root() -> PathBuf {
    crate_root().parent().unwrap().to_path_buf()
}

fn core_source() -> PathBuf {
    std::env::var_os("TESSIVUM_CORE_SOURCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| repository_root().join("tessivum-core"))
}

fn deepseek_source() -> PathBuf {
    std::env::var_os("TESSIVUM_DEEPSEEK_SOURCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| repository_root().join("upstream/deepseek-harness"))
}

fn bridge_services(tools: ToolRuntime) -> BridgeServices {
    let sessions = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    BridgeServices::new(
        tools,
        SystemPrompt::new(),
        LlmRuntime::new(),
        sessions.clone(),
        AgentRegistry::new(sessions),
    )
}

fn wasm_entry() -> Entry {
    Entry::new(
        crate_root()
            .join("fixtures/wasm/rust-minimal")
            .to_string_lossy(),
        EntryOptions {
            id: EntryId::new("deep-interop-wasm").unwrap(),
            name: Some("deep-interop-wasm".into()),
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

fn node_command() -> HostCommand {
    let root = core_source();
    let vendor = std::env::var_os("TESSIVUM_DEEPSEEK_VENDOR")
        .map(PathBuf::from)
        .unwrap_or_else(|| deepseek_source().join("vendor"));
    HostCommand::new("bun")
        .arg("run")
        .arg(root.join("node/compat-host/src/index.ts"))
        .env("CORDIS_VENDOR_ROOT", vendor)
        .current_dir(root.join("node/compat-host"))
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_runner_loads_deep_interop_package_and_bridges_legacy_node() {
    let tools = ToolRuntime::new();
    let _native = tools
        .register(ToolDefinition::new(
            "deep_native_echo",
            "proves the native extension lane",
            json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false
            }),
            NativeEcho,
        ))
        .unwrap();
    let native_context = ContextHandle::root();
    let native = tools
        .execute(
            ToolRunContext {
                session: SessionId::from("deep-interop"),
                call: ToolCallId::from("native"),
                cancellation: native_context.scope().cancellation(),
            },
            "deep_native_echo",
            json!({"value": "native-ok"}),
        )
        .await;
    assert_eq!(
        native.content,
        vec![ContentBlock::Text {
            text: "native-ok".into()
        }]
    );

    let policies = WasmPolicyRegistry::new();
    let hostcalls = Arc::new(AtomicUsize::new(0));
    let bridge =
        DomainBridge::with_policy_registry(bridge_services(tools), policies.clone()).unwrap();
    let capabilities = Arc::new(CapabilityRegistry::new());
    let seen = Arc::clone(&hostcalls);
    capabilities
        .register(Capability::ServiceCall, move |request| {
            seen.fetch_add(1, Ordering::SeqCst);
            CapabilityHandler::call(&bridge, request)
        })
        .unwrap();
    capabilities.grant(Capability::ServiceCall);
    let wasm = Arc::new(WasmProductRuntime::new(
        capabilities,
        policies,
        ResourceLimits::default(),
    ));
    let resolver: Arc<dyn PackageResolver> = Arc::new(
        ProductPackageResolver::new()
            .confine_to(crate_root())
            .unwrap(),
    );
    let wasm_context = ContextHandle::root();
    let mut loader = product_loader(None, resolver, wasm)
        .unwrap()
        .with_context(wasm_context.clone());
    loader
        .load(EntryTree {
            entries: vec![wasm_entry()],
            groups: Vec::new(),
        })
        .await
        .unwrap();
    assert!(
        hostcalls.load(Ordering::SeqCst) >= 1,
        "the real WASM guest must call logger@1 through its authorized hostcall"
    );
    loader.unload().await.unwrap();
    wasm_context.scope().dispose().await.unwrap();

    let temp = TempDir::new();
    let npm = temp.path().join("deep-interop-node");
    fs::create_dir(&npm).unwrap();
    fs::write(
        npm.join("package.json"),
        r#"{"name":"@fixture/deep-interop","type":"module","exports":"./index.ts"}"#,
    )
    .unwrap();
    fs::copy(
        core_source().join("fixtures/legacy/function-plugin.ts"),
        npm.join("index.ts"),
    )
    .unwrap();
    let supervisor = NodeSupervisor::new(node_command(), ClientConfig::default()).unwrap();
    let client = supervisor.start().unwrap();
    let entry = npm.join("index.ts").to_string_lossy().into_owned();
    assert_eq!(
        client
            .request(
                FrameKind::PluginLoad,
                json!({
                    "pluginId": "deep-interop-node",
                    "package": {"specifier": "@fixture/deep-interop", "location": entry},
                    "config": {"prefix": "node"}
                }),
                Duration::from_secs(5),
            )
            .unwrap()["state"],
        "ACTIVE"
    );
    let canonical_call = fs::read_to_string(core_source().join("node/compat-host/src/protocol.ts"))
        .unwrap()
        .contains("parseServiceCall");
    let service_call = if canonical_call {
        json!({"service": "legacy.function", "method": "inspect", "params": ["bridge-ok"]})
    } else {
        json!({"service": "legacy.function", "method": "inspect", "args": ["bridge-ok"]})
    };
    assert_eq!(
        client
            .request(FrameKind::ServiceCall, service_call, Duration::from_secs(2),)
            .unwrap(),
        json!({"prefix": "node", "value": "bridge-ok", "file": "index.ts", "readable": true})
    );
    client
        .request(
            FrameKind::PluginDispose,
            json!({"pluginId": "deep-interop-node"}),
            Duration::from_secs(2),
        )
        .unwrap();
    supervisor.shutdown().unwrap();

    let browser = PluginRouter::new()
        .inspect(deepseek_source().join("packages/client/ui-settings"), None)
        .unwrap();
    assert_eq!(browser.selected_runtime, PluginRuntime::Browser);
    assert_eq!(browser.dsh_client.unwrap()["platform"], "web");

    native_context.scope().dispose().await.unwrap();
}

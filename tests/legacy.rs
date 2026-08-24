use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::{json, Value};
use tessivum::{
    agent::AgentRegistry,
    bridge::BridgeServices,
    legacy::{legacy_loader, LegacyProfile, LegacyProfileHealth, ProductPackageResolver},
    llm::LlmRuntime,
    session::{MemorySessionPersistence, SessionStore},
    system_prompt::SystemPrompt,
    tools::ToolRuntime,
};
use tessivum_core::{
    Entry, EntryId, EntryOptions, EntryTree, PackageResolver, Patch, ResolvedPackage, RuntimeKind,
};
use tessivum_node_bridge::{ClientConfig, FrameKind, HostCommand};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

struct FixedResolver(PathBuf);

impl PackageResolver for FixedResolver {
    fn resolve<'a>(
        &'a self,
        specifier: &'a str,
        _runtime: RuntimeKind,
    ) -> tessivum_core::LoaderFuture<'a, ResolvedPackage> {
        let location = self.0.to_string_lossy().into_owned();
        Box::pin(async move {
            Ok(ResolvedPackage {
                specifier: specifier.into(),
                location,
            })
        })
    }
}

fn bridge_services(tools: ToolRuntime) -> BridgeServices {
    let prompt = SystemPrompt::new();
    let llm = LlmRuntime::new();
    let sessions = SessionStore::new(Arc::new(MemorySessionPersistence::new()));
    let agents = AgentRegistry::new(sessions.clone());
    BridgeServices::new(tools, prompt, llm, sessions, agents)
}

fn entry(package: &Path, config: Value) -> Entry {
    Entry {
        package: package.to_string_lossy().into_owned(),
        options: EntryOptions {
            id: EntryId::new("legacy").expect("stable fixture id"),
            name: Some("legacy".into()),
            runtime: RuntimeKind::LegacyNode,
            config,
            inject: Vec::new(),
            isolate: Vec::new(),
            intercept: json!({}),
            disabled: false,
            group: None,
        },
    }
}

fn fixture_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "tessivum-legacy-profile-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(&root).expect("fixture root is writable");
    root
}

fn write_host(root: &Path, child_marker: &Path) -> PathBuf {
    let path = root.join("framed-host.py");
    let marker = serde_json::to_string(&child_marker.to_string_lossy()).expect("marker serializes");
    let source = r#"
import json
import os
import struct
import subprocess
import sys
import time

CHILD_MARKER = __CHILD_MARKER__
plugins = {}
generation = None
provided = False

def read_frame():
    prefix = sys.stdin.buffer.read(4)
    if len(prefix) != 4:
        return None
    length = struct.unpack('>I', prefix)[0]
    body = sys.stdin.buffer.read(length)
    if len(body) != length:
        return None
    return json.loads(body)

def send(kind, payload, request_id=None):
    frame = {
        'protocolVersion': 'cordis.node/v1',
        'connectionGeneration': generation,
        'kind': kind,
        'payload': payload,
    }
    if request_id is not None:
        frame['requestId'] = request_id
    data = json.dumps(frame, separators=(',', ':')).encode()
    sys.stdout.buffer.write(struct.pack('>I', len(data)) + data)
    sys.stdout.buffer.flush()

def respond(request, payload):
    send('response', payload, request['requestId'])

def reject(request, code, message):
    send('error', {'code': code, 'message': message}, request['requestId'])

while True:
    request = read_frame()
    if request is None:
        break
    kind = request['kind']
    if kind == 'hello':
        generation = request['connectionGeneration']
        send('ready', {'scripted': True})
        continue
    if kind == 'heartbeat':
        send('heartbeat', {'ok': True})
        continue
    if kind == 'plugin.load':
        payload = request['payload']
        plugin_id = payload['pluginId']
        config = payload.get('entry', {}).get('config', payload.get('config', {}))
        if config.get('crash'):
            os._exit(23)
        if config.get('reject'):
            reject(request, 'REJECTED_CONFIG', 'scripted update failure')
            continue
        if config.get('provideTool') and not provided:
            provided = True
            send('service.provide', {
                'service': 'tools@1',
                'method': 'register',
                'params': {
                    'registrationId': 'tool-1',
                    'callbackId': 'cb-1',
                    'name': 'legacy-tool',
                    'description': 'legacy',
                    'parameters': {'type': 'object', 'properties': {}},
                },
            }, 900)
            response = read_frame()
            if response is None or response.get('kind') != 'response' or response.get('requestId') != 900:
                os._exit(24)
        if config.get('spawnChild'):
            subprocess.Popen([
                sys.executable,
                '-c',
                "from pathlib import Path; import os, time; Path(" + repr(CHILD_MARKER) + ").write_text(str(os.getpid())); time.sleep(120)",
            ])
        plugins[plugin_id] = config
        respond(request, {'pluginId': plugin_id, 'state': 'ACTIVE'})
        continue
    if kind == 'plugin.update':
        plugin_id = request['payload']['pluginId']
        config = request['payload']['config']
        if plugin_id not in plugins:
            reject(request, 'UNKNOWN_PLUGIN', plugin_id)
        elif config.get('reject'):
            reject(request, 'REJECTED_CONFIG', 'scripted update failure')
        else:
            plugins[plugin_id] = config
            respond(request, {'updated': True})
        continue
    if kind == 'plugin.snapshot':
        plugin_id = request['payload']['pluginId']
        if plugin_id not in plugins:
            reject(request, 'UNKNOWN_PLUGIN', plugin_id)
        else:
            respond(request, {'pluginId': plugin_id, 'state': 'ACTIVE', 'config': plugins[plugin_id]})
        continue
    if kind == 'plugin.dispose':
        plugins.pop(request['payload']['pluginId'], None)
        respond(request, {'disposed': True})
        continue
    if kind == 'exit':
        respond(request, {'drained': True})
        break
    if kind == 'cancel':
        continue
    respond(request, {})
"#
    .replace("__CHILD_MARKER__", &marker);
    fs::write(&path, source).expect("scripted host is writable");
    path
}

async fn wait_until(label: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {label}");
}
async fn wait_for_pid(path: &Path) -> u32 {
    for _ in 0..200 {
        if let Ok(pid) = fs::read_to_string(path).and_then(|value| {
            value
                .parse()
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        }) {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for child pid");
}

#[cfg(unix)]
async fn assert_process_gone(pid: u32) {
    for _ in 0..200 {
        if unsafe { libc::kill(pid as i32, 0) } != 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("process tree child {pid} survived shutdown");
}

#[cfg(not(unix))]
async fn assert_process_gone(_: u32) {}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_cleans_generation_restarts_cleanly_and_shutdown_reaps_tree() {
    let root = fixture_root();
    let child_marker = root.join("child.pid");
    let host = write_host(&root, &child_marker);
    let package = root.join("plugin.js");
    fs::write(&package, "export default () => {}\n").expect("fixture package is writable");

    let tools = ToolRuntime::new();
    let profile = LegacyProfile::new(
        HostCommand::new("python3").arg(host),
        ClientConfig::default(),
        bridge_services(tools.clone()),
    )
    .expect("validated scripted host profile");
    profile.start().expect("hello/ready completes");
    let first_generation = profile
        .snapshot()
        .generation
        .expect("host generation exists");

    let mut loader = legacy_loader(&profile, Arc::new(FixedResolver(package.clone())))
        .expect("legacy runtime is registered with Loader");
    let original = EntryTree {
        entries: vec![entry(
            &package,
            json!({"provideTool": true, "spawnChild": true}),
        )],
        groups: Vec::new(),
    };
    loader
        .load(original.clone())
        .await
        .expect("scripted plugin loads");
    assert_eq!(tools.schemas().len(), 1, "Node tool proxy is live");
    let first_child = wait_for_pid(&child_marker).await;
    assert!(
        profile
            .runtime()
            .expect("live runtime")
            .client()
            .request(
                FrameKind::PluginUpdate,
                json!({"pluginId": "legacy", "config": {"reject": true}}),
                Duration::from_secs(1),
            )
            .is_err(),
        "failed host update preserves the previous plugin state"
    );
    assert_eq!(
        profile
            .runtime()
            .expect("live runtime")
            .client()
            .request(
                FrameKind::PluginSnapshot,
                json!({"pluginId": "legacy"}),
                Duration::from_secs(1),
            )
            .expect("previous host state remains available")["config"]["provideTool"],
        true
    );

    let rejected = loader
        .update(&[Patch::UpdateConfig {
            id: EntryId::new("legacy").expect("stable fixture id"),
            config: json!({"reject": true}),
        }])
        .await;
    assert!(rejected.is_err(), "failed config candidate rolls back");
    assert_eq!(
        loader.tree(),
        &original,
        "old configuration remains committed"
    );
    assert_eq!(
        profile
            .runtime()
            .expect("old runtime remains live after rollback")
            .client()
            .request(
                FrameKind::PluginSnapshot,
                json!({"pluginId": "legacy"}),
                Duration::from_secs(1),
            )
            .expect("old plugin remains active")["state"],
        "ACTIVE"
    );

    let old_client = profile.runtime().expect("current runtime").client();
    assert!(old_client
        .request(
            FrameKind::PluginLoad,
            json!({"pluginId": "crash", "config": {"crash": true}}),
            Duration::from_secs(1),
        )
        .is_err());
    wait_until("generation cleanup", || {
        profile.health() == LegacyProfileHealth::Stopped
    })
    .await;
    assert_eq!(profile.health(), LegacyProfileHealth::Stopped);
    assert!(profile.runtime().is_err(), "stale runtime is rejected");
    assert!(
        tools.schemas().is_empty(),
        "crash removed Node-owned tool proxy"
    );
    assert_process_gone(first_child).await;
    fs::remove_file(&child_marker).expect("first child marker removal");

    profile.restart().await.expect("cleaned profile restarts");
    let restarted = profile.runtime().expect("new runtime is ready");
    assert_ne!(
        restarted.client().generation(),
        first_generation,
        "restart allocates a new generation"
    );
    assert!(
        restarted
            .client()
            .request(
                FrameKind::PluginSnapshot,
                json!({"pluginId": "legacy"}),
                Duration::from_secs(1),
            )
            .is_err(),
        "restarted host has no inherited plugin tree"
    );

    let mut fresh_loader = legacy_loader(&profile, Arc::new(FixedResolver(package)))
        .expect("new generation registers with Loader");
    fresh_loader
        .load(original)
        .await
        .expect("Loader reconstructs the profile tree");
    let second_child = wait_for_pid(&child_marker).await;
    assert_ne!(second_child, first_child, "restart spawns a distinct child");
    fresh_loader.unload().await.expect("plugin disposes");
    profile.shutdown().await.expect("shutdown drains host");
    assert_process_gone(second_child).await;
    fs::remove_dir_all(root).expect("fixture cleanup");
}

#[tokio::test]
async fn product_resolver_routes_npm_defaults_to_legacy_and_confines_location() {
    let root = fixture_root();
    let package = root.join("community-plugin");
    fs::create_dir_all(&package).expect("package directory");
    fs::write(
        package.join("package.json"),
        r#"{"name":"community-plugin","version":"1.0.0","main":"index.js"}"#,
    )
    .expect("package manifest");
    fs::write(package.join("index.js"), "export default () => {}\n").expect("package entry");

    let resolver = ProductPackageResolver::new()
        .confine_to(&root)
        .expect("package root is valid");
    let report = resolver
        .inspect(&package, None)
        .expect("package is analyzed");
    assert_eq!(
        report.selected_runtime,
        tessivum::plugins::PluginRuntime::LegacyNode
    );
    let resolved = resolver
        .resolve(&package.to_string_lossy(), RuntimeKind::LegacyNode)
        .await
        .expect("legacy package resolves");
    let canonical_root = fs::canonicalize(&root).expect("canonical fixture root");
    assert!(Path::new(&resolved.location).is_absolute());
    assert!(Path::new(&resolved.location).starts_with(&canonical_root));

    let outside = fixture_root();
    fs::write(outside.join("index.js"), "export default () => {}\n").expect("outside entry");
    fs::write(
        outside.join("package.json"),
        r#"{"name":"outside","version":"1.0.0","main":"index.js"}"#,
    )
    .expect("outside manifest");
    assert!(
        resolver
            .resolve(&outside.to_string_lossy(), RuntimeKind::LegacyNode)
            .await
            .is_err(),
        "canonical outside package is rejected"
    );
    fs::remove_dir_all(root).expect("fixture cleanup");
    fs::remove_dir_all(outside).expect("fixture cleanup");
}

use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use futures_util::StreamExt;
use serde_json::{json, Value};
use tessivum::{
    api::{ApiServer, ApiServerConfig},
    frontend::FrontendStatic,
    host::{HostApi, HostConfig, HostRuntime},
    plugin_manager::configure_host_plugins,
};
#[cfg(unix)]
use tessivum::plugin_manager::{mutate_plugins, PluginMutation};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::{Child, Command},
    time::{sleep, timeout},
};
use uuid::Uuid;

const REPLAY: &str = include_str!("../fixtures/headless/recorded-replay.jsonl");
const REPLAY_FACT: &str = "CLI tool round trip complete: CLI_TOOL_ROUND_TRIP";
const WEB_REPLAY: &str = concat!(
    "{\"requestId\":\"provider-request-1\",\"chunk\":{\"type\":\"block-start\",\"index\":0,\"blockType\":\"text\"}}\n",
    "{\"requestId\":\"provider-request-1\",\"chunk\":{\"type\":\"text-delta\",\"index\":0,\"text\":\"CLI tool round trip complete: CLI_TOOL_ROUND_TRIP\"}}\n",
    "{\"requestId\":\"provider-request-1\",\"chunk\":{\"type\":\"block-end\",\"index\":0,\"block\":{\"type\":\"text\",\"text\":\"CLI tool round trip complete: CLI_TOOL_ROUND_TRIP\"}}}\n",
    "{\"requestId\":\"provider-request-1\",\"chunk\":{\"type\":\"usage\",\"usage\":{\"inputTokens\":7,\"outputTokens\":5}}}\n",
    "{\"requestId\":\"provider-request-1\",\"chunk\":{\"type\":\"finish\",\"reason\":{\"kind\":\"stop\"}}}\n",
);

struct Fixture(PathBuf);

impl Fixture {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("tessivum-web-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).expect("fixture directory creates");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: impl AsRef<Path>, contents: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file has parent"))
            .expect("fixture parent creates");
        fs::write(path, contents).expect("fixture file writes");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn install_web_half(fixture: &Fixture) -> (PathBuf, PathBuf) {
    fixture.write(
        "dist/index.html",
        "<!doctype html><html><head><meta name=\"existing\"></head><body><div id=\"root\"></div></body></html>",
    );
    fixture.write(
        "packages/clock/package.json",
        r#"{"name":"@fixture/clock","exports":{"./client":"./dist/client.js"},"dsh":{"client":{"platform":"web","inject":["logger","<Clock>"],"immediately":true}}}"#,
    );
    fixture.write(
        "packages/clock/dist/client.js",
        "export const revision = 1;",
    );
    (fixture.path().join("dist"), fixture.path().join("packages"))
}

fn install_dream_skin_profile(fixture: &Fixture) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/community/dream-skin");
    let package = Path::new("data/plugins/node_modules/dsh-dream-skin");
    fixture.write(
        "data/plugins/package.json",
        r#"{"private":true,"dependencies":{"dsh-dream-skin":"8.30.1"},"dsh":{"profile":{"bundles":["dsh-dream-skin"]}}}"#,
    );
    for relative in [
        "package.json",
        "cordis.patch.yml",
        "lib/index.js",
        "lib/client.js",
    ] {
        fixture.write(
            package.join(relative),
            &fs::read_to_string(source.join(relative)).expect("dream skin fixture is readable"),
        );
    }
    fixture.path().join("data/plugins")
}

fn boot_graph(html: &str) -> Value {
    const PREFIX: &str = "<script>window.__DSH_BOOT__=";
    let start = html.find(PREFIX).expect("index has boot script") + PREFIX.len();
    let end = start
        + html[start..]
            .find(";</script>")
            .expect("boot script closes");
    serde_json::from_str(&html[start..end]).expect("boot graph is JSON")
}

async fn rpc(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    request_id: &str,
    args: Value,
) -> Value {
    let response = client
        .post(format!("{base}/api/{method}"))
        .json(&json!({"requestId": request_id, "args": args}))
        .send()
        .await
        .expect("API responds");
    let status = response.status();
    let body: Value = response.json().await.expect("API response is JSON");
    assert_eq!(status, reqwest::StatusCode::OK, "{method} failed: {body}");
    body
}

async fn browser_rpc(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    rpc_id: &str,
    payload: Value,
) -> Value {
    client
        .post(format!("{base}/api/{method}"))
        .json(&json!({
            "type": "client-request",
            "rpcId": rpc_id,
            "method": method,
            "payload": payload,
        }))
        .send()
        .await
        .expect("browser API responds")
        .json()
        .await
        .expect("browser API response is JSON")
}

async fn host_events(address: SocketAddr) -> BufReader<TcpStream> {
    let stream = TcpStream::connect(address)
        .await
        .expect("host events connect");
    let mut stream = BufReader::new(stream);
    let authority = address.to_string();
    let request = format!(
        "GET /api/events.host HTTP/1.1\r\nHost: {authority}\r\nOrigin: http://{authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream
        .get_mut()
        .write_all(request.as_bytes())
        .await
        .expect("host event handshake writes");
    stream
        .get_mut()
        .flush()
        .await
        .expect("host event handshake flushes");
    let mut status = String::new();
    stream
        .read_line(&mut status)
        .await
        .expect("host event status reads");
    assert!(status.contains(" 101 "), "host events upgrade: {status}");
    loop {
        let mut header = Vec::new();
        stream
            .read_until(b'\n', &mut header)
            .await
            .expect("host event header reads");
        if header == b"\r\n" {
            return stream;
        }
    }
}

async fn websocket_text(stream: &mut BufReader<TcpStream>) -> String {
    let opcode = stream.read_u8().await.expect("host event opcode") & 0x0f;
    let length = stream.read_u8().await.expect("host event length");
    assert_eq!(length & 0x80, 0, "server frames are unmasked");
    let length = match length & 0x7f {
        value @ 0..=125 => value as usize,
        126 => stream.read_u16().await.expect("host event extended length") as usize,
        127 => stream.read_u64().await.expect("host event long length") as usize,
        _ => unreachable!(),
    };
    let mut payload = vec![0; length];
    stream
        .read_exact(&mut payload)
        .await
        .expect("host event payload reads");
    assert_eq!(opcode, 0x1, "host event is text");
    String::from_utf8(payload).expect("host event is UTF-8")
}

async fn replay_events(client: &reqwest::Client, base: &str, session: &str) -> Value {
    timeout(Duration::from_secs(5), async {
        loop {
            let events = rpc(
                client,
                base,
                "session/events",
                "events",
                json!({"session": session, "fromSeq": 0}),
            )
            .await;
            if events["output"].to_string().contains(REPLAY_FACT) {
                return events;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("recorded tool presentation completes")
}

async fn idle_status(client: &reqwest::Client, base: &str, session: &str) {
    timeout(Duration::from_secs(5), async {
        loop {
            let status = rpc(
                client,
                base,
                "session/status",
                "status",
                json!({"session": session}),
            )
            .await;
            if status["output"] == "idle" {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("recorded session becomes idle");
}

fn host_config(root: &Fixture) -> HostConfig {
    let mut config =
        HostConfig::new(root.path(), root.path().join("data")).with_recorded_replay(REPLAY);
    config.enable_trusted_bash = true;
    config
}

struct ChildCleanup(Option<Child>);

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.start_kill();
        }
    }
}

#[test]
fn web_command_rejects_a_missing_distribution_before_host_boot() {
    let fixture = Fixture::new("missing-dist");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tessivum"))
        .current_dir(fixture.path())
        .env("TESSIVUM_WEB_DIST", fixture.path().join("missing-dist"))
        .arg("web")
        .output()
        .expect("web command starts");

    assert!(
        !output.status.success(),
        "missing frontend must fail web mode"
    );
    let stderr = String::from_utf8(output.stderr).expect("diagnostic is UTF-8");
    assert!(stderr.contains("WEB_FRONTEND_FAILED"));
    assert!(stderr.contains("distribution"));
    assert!(stderr.contains("missing-dist"));
    assert!(
        !fixture.path().join(".tessivum").exists(),
        "frontend activation fails before durable host state is created"
    );
}

#[tokio::test]
async fn web_command_combines_explicit_enabled_and_client_only_packages() {
    let fixture = Fixture::new("command");
    let (dist, packages) = install_web_half(&fixture);
    let data_dir = fixture.path().join("state");
    fixture.write(
        "state/plugins/package.json",
        r#"{"private":true,"dependencies":{"community-clock":"1.0.0","disabled-clock":"1.0.0","client-only":"1.0.0","dependency-only":"1.0.0"},"dsh":{"profile":{"bundles":["community-clock","disabled-clock"]}}}"#,
    );
    fixture.write(
        "state/plugins/.dsh-market/state.json",
        r#"{"disabled":["disabled-clock"]}"#,
    );
    fixture.write(
        "state/plugins/node_modules/community-clock/package.json",
        r#"{"name":"community-clock","exports":{"./client":"./dist/client.js"},"dsh":{"bundle":{"patch":"./cordis.patch.yml"},"client":{"platform":"web"}}}"#,
    );
    fixture.write(
        "state/plugins/node_modules/community-clock/cordis.patch.yml",
        "[]\n",
    );
    fixture.write(
        "state/plugins/node_modules/community-clock/dist/client.js",
        "export const community = true;",
    );
    fixture.write(
        "state/plugins/node_modules/disabled-clock/package.json",
        r#"{"name":"disabled-clock","exports":{"./client":"./dist/client.js"},"dsh":{"bundle":{"patch":"./cordis.patch.yml"},"client":{"platform":"web"}}}"#,
    );
    fixture.write(
        "state/plugins/node_modules/disabled-clock/cordis.patch.yml",
        "[]\n",
    );
    fixture.write(
        "state/plugins/node_modules/disabled-clock/dist/client.js",
        "export const disabled = true;",
    );
    fixture.write(
        "state/plugins/node_modules/client-only/package.json",
        r#"{"name":"client-only","exports":{"./client":"./dist/client.js"},"dsh":{"client":{"platform":"web"}}}"#,
    );
    fixture.write(
        "state/plugins/node_modules/client-only/dist/client.js",
        "export const clientOnly = true;",
    );
    fixture.write(
        "state/plugins/node_modules/dependency-only/package.json",
        r#"{"name":"dependency-only"}"#,
    );
    let package_paths = std::env::join_paths([packages]).expect("package path list encodes");
    let mut child = ChildCleanup(Some(
        Command::new(env!("CARGO_BIN_EXE_tessivum"))
            .current_dir(fixture.path())
            .env("TESSIVUM_WEB_DIST", dist)
            .env("TESSIVUM_CLIENT_PACKAGES", package_paths)
            .env("TESSIVUM_REPLAY", WEB_REPLAY)
            .env("TESSIVUM_WEB_ADDR", "127.0.0.1:0")
            .arg("web")
            .arg("--data-dir")
            .arg(&data_dir)
            .stderr(Stdio::piped())
            .spawn()
            .expect("web command starts"),
    ));
    let stderr = child
        .0
        .as_mut()
        .expect("child exists")
        .stderr
        .take()
        .expect("stderr is piped");
    let mut lines = BufReader::new(stderr).lines();
    let listen = timeout(Duration::from_secs(5), async {
        loop {
            let line = lines
                .next_line()
                .await
                .expect("web stderr reads")
                .expect("web command stays alive");
            if line.starts_with("Tessivum web listening at http://") {
                return line;
            }
        }
    })
    .await
    .expect("web command logs its bound URL");
    let base = listen
        .strip_prefix("Tessivum web listening at ")
        .expect("bound URL prefix")
        .to_owned();
    drop(lines);

    let client = reqwest::Client::new();
    let index = client
        .get(&base)
        .send()
        .await
        .expect("index is served")
        .text()
        .await
        .expect("index is text");
    let graph = boot_graph(&index);
    let entries = graph["entries"].as_array().expect("boot entries");
    let clock = entries
        .iter()
        .find(|entry| entry["id"] == "@fixture/clock")
        .expect("explicit client package");
    let rev = clock["rev"].as_str().expect("plugin revision");
    assert_eq!(
        clock["url"],
        format!("/plugins/@fixture/clock/client.js?rev={rev}")
    );
    assert_eq!(
        client
            .get(format!("{base}/plugins/@fixture/clock/client.js?rev={rev}"))
            .send()
            .await
            .expect("plugin is served")
            .text()
            .await
            .expect("plugin is text"),
        "export const revision = 1;"
    );
    let client_only = entries
        .iter()
        .find(|entry| entry["id"] == "client-only")
        .expect("true client-only browser root");
    assert!(
        !entries.iter().any(|entry| entry["id"] == "disabled-clock"),
        "disabled bundle clients stay out of the browser graph"
    );
    assert!(
        !entries.iter().any(|entry| entry["id"] == "dependency-only"),
        "dependency-only packages do not leak into the browser graph"
    );
    let community = entries
        .iter()
        .find(|entry| entry["id"] == "community-clock")
        .expect("installed community client package");
    assert_eq!(
        client
            .get(format!("{base}{}", community["url"].as_str().unwrap()))
            .send()
            .await
            .expect("community plugin is served")
            .text()
            .await
            .expect("community plugin is text"),
        "export const community = true;"
    );
    assert_eq!(
        client
            .get(format!("{base}{}", client_only["url"].as_str().unwrap()))
            .send()
            .await
            .expect("client-only plugin is served")
            .text()
            .await
            .expect("client-only plugin is text"),
        "export const clientOnly = true;"
    );
    assert!(data_dir.join("modes").is_dir());
    assert!(!data_dir.join(".agent-presets").exists());
    assert!(!fixture.path().join(".tessivum").exists());

    let cwd = fixture.path().to_string_lossy().into_owned();
    let initialized = rpc(
        &client,
        &base,
        "session/initialize",
        "initialize",
        json!({"cwd": cwd, "provider": "recorded", "model": "recorded"}),
    )
    .await;
    assert!(initialized["ok"].as_bool().unwrap_or(false));
    let prompted = rpc(
        &client,
        &base,
        "session/prompt",
        "prompt",
        json!({
            "sessionId": "web-replay",
            "contentBlocks": [{"type": "text", "text": "present the recorded tool result"}]
        }),
    )
    .await;
    assert!(prompted["output"]["messageId"].is_string());
    let replay = replay_events(&client, &base, "web-replay").await;
    assert!(replay["output"].to_string().contains(REPLAY_FACT));
    idle_status(&client, &base, "web-replay").await;

    #[cfg(unix)]
    unsafe {
        assert_eq!(
            libc::kill(
                child
                    .0
                    .as_ref()
                    .expect("child exists")
                    .id()
                    .expect("child has PID") as i32,
                libc::SIGTERM,
            ),
            0,
            "SIGTERM is delivered"
        );
    }
    #[cfg(not(unix))]
    child
        .0
        .as_mut()
        .expect("child exists")
        .start_kill()
        .expect("web command stops");
    let status = timeout(
        Duration::from_secs(5),
        child.0.as_mut().expect("child exists").wait(),
    )
    .await
    .expect("web command stops")
    .expect("web command waits");
    #[cfg(unix)]
    assert!(status.success(), "SIGTERM is graceful");
    #[cfg(not(unix))]
    assert!(
        !status.success(),
        "forced process termination reports failure"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn failed_plugin_add_keeps_the_last_good_profile_bootable_by_web() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new("failed-plugin-add-web-boot");
    let (dist, packages) = install_web_half(&fixture);
    let data_dir = fixture.path().join("state");
    let profile = data_dir.join("plugins");
    fixture.write(
        "state/plugins/package.json",
        r#"{"private":true,"dependencies":{"old-plugin":"1.0.0"},"dsh":{"profile":{"bundles":["old-plugin"]}}}"#,
    );
    fixture.write("state/plugins/pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
    fixture.write(
        "state/plugins/node_modules/old-plugin/package.json",
        r#"{"name":"old-plugin","exports":{"./client":"./dist/client.js"},"dsh":{"bundle":{"patch":"./cordis.patch.yml"},"client":{"platform":"web"}}}"#,
    );
    fixture.write("state/plugins/node_modules/old-plugin/cordis.patch.yml", "[]\n");
    fixture.write(
        "state/plugins/node_modules/old-plugin/dist/client.js",
        "export const oldPlugin = true;",
    );
    let manifest = fs::read(profile.join("package.json")).expect("last-good manifest reads");
    let lock = fs::read(profile.join("pnpm-lock.yaml")).expect("last-good lock reads");
    let old_manifest = fs::read(profile.join("node_modules/old-plugin/package.json"))
        .expect("last-good plugin manifest reads");
    let old_patch = fs::read(profile.join("node_modules/old-plugin/cordis.patch.yml"))
        .expect("last-good plugin patch reads");
    let old_client = fs::read(profile.join("node_modules/old-plugin/dist/client.js"))
        .expect("last-good plugin client reads");

    let fake_bin = fixture.path().join("fake-pnpm");
    let backup = fake_bin.join("last-known-good/node_modules/old-plugin");
    fs::create_dir_all(backup.join("dist")).expect("fake pnpm backup creates");
    fs::copy(
        profile.join("node_modules/old-plugin/package.json"),
        backup.join("package.json"),
    )
    .expect("fake pnpm saves old manifest");
    fs::copy(
        profile.join("node_modules/old-plugin/cordis.patch.yml"),
        backup.join("cordis.patch.yml"),
    )
    .expect("fake pnpm saves old patch");
    fs::copy(
        profile.join("node_modules/old-plugin/dist/client.js"),
        backup.join("dist/client.js"),
    )
    .expect("fake pnpm saves old client");
    let pnpm = fake_bin.join("pnpm");
    fixture.write(
        "fake-pnpm/pnpm",
        r#"#!/bin/sh
case "$1" in
  add)
    printf '{"private":true,"dependencies":{"candidate":"1.0.0"}}' > "$PWD/package.json"
    printf "lockfileVersion: '9.0'\npackages: {}\n" > "$PWD/pnpm-lock.yaml"
    rm -rf "$PWD/node_modules"
    mkdir -p "$PWD/node_modules/candidate"
    printf '{"name":"candidate","version":"1.0.0"}' > "$PWD/node_modules/candidate/package.json"
    exit 23
    ;;
  install)
    rm -rf "$PWD/node_modules"
    cp -R "$(dirname "$0")/last-known-good/node_modules" "$PWD/node_modules"
    ;;
esac
"#,
    );
    fs::set_permissions(&pnpm, fs::Permissions::from_mode(0o755))
        .expect("fake pnpm is executable");

    let previous_path = std::env::var_os("PATH");
    let mut path = fake_bin.into_os_string();
    if let Some(previous) = &previous_path {
        path.push(":");
        path.push(previous);
    }
    std::env::set_var("PATH", path);
    let mutation = mutate_plugins(&data_dir, PluginMutation::Add("candidate@1.0.0".into()));
    match previous_path {
        Some(previous) => std::env::set_var("PATH", previous),
        None => std::env::remove_var("PATH"),
    }
    let error = mutation.expect_err("fake pnpm add fails");
    assert!(error.to_string().contains("pnpm exited with code 23"));
    assert_eq!(
        fs::read(profile.join("package.json")).expect("restored manifest reads"),
        manifest
    );
    assert_eq!(
        fs::read(profile.join("pnpm-lock.yaml")).expect("restored lock reads"),
        lock
    );
    assert_eq!(
        fs::read(profile.join("node_modules/old-plugin/package.json"))
            .expect("restored plugin manifest reads"),
        old_manifest
    );
    assert_eq!(
        fs::read(profile.join("node_modules/old-plugin/cordis.patch.yml"))
            .expect("restored plugin patch reads"),
        old_patch
    );
    assert_eq!(
        fs::read(profile.join("node_modules/old-plugin/dist/client.js"))
            .expect("restored plugin client reads"),
        old_client
    );
    assert!(
        !profile.join("node_modules/candidate").exists(),
        "failed plugin is removed during rollback"
    );

    let package_paths = std::env::join_paths([packages]).expect("package path list encodes");
    let mut child = ChildCleanup(Some(
        Command::new(env!("CARGO_BIN_EXE_tessivum"))
            .current_dir(fixture.path())
            .env("TESSIVUM_WEB_DIST", dist)
            .env("TESSIVUM_CLIENT_PACKAGES", package_paths)
            .env("TESSIVUM_REPLAY", WEB_REPLAY)
            .env("TESSIVUM_WEB_ADDR", "127.0.0.1:0")
            .arg("web")
            .arg("--data-dir")
            .arg(&data_dir)
            .stderr(Stdio::piped())
            .spawn()
            .expect("web command starts"),
    ));
    let stderr = child
        .0
        .as_mut()
        .expect("child exists")
        .stderr
        .take()
        .expect("stderr is piped");
    let mut lines = BufReader::new(stderr).lines();
    let listen = timeout(Duration::from_secs(5), async {
        loop {
            let line = lines
                .next_line()
                .await
                .expect("web stderr reads")
                .expect("web command stays alive");
            if line.starts_with("Tessivum web listening at http://") {
                return line;
            }
        }
    })
    .await
    .expect("web command logs its bound URL");
    let base = listen
        .strip_prefix("Tessivum web listening at ")
        .expect("bound URL prefix")
        .to_owned();
    drop(lines);

    let index = reqwest::Client::new()
        .get(&base)
        .send()
        .await
        .expect("index is served");
    assert_eq!(index.status(), reqwest::StatusCode::OK);
    let graph = boot_graph(&index.text().await.expect("index is text"));
    assert!(
        graph["entries"]
            .as_array()
            .expect("boot entries")
            .iter()
            .any(|entry| entry["id"] == "old-plugin"),
        "web boot keeps the last-known-good plugin active"
    );

    unsafe {
        assert_eq!(
            libc::kill(
                child
                    .0
                    .as_ref()
                    .expect("child exists")
                    .id()
                    .expect("child has PID") as i32,
                libc::SIGTERM,
            ),
            0,
            "SIGTERM is delivered"
        );
    }
    let status = timeout(
        Duration::from_secs(5),
        child.0.as_mut().expect("child exists").wait(),
    )
    .await
    .expect("web command stops")
    .expect("web command waits");
    assert!(status.success(), "SIGTERM is graceful");
}

#[tokio::test]
async fn web_environment_route_remains_dynamic_after_settings_mutation() {
    let fixture = Fixture::new("environment-route");
    let (dist, packages) = install_web_half(&fixture);
    let package_paths = std::env::join_paths([packages]).expect("package path list encodes");
    let mut child = ChildCleanup(Some(
        Command::new(env!("CARGO_BIN_EXE_tessivum"))
            .current_dir(fixture.path())
            .env("TESSIVUM_WEB_DIST", dist)
            .env("TESSIVUM_CLIENT_PACKAGES", package_paths)
            .env("TESSIVUM_WEB_ADDR", "127.0.0.1:0")
            .env("OPENAI_MODEL", "environment-model")
            .env("OPENAI_BASE_URL", "http://127.0.0.1:1/v1")
            .env_remove("OPENAI_API_KEY")
            .env_remove("TESSIVUM_REPLAY")
            .env_remove("TESSIVUM_LLM_PROVIDER")
            .arg("web")
            .arg("--data-dir")
            .arg(fixture.path().join("state"))
            .stderr(Stdio::piped())
            .spawn()
            .expect("web command starts"),
    ));
    let stderr = child
        .0
        .as_mut()
        .expect("child exists")
        .stderr
        .take()
        .expect("stderr is piped");
    let mut lines = BufReader::new(stderr).lines();
    let mut startup = Vec::new();
    let listen = timeout(Duration::from_secs(5), async {
        loop {
            let Some(line) = lines.next_line().await.expect("web stderr reads") else {
                panic!("web command exited during startup:\n{}", startup.join("\n"));
            };
            if line.starts_with("Tessivum web listening at http://") {
                return line;
            }
            startup.push(line);
        }
    })
    .await
    .expect("web command logs its bound URL");
    let base = listen
        .strip_prefix("Tessivum web listening at ")
        .expect("bound URL prefix")
        .to_owned();
    drop(lines);

    let client = reqwest::Client::new();
    let providers = browser_rpc(&client, &base, "llm.providers", "providers", json!({})).await;
    assert!(providers["result"]["value"]["providers"]
        .as_array()
        .is_some_and(|providers| providers
            .iter()
            .any(|provider| provider["provider"] == "openai-responses")));
    let address = base
        .strip_prefix("http://")
        .expect("bound URL is HTTP")
        .parse()
        .expect("bound address parses");
    let mut events = host_events(address).await;
    let updated = browser_rpc(
        &client,
        &base,
        "settings.update",
        "settings-update",
        json!({
            "ns": "llm-pi-ai",
            "patch": {
                "providers": {
                    "openai-responses": {
                        "displayName": "Updated environment route",
                        "baseURL": "http://127.0.0.1:2/v1",
                        "apiKeyEnv": "TESSIVUM_UPDATED_ROUTE_KEY",
                        "models": [{"id": "updated-model", "input": ["text", "image"]}]
                    }
                }
            }
        }),
    )
    .await;
    assert_eq!(updated["result"]["ok"], true, "{updated}");
    let models = browser_rpc(&client, &base, "llm.models", "models", json!({})).await;
    assert!(models["result"]["value"]["groups"]
        .as_array()
        .is_some_and(|groups| groups.iter().any(|group| {
            group["id"] == "openai-responses"
                && group["models"]
                    .as_array()
                    .is_some_and(|models| models.iter().any(|model| model["id"] == "updated-model"))
        })));

    let models_changed = timeout(Duration::from_secs(1), async {
        loop {
            let frame: Value = serde_json::from_str(&websocket_text(&mut events).await)
                .expect("host event is JSON");
            if frame["payload"]["type"] == "host/remote-event"
                && frame["payload"]["event"] == "llm/adapters-updated"
            {
                return frame;
            }
        }
    })
    .await
    .expect("committed route invalidation arrives");
    assert_eq!(
        models_changed["payload"],
        json!({"type": "host/remote-event", "event": "llm/adapters-updated", "args": []})
    );

    #[cfg(unix)]
    unsafe {
        assert_eq!(
            libc::kill(
                child
                    .0
                    .as_ref()
                    .expect("child exists")
                    .id()
                    .expect("child has PID") as i32,
                libc::SIGTERM,
            ),
            0,
            "SIGTERM is delivered"
        );
    }
    #[cfg(not(unix))]
    child
        .0
        .as_mut()
        .expect("child exists")
        .start_kill()
        .expect("web command stops");
    let status = timeout(
        Duration::from_secs(5),
        child.0.as_mut().expect("child exists").wait(),
    )
    .await
    .expect("web command stops")
    .expect("web command waits");
    #[cfg(unix)]
    assert!(status.success(), "SIGTERM is graceful");
    #[cfg(not(unix))]
    assert!(
        !status.success(),
        "forced process termination reports failure"
    );
}

#[tokio::test]
async fn real_host_api_keeps_sessions_authoritative_while_static_graphs_update() {
    let fixture = Fixture::new("integration");
    let (dist, packages) = install_web_half(&fixture);
    let frontend = FrontendStatic::new(&dist).expect("distribution is valid");
    let graph = frontend
        .scan_packages([packages.as_path()])
        .expect("client package scan succeeds");
    let config = host_config(&fixture);
    let runtime = HostRuntime::boot(config.clone()).await.expect("host boots");
    let handle = runtime.handle();
    let host: Arc<dyn HostApi> = Arc::new(handle.clone());
    let mut server = ApiServer::bind_with_config(
        host.clone(),
        ApiServerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("loopback address"),
            frontend: Some(frontend.clone()),
        },
    )
    .await
    .expect("API server binds");
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();

    let rebinding_static = client
        .get(&base)
        .header(reqwest::header::HOST, "attacker.example")
        .header(reqwest::header::ORIGIN, "http://attacker.example")
        .send()
        .await
        .expect("rebound static response");
    assert_eq!(
        rebinding_static.status(),
        reqwest::StatusCode::FORBIDDEN,
        "static route rejects a rebinding authority"
    );
    let index_response = client.get(&base).send().await.expect("index response");
    assert_eq!(
        index_response
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok()),
        Some("frame-ancestors 'none'")
    );
    assert_eq!(
        index_response
            .headers()
            .get("x-frame-options")
            .and_then(|value| value.to_str().ok()),
        Some("DENY")
    );
    let index = index_response.text().await.expect("index body");
    let first_head_child = index.find("<head>").expect("head exists") + "<head>".len();
    assert_eq!(
        index.find("<script>window.__DSH_BOOT__="),
        Some(first_head_child),
        "the boot script is the first head child"
    );
    assert!(index.contains("id=\"root\""));
    assert!(index.contains("\\u003cClock>"), "boot JSON escapes HTML");
    let boot = boot_graph(&index);
    assert_eq!(boot["entries"][0]["id"], "@fixture/clock");
    assert_eq!(boot["entries"][0]["inject"], json!(["logger", "<Clock>"]));
    let rev = boot["entries"][0]["rev"].as_str().expect("plugin revision");
    assert_eq!(
        client
            .get(format!("{base}/plugins/@fixture/clock/client.js?rev={rev}"))
            .send()
            .await
            .expect("plugin response")
            .text()
            .await
            .expect("plugin body"),
        "export const revision = 1;"
    );
    assert_eq!(
        client
            .get(format!("{base}/plugins/missing/client.js"))
            .send()
            .await
            .expect("missing plugin response")
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let api_miss = client
        .get(format!("{base}/api"))
        .send()
        .await
        .expect("API boundary response");
    assert_eq!(api_miss.status(), reqwest::StatusCode::NOT_FOUND);
    assert!(
        api_miss
            .text()
            .await
            .expect("API boundary body")
            .contains("METHOD_NOT_FOUND"),
        "API routes never receive the SPA fallback"
    );

    let workspaces = browser_rpc(
        &client,
        &base,
        "workspace.list",
        "initial-workspaces",
        json!({}),
    )
    .await;
    let default_workspace = workspaces["result"]["value"]["items"][0]["workspaceId"]
        .as_str()
        .expect("default workspace id")
        .to_owned();
    let blank = browser_rpc(
        &client,
        &base,
        "session.create",
        "blank-session",
        json!({"workspaceId": default_workspace, "sessionId": "blank-session"}),
    )
    .await;
    assert_eq!(blank["result"]["value"]["sessionId"], "blank-session");
    let cwd = fixture.path().to_string_lossy().into_owned();
    let initialized = rpc(
        &client,
        &base,
        "session/initialize",
        "initialize",
        json!({"cwd": cwd, "provider": "recorded", "model": "recorded"}),
    )
    .await;
    assert_eq!(initialized["ok"], true);
    let prompted = rpc(
        &client,
        &base,
        "session/prompt",
        "prompt",
        json!({
            "sessionId": "web-session",
            "contentBlocks": [{"type": "text", "text": "present the recorded tool result"}]
        }),
    )
    .await;
    assert!(prompted["output"]["messageId"].is_string());

    let sse = client
        .get(format!("{base}/events/web-session?from=0"))
        .send()
        .await
        .expect("SSE response");
    let mut stream = sse.bytes_stream();
    let first_event = timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("SSE replays durable event")
        .expect("SSE stream has data")
        .expect("SSE data is valid");
    assert!(String::from_utf8_lossy(&first_event).contains("event: session.event"));
    drop(stream);

    let events = replay_events(&client, &base, "web-session").await;
    assert!(events["output"].to_string().contains(REPLAY_FACT));
    idle_status(&client, &base, "web-session").await;

    fixture.write(
        "packages/clock/dist/client.js",
        "export const revision = 2;",
    );
    let updated = frontend.rebuild().expect("graph rebuild succeeds");
    assert_ne!(updated.rev, graph.rev);
    let updated_index = client
        .get(&base)
        .send()
        .await
        .expect("updated index response")
        .text()
        .await
        .expect("updated index body");
    assert_eq!(boot_graph(&updated_index)["rev"], updated.rev.as_str());
    assert_eq!(
        client
            .get(format!(
                "{base}/plugins/@fixture/clock/client.js?rev={}",
                updated.entries[0].rev
            ))
            .send()
            .await
            .expect("updated plugin response")
            .text()
            .await
            .expect("updated plugin body"),
        "export const revision = 2;"
    );

    let stopped = rpc(&client, &base, "host/shutdown", "stop", json!({})).await;
    assert_eq!(stopped["ok"], true);
    server.shutdown().await.expect("server drains sockets");
    runtime
        .shutdown()
        .await
        .expect("host shutdown is idempotent");
    assert_eq!(handle.in_flight(), 0, "host has no admitted work");

    let resumed = HostRuntime::boot(config)
        .await
        .expect("durable host resumes");
    let resumed_handle = resumed.handle();
    let resumed_host: Arc<dyn HostApi> = Arc::new(resumed_handle.clone());
    let mut resumed_server = ApiServer::bind_with_config(
        resumed_host,
        ApiServerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("loopback address"),
            frontend: Some(frontend),
        },
    )
    .await
    .expect("resumed API server binds");
    let resumed_base = format!("http://{}", resumed_server.local_addr());
    let browser_sessions = browser_rpc(
        &client,
        &resumed_base,
        "session.list",
        "browser-sessions",
        json!({}),
    )
    .await;
    let session_items = browser_sessions["result"]["value"]["items"]
        .as_array()
        .expect("browser session list");
    assert!(session_items
        .iter()
        .any(|item| item["sessionId"] == "web-session"));
    assert!(session_items
        .iter()
        .any(|item| { item["sessionId"] == "blank-session" && item["blank"] == true }));
    let browser_workspaces = browser_rpc(
        &client,
        &resumed_base,
        "workspace.list",
        "browser-workspaces",
        json!({}),
    )
    .await;
    let workspace_sessions = browser_workspaces["result"]["value"]["items"][0]["sessionIds"]
        .as_array()
        .expect("browser workspace session ids");
    assert!(workspace_sessions.iter().any(|item| item == "web-session"));
    assert!(workspace_sessions
        .iter()
        .any(|item| item == "blank-session"));
    let resumed_events = rpc(
        &client,
        &resumed_base,
        "session/events",
        "resumed-events",
        json!({"session": "web-session", "fromSeq": 0}),
    )
    .await;
    assert!(resumed_events["output"].to_string().contains(REPLAY_FACT));
    idle_status(&client, &resumed_base, "web-session").await;
    resumed_server
        .shutdown()
        .await
        .expect("resumed server drains sockets");
    resumed.shutdown().await.expect("resumed host shuts down");
    assert_eq!(resumed_handle.in_flight(), 0, "resumed host has no tasks");
}

#[tokio::test]
async fn durable_multi_workspace_api_survives_restart_and_preserves_isolation() {
    let fixture = Fixture::new("multi-workspace");
    let first_path = fixture.path().join("workspace-a");
    let second_path = fixture.path().join("workspace-b");
    let duplicate_one = fixture.path().join("one/project");
    let duplicate_two = fixture.path().join("two/project");
    let unregistered = fixture.path().join("unregistered");
    for path in [
        &first_path,
        &second_path,
        &duplicate_one,
        &duplicate_two,
        &unregistered,
    ] {
        fs::create_dir_all(path).unwrap();
    }
    let config = host_config(&fixture);
    let runtime = HostRuntime::boot(config.clone()).await.unwrap();
    let handle = runtime.handle();
    let host: Arc<dyn HostApi> = Arc::new(handle.clone());
    let mut server = ApiServer::bind(host).await.unwrap();
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();

    let initial = browser_rpc(&client, &base, "workspace.list", "initial", json!({})).await;
    assert_eq!(
        initial["result"]["value"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let default_workspace_id = initial["result"]["value"]["items"][0]["workspaceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let create = |rpc_id: &'static str, path: &Path| {
        browser_rpc(
            &client,
            &base,
            "workspace.create",
            rpc_id,
            json!({"path": path.to_string_lossy()}),
        )
    };
    let first = create("create-a", &first_path).await;
    let second = create("create-b", &second_path).await;
    let duplicate_a = create("create-dup-a", &duplicate_one).await;
    let duplicate_b = create("create-dup-b", &duplicate_two).await;
    assert_eq!(
        duplicate_a["result"]["value"]["workspace"]["title"],
        "project"
    );
    assert_eq!(
        duplicate_b["result"]["value"]["workspace"]["title"],
        "project"
    );
    let first_id = first["result"]["value"]["workspace"]["workspaceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let second_id = second["result"]["value"]["workspace"]["workspaceId"]
        .as_str()
        .unwrap()
        .to_owned();

    let renamed = browser_rpc(
        &client,
        &base,
        "workspace.rename",
        "rename-b",
        json!({"workspaceId": second_id, "title": "renamed"}),
    )
    .await;
    assert_eq!(renamed["result"]["value"]["workspace"]["title"], "renamed");
    let conflict = browser_rpc(
        &client,
        &base,
        "workspace.rename",
        "rename-conflict",
        json!({"workspaceId": first_id, "title": "renamed"}),
    )
    .await;
    assert_eq!(
        conflict["result"]["error"],
        json!({
            "code": "workspace-name-conflict",
            "message": "workspace name is already used",
            "details": {"name": "renamed"},
        })
    );
    let blank_rename = browser_rpc(
        &client,
        &base,
        "workspace.rename",
        "rename-blank",
        json!({"workspaceId": first_id, "title": "  "}),
    )
    .await;
    assert_eq!(
        blank_rename["result"]["error"],
        json!({
            "code": "bad-request",
            "message": "title must not be blank",
            "details": {"issues": []},
        })
    );
    let unknown = browser_rpc(
        &client,
        &base,
        "workspace.rename",
        "rename-missing",
        json!({"workspaceId": "missing-workspace", "title": "unused"}),
    )
    .await;
    assert_eq!(
        unknown["result"]["error"],
        json!({
            "code": "workspace-not-found",
            "message": "workspace was not found",
            "details": {"workspaceId": "missing-workspace"},
        })
    );

    for (rpc_id, session_id) in [("session-one", "multi-one"), ("session-two", "multi-two")] {
        let created = browser_rpc(
            &client,
            &base,
            "session.create",
            rpc_id,
            json!({"workspaceId": second_id, "sessionId": session_id}),
        )
        .await;
        assert_eq!(created["result"]["value"]["sessionId"], session_id);
    }
    let outside = browser_rpc(
        &client,
        &base,
        "session.create",
        "session-outside",
        json!({"workspaceId": first_id, "sessionId": "multi-outside"}),
    )
    .await;
    assert_eq!(outside["result"]["ok"], true);
    let invalid_move = browser_rpc(
        &client,
        &base,
        "workspace.insertSessionBefore",
        "session-order-invalid",
        json!({
            "workspaceId": second_id,
            "sessionId": "multi-one",
            "beforeSessionId": "multi-outside",
        }),
    )
    .await;
    assert_eq!(
        invalid_move["result"]["error"],
        json!({
            "code": "workspace-move-invalid",
            "message": "workspace move is invalid",
            "details": {
                "workspaceId": second_id,
                "sessionId": "multi-one",
                "beforeSessionId": "multi-outside",
            },
        })
    );
    let invalid_session = browser_rpc(
        &client,
        &base,
        "workspace.insertSessionBefore",
        "session-outside-invalid",
        json!({"workspaceId": second_id, "sessionId": "multi-outside"}),
    )
    .await;
    assert_eq!(
        invalid_session["result"]["error"],
        json!({
            "code": "workspace-move-invalid",
            "message": "workspace move is invalid",
            "details": {"workspaceId": second_id, "sessionId": "multi-outside"},
        })
    );

    let moved = browser_rpc(
        &client,
        &base,
        "workspace.insertSessionBefore",
        "session-order",
        json!({"workspaceId": second_id, "sessionId": "multi-one", "beforeSessionId": "multi-two"}),
    )
    .await;
    assert_eq!(
        moved["result"]["value"]["workspace"]["sessionIds"],
        json!(["multi-one", "multi-two"])
    );
    let archived = browser_rpc(
        &client,
        &base,
        "workspace.archiveSession",
        "archive",
        json!({"sessionId": "multi-one"}),
    )
    .await;
    assert_eq!(
        archived["result"]["value"]["archivedSessionIds"],
        json!(["multi-one"])
    );
    let invalid_cwd = browser_rpc(
        &client,
        &base,
        "session.create",
        "raw-cwd",
        json!({"cwd": unregistered.to_string_lossy(), "sessionId": "raw-cwd"}),
    )
    .await;
    assert_eq!(
        invalid_cwd["result"]["error"],
        json!({
            "code": "bad-request",
            "message": "session.create requires workspaceId; register paths with workspace.create",
            "details": {"issues": []},
        })
    );
    let deleted = browser_rpc(
        &client,
        &base,
        "workspace.delete",
        "delete-b",
        json!({"workspaceId": second_id}),
    )
    .await;
    assert_eq!(deleted["result"]["value"], json!({"deleted": true}));
    let delete_retry = browser_rpc(
        &client,
        &base,
        "workspace.delete",
        "delete-b-retry",
        json!({"workspaceId": second_id}),
    )
    .await;
    assert_eq!(delete_retry["result"]["value"], json!({"deleted": true}));
    let deleted_default = browser_rpc(
        &client,
        &base,
        "workspace.delete",
        "delete-default",
        json!({"workspaceId": default_workspace_id.clone()}),
    )
    .await;
    assert_eq!(deleted_default["result"]["value"], json!({"deleted": true}));
    let implicit = browser_rpc(
        &client,
        &base,
        "session.create",
        "implicit-without-host-workspace",
        json!({"sessionId": "implicit-without-host-workspace"}),
    )
    .await;
    assert_eq!(
        implicit["result"]["error"],
        json!({
            "code": "workspace-not-found",
            "message": "workspace was not found",
            "details": {"workspaceId": default_workspace_id},
        })
    );

    server.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
    let resumed = HostRuntime::boot(config).await.unwrap();
    let resumed_handle = resumed.handle();
    let resumed_host: Arc<dyn HostApi> = Arc::new(resumed_handle.clone());
    let mut resumed_server = ApiServer::bind(resumed_host).await.unwrap();
    let resumed_base = format!("http://{}", resumed_server.local_addr());
    let workspaces = browser_rpc(
        &client,
        &resumed_base,
        "workspace.list",
        "resumed-workspaces",
        json!({}),
    )
    .await;
    assert!(workspaces["result"]["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|workspace| workspace["workspaceId"] != second_id));
    let sessions = browser_rpc(
        &client,
        &resumed_base,
        "session.list",
        "resumed-sessions",
        json!({}),
    )
    .await;
    assert!(sessions["result"]["value"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|session| session["sessionId"] == "multi-one"));
    resumed_server.shutdown().await.unwrap();
    resumed.shutdown().await.unwrap();
}

#[tokio::test]
async fn dream_skin_profile_routes_persist_through_host_restart() {
    let fixture = Fixture::new("dream-skin");
    let profile = install_dream_skin_profile(&fixture);
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(profile.join("node_modules/dsh-dream-skin/package.json"))
            .expect("dream skin manifest reads"),
    )
    .expect("dream skin manifest is JSON");
    assert_eq!(manifest["name"], "dsh-dream-skin");
    assert_eq!(manifest["version"], "8.30.1");
    assert_eq!(manifest["dsh"]["bundle"]["patch"], "./cordis.patch.yml");

    fixture.write(
        "dist/index.html",
        "<!doctype html><html><head></head><body><div id=\"root\"></div></body></html>",
    );
    let frontend = FrontendStatic::new(fixture.path().join("dist")).expect("distribution is valid");
    let graph = frontend
        .scan_packages([profile.join("node_modules")])
        .expect("dream skin browser package is discovered");
    let client_entry = graph
        .entries
        .iter()
        .find(|entry| entry.id == "dsh-dream-skin")
        .expect("dream skin client is in the boot graph");

    let home = fixture.path().join("dream-skin-home");
    let mut config = host_config(&fixture);
    config.cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    configure_host_plugins(&mut config).expect("dream skin profile configures");
    config.cwd = fixture.path().to_path_buf();
    config
        .legacy_host
        .as_mut()
        .expect("profile creates a Legacy Node host")
        .command
        .env
        .push((
            std::ffi::OsString::from("DSH_HOME"),
            home.clone().into_os_string(),
        ));

    let runtime = HostRuntime::boot(config.clone())
        .await
        .expect("dream skin Host runtime boots");
    let handle = runtime.handle();
    let web_routes = handle
        .web_route_registry()
        .expect("Legacy Node host exposes web routes");
    let mut server = ApiServer::bind_with_web_routes(
        Arc::new(handle),
        ApiServerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("loopback address"),
            frontend: Some(frontend.clone()),
        },
        Vec::new(),
        Some(web_routes),
    )
    .await
    .expect("web server binds");
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();

    let index = client
        .get(&base)
        .send()
        .await
        .expect("boot graph is served")
        .text()
        .await
        .expect("boot graph is text");
    let boot = boot_graph(&index);
    let boot_entry = boot["entries"]
        .as_array()
        .expect("boot entries")
        .iter()
        .find(|entry| entry["id"] == "dsh-dream-skin")
        .expect("dream skin entry is booted");
    assert_eq!(boot_entry["url"], client_entry.url);
    assert_eq!(
        client
            .get(format!("{base}{}", client_entry.url))
            .send()
            .await
            .expect("dream skin client is served")
            .text()
            .await
            .expect("dream skin client is text"),
        "export const dreamSkinClient = 'dsh-dream-skin@8.30.1';\n"
    );

    let state = json!({"skin": "aurora", "wallpaper": "data:image/png;base64,c2tpbg=="});
    assert_eq!(
        client
            .post(format!("{base}/dream-skin/api"))
            .json(&json!({"method": "set", "patch": state.clone()}))
            .send()
            .await
            .expect("dream skin state saves")
            .json::<Value>()
            .await
            .expect("dream skin save response is JSON"),
        json!({"ok": true})
    );
    assert_eq!(
        serde_json::from_str::<Value>(
            &fs::read_to_string(home.join("dream-skin.json")).expect("state reaches DSH_HOME"),
        )
        .expect("saved state is JSON"),
        state
    );
    assert_eq!(
        client
            .post(format!("{base}/dream-skin/api"))
            .json(&json!({"method": "get"}))
            .send()
            .await
            .expect("dream skin state loads")
            .json::<Value>()
            .await
            .expect("dream skin state is JSON"),
        json!({"ok": true, "value": state})
    );
    assert_eq!(
        client
            .post(format!("{base}/dream-skin-evil"))
            .json(&json!({"method": "set", "patch": state.clone()}))
            .send()
            .await
            .expect("unsupported namespace never reaches Legacy route")
            .status(),
        reqwest::StatusCode::METHOD_NOT_ALLOWED
    );

    server.shutdown().await.expect("server drains sockets");
    runtime.shutdown().await.expect("host shuts down");

    let resumed = HostRuntime::boot(config)
        .await
        .expect("dream skin Host runtime restarts");
    let resumed_handle = resumed.handle();
    let resumed_routes = resumed_handle
        .web_route_registry()
        .expect("restarted Legacy Node host exposes web routes");
    let mut resumed_server = ApiServer::bind_with_web_routes(
        Arc::new(resumed_handle),
        ApiServerConfig {
            bind_addr: "127.0.0.1:0".parse().expect("loopback address"),
            frontend: Some(frontend),
        },
        Vec::new(),
        Some(resumed_routes),
    )
    .await
    .expect("restarted web server binds");
    let resumed_base = format!("http://{}", resumed_server.local_addr());
    assert_eq!(
        client
            .post(format!("{resumed_base}/dream-skin/api"))
            .json(&json!({"method": "get"}))
            .send()
            .await
            .expect("persisted dream skin state loads")
            .json::<Value>()
            .await
            .expect("persisted dream skin state is JSON"),
        json!({"ok": true, "value": state})
    );
    resumed_server
        .shutdown()
        .await
        .expect("restarted server drains sockets");
    resumed.shutdown().await.expect("restarted host shuts down");
}

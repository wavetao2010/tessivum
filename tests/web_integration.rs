use std::{
    fs,
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
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
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
        r#"{"name":"clock-package","exports":{"./client":"./dist/client.js"},"dsh":{"client":{"platform":"web","id":"clock","name":"<Clock>","inject":["logger"],"immediately":true}}}"#,
    );
    fixture.write(
        "packages/clock/dist/client.js",
        "export const revision = 1;",
    );
    (fixture.path().join("dist"), fixture.path().join("packages"))
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
async fn web_command_explicit_packages_override_discovery_and_log_the_bound_url() {
    let fixture = Fixture::new("command");
    let (dist, packages) = install_web_half(&fixture);
    let package_paths = std::env::join_paths([packages]).expect("package path list encodes");
    let mut child = ChildCleanup(Some(
        Command::new(env!("CARGO_BIN_EXE_tessivum"))
            .current_dir(fixture.path())
            .env("TESSIVUM_WEB_DIST", dist)
            .env("TESSIVUM_CLIENT_PACKAGES", package_paths)
            .env("TESSIVUM_REPLAY", WEB_REPLAY)
            .env("TESSIVUM_WEB_ADDR", "127.0.0.1:0")
            .arg("web")
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
    let rev = graph["entries"][0]["rev"]
        .as_str()
        .expect("plugin revision")
        .to_owned();
    assert_eq!(graph["entries"][0]["url"], "/plugins/clock/client.js");
    assert_eq!(
        client
            .get(format!("{base}/plugins/clock/client.js?rev={rev}"))
            .send()
            .await
            .expect("plugin is served")
            .text()
            .await
            .expect("plugin is text"),
        "export const revision = 1;"
    );

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
    let index = client
        .get(&base)
        .send()
        .await
        .expect("index response")
        .text()
        .await
        .expect("index body");
    let first_head_child = index.find("<head>").expect("head exists") + "<head>".len();
    assert_eq!(
        index.find("<script>window.__DSH_BOOT__="),
        Some(first_head_child),
        "the boot script is the first head child"
    );
    assert!(index.contains("id=\"root\""));
    assert!(index.contains("\\u003cClock>"), "boot JSON escapes HTML");
    let boot = boot_graph(&index);
    assert_eq!(boot["entries"][0]["id"], "clock");
    assert_eq!(boot["entries"][0]["inject"], json!(["logger"]));
    let rev = boot["entries"][0]["rev"].as_str().expect("plugin revision");
    assert_eq!(
        client
            .get(format!("{base}/plugins/clock/client.js?rev={rev}"))
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

    let blank = browser_rpc(
        &client,
        &base,
        "session.create",
        "blank-session",
        json!({"workspaceId": "default", "sessionId": "blank-session"}),
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
                "{base}/plugins/clock/client.js?rev={}",
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

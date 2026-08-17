use std::{
    collections::BTreeSet,
    fs,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tessivum::{
    host::{HostApi, HostNotification},
    protocol::{
        AgentCancelCause, InitializeParams, InitializeResult, MessageId, SdkServerInfo,
        SessionEvent, SessionEventNotification, SessionId, SessionPromptParams,
        SessionPromptResult, SessionStatus, SessionStatusNotification,
    },
    sdk::{JsonRpcServer, MAX_LINE_BYTES},
    TessivumError,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    sync::broadcast,
    time::{timeout, Duration},
};

struct FakeHost {
    calls: Mutex<Vec<String>>,
    notifications: broadcast::Sender<HostNotification>,
    shutdowns: AtomicUsize,
}

impl FakeHost {
    fn new() -> Arc<Self> {
        let (notifications, _) = broadcast::channel(32);
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            notifications,
            shutdowns: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl HostApi for FakeHost {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult, TessivumError> {
        self.calls
            .lock()
            .push(format!("initialize:{}", params.model));
        Ok(InitializeResult {
            server_info: SdkServerInfo {
                name: "deepseek-harness-sdk-runtime".into(),
                version: "1".into(),
            },
        })
    }

    async fn prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.calls
            .lock()
            .push(format!("prompt:{}", params.session_id));
        let _ = self
            .notifications
            .send(HostNotification::SessionEvent(SessionEventNotification {
                session_id: params.session_id.clone(),
                event: SessionEvent {
                    event_type: "turn/start".into(),
                    seq: 0,
                    time: 0,
                    data: json!({"turn": 1}),
                    ignorable: None,
                    source_event_seqs: None,
                    surface_op: None,
                },
            }));
        let _ =
            self.notifications
                .send(HostNotification::SessionStatus(SessionStatusNotification {
                    session_id: params.session_id,
                    status: SessionStatus::Running,
                }));
        Ok(SessionPromptResult {
            message_id: MessageId::from("message-1"),
        })
    }

    async fn cancel(
        &self,
        session: SessionId,
        cause: AgentCancelCause,
    ) -> Result<bool, TessivumError> {
        self.calls
            .lock()
            .push(format!("cancel:{session}:{cause:?}"));
        Ok(true)
    }

    async fn events(
        &self,
        _session: SessionId,
        _from_seq: u64,
    ) -> Result<Vec<SessionEvent>, TessivumError> {
        Ok(Vec::new())
    }

    async fn status(&self, _session: SessionId) -> Result<Option<SessionStatus>, TessivumError> {
        Ok(Some(SessionStatus::Idle))
    }

    fn subscribe(&self) -> broadcast::Receiver<HostNotification> {
        self.notifications.subscribe()
    }

    async fn shutdown(&self) -> Result<(), TessivumError> {
        self.calls.lock().push("shutdown".into());
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        let _ =
            self.notifications
                .send(HostNotification::SessionStatus(SessionStatusNotification {
                    session_id: SessionId::from("session-1"),
                    status: SessionStatus::Idle,
                }));
        Ok(())
    }
}

async fn send(stream: &mut tokio::io::WriteHalf<DuplexStream>, value: Value) {
    let line = serde_json::to_vec(&value).unwrap();
    stream.write_all(&line).await.unwrap();
    stream.write_all(b"\n").await.unwrap();
    stream.flush().await.unwrap();
}

async fn receive(reader: &mut BufReader<tokio::io::ReadHalf<DuplexStream>>) -> Value {
    let mut line = String::new();
    timeout(Duration::from_secs(1), reader.read_line(&mut line))
        .await
        .expect("server timed out")
        .expect("server read failed");
    serde_json::from_str(&line).unwrap()
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn initialize(id: u64) -> Value {
    request(
        id,
        "initialize",
        json!({"cwd": "/tmp", "provider": "recorded", "model": "test"}),
    )
}

#[tokio::test]
async fn duplex_rpc_orders_responses_and_drains_notifications_before_shutdown() {
    let host = FakeHost::new();
    let server = JsonRpcServer::new(host.clone());
    let (client, server_stream) = tokio::io::duplex(MAX_LINE_BYTES * 2 + 4096);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task = tokio::spawn(async move { server.serve(server_read, server_write).await });
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut client_read = BufReader::new(client_read);

    send(&mut client_write, request(0, "initialize", json!({}))).await;
    let invalid_id = receive(&mut client_read).await;
    assert_eq!(invalid_id["id"], Value::Null);
    assert_eq!(invalid_id["error"]["code"], -32600);
    send(
        &mut client_write,
        request(
            1,
            "initialize",
            json!({"cwd": "/tmp", "provider": "recorded", "model": "test", "extra": true}),
        ),
    )
    .await;
    assert_eq!(receive(&mut client_read).await["error"]["code"], -32602);

    send(&mut client_write, initialize(2)).await;
    let initialized = receive(&mut client_read).await;
    assert_eq!(initialized["id"], 2);
    assert_eq!(
        initialized["result"]["serverInfo"]["name"],
        "deepseek-harness-sdk-runtime"
    );

    send(&mut client_write, initialize(3)).await;
    assert_eq!(receive(&mut client_read).await["error"]["code"], -32000);

    send(&mut client_write, request(4, "unknown", json!({}))).await;
    assert_eq!(receive(&mut client_read).await["error"]["code"], -32601);

    send(
        &mut client_write,
        request(
            5,
            "session/prompt",
            json!({"sessionId": "session-1", "contentBlocks": [{"type": "text", "text": "hello"}]}),
        ),
    )
    .await;
    send(
        &mut client_write,
        request(
            6,
            "session/cancel",
            json!({"sessionId": "session-1", "cause": {"kind": "user"}}),
        ),
    )
    .await;
    send(&mut client_write, request(7, "shutdown", json!({}))).await;

    let mut response_ids = BTreeSet::new();
    let mut notification_indexes = Vec::new();
    let mut shutdown_index = None;
    for index in 0..8 {
        let frame = receive(&mut client_read).await;
        if let Some(id) = frame.get("id").and_then(Value::as_u64) {
            response_ids.insert(id);
            if id == 7 {
                shutdown_index = Some(index);
                assert_eq!(frame["result"], json!({}));
                break;
            }
        } else {
            notification_indexes.push(index);
        }
    }
    assert_eq!(response_ids, BTreeSet::from([5, 6, 7]));
    assert!(notification_indexes.len() >= 3);
    assert!(notification_indexes
        .iter()
        .all(|index| *index < shutdown_index.unwrap()));
    assert_eq!(
        host.calls.lock().join(","),
        "initialize:test,prompt:session-1,cancel:session-1:User,shutdown"
    );
    assert_eq!(host.shutdowns.load(Ordering::SeqCst), 1);
    assert!(server_task.await.unwrap().is_ok());
}

#[tokio::test]
async fn oversized_frame_is_rejected_without_poisoning_following_request() {
    let host = FakeHost::new();
    let server = JsonRpcServer::new(host.clone());
    let (client, server_stream) = tokio::io::duplex(MAX_LINE_BYTES * 2 + 4096);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task = tokio::spawn(async move { server.serve(server_read, server_write).await });
    let (client_read, mut client_write) = tokio::io::split(client);
    let mut client_read = BufReader::new(client_read);

    client_write
        .write_all(&vec![b'x'; MAX_LINE_BYTES + 1])
        .await
        .unwrap();
    client_write.write_all(b"\n").await.unwrap();
    client_write.flush().await.unwrap();
    assert_eq!(receive(&mut client_read).await["error"]["code"], -32600);

    send(&mut client_write, initialize(1)).await;
    assert_eq!(receive(&mut client_read).await["id"], 1);
    send(&mut client_write, request(2, "shutdown", json!({}))).await;
    loop {
        if receive(&mut client_read).await["id"] == 2 {
            break;
        }
    }
    assert_eq!(host.shutdowns.load(Ordering::SeqCst), 1);
    assert!(server_task.await.unwrap().is_ok());
}

#[tokio::test]
async fn disconnect_shuts_down_host_without_waiting_for_input() {
    let host = FakeHost::new();
    let server = JsonRpcServer::new(host.clone());
    let (client, server_stream) = tokio::io::duplex(4096);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let server_task = tokio::spawn(async move { server.serve(server_read, server_write).await });
    let (client_read, mut client_write) = tokio::io::split(client);
    client_write.shutdown().await.unwrap();
    drop(client_write);
    drop(client_read);

    assert!(timeout(Duration::from_secs(1), server_task)
        .await
        .expect("server did not clean up disconnected client")
        .unwrap()
        .is_ok());
    assert_eq!(host.shutdowns.load(Ordering::SeqCst), 1);
}

#[test]
fn typescript_and_python_clients_match_scripted_wire_snapshots() {
    if Command::new("bun").arg("--version").output().is_err()
        || Command::new("python3").arg("--version").output().is_err()
        || Command::new("node").arg("--version").output().is_err()
    {
        return;
    }

    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let temporary = std::env::temp_dir().join(format!(
        "tessivum-sdk-client-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&temporary).unwrap();
    let node_fixture = temporary.join("fixture.js");
    let python_fixture = temporary.join("fixture.py");
    let ts_driver = temporary.join("driver.ts");
    let python_driver = temporary.join("driver.py");
    let ts_audit = temporary.join("typescript-audit.jsonl");
    let python_audit = temporary.join("python-audit.jsonl");

    fs::write(
        &node_fixture,
        r#"const fs = require("node:fs");
const audit = process.argv[2];
const lines = require("node:readline").createInterface({input: process.stdin});
lines.on("line", line => {
  fs.appendFileSync(audit, line + "\n");
  const request = JSON.parse(line);
  let result;
  if (request.method === "initialize") result = {serverInfo:{name:"deepseek-harness-sdk-runtime",version:"1"}};
  else if (request.method === "session/prompt") { process.stdout.write(JSON.stringify({jsonrpc:"2.0",method:"session.event",params:{sessionId:request.params.sessionId,event:{type:"turn/start",seq:0,time:0,data:{turn:1}}}}) + "\n"); result = {messageId:"message-1"}; }
  else if (request.method === "session/cancel") result = true;
  else result = {};
  process.stdout.write(JSON.stringify({jsonrpc:"2.0",id:request.id,result}) + "\n");
  if (request.method === "shutdown") lines.close();
});
"#,
    )
    .unwrap();
    fs::write(
        &python_fixture,
        r#"import json, sys
audit = sys.argv[1]
for line in sys.stdin:
    with open(audit, "a") as output: output.write(line)
    request = json.loads(line)
    if request["method"] == "initialize": result = {"serverInfo":{"name":"deepseek-harness-sdk-runtime","version":"1"}}
    elif request["method"] == "session/prompt":
        print(json.dumps({"jsonrpc":"2.0","method":"session.event","params":{"sessionId":request["params"]["sessionId"],"event":{"type":"turn/start","seq":0,"time":0,"data":{"turn":1}}}}), flush=True)
        result = {"messageId":"message-1"}
    elif request["method"] == "session/cancel": result = True
    else: result = {}
    print(json.dumps({"jsonrpc":"2.0","id":request["id"],"result":result}), flush=True)
    if request["method"] == "shutdown": break
"#,
    )
    .unwrap();

    let ts_client = serde_json::to_string(
        root.join("sdk/typescript/client.ts")
            .to_string_lossy()
            .as_ref(),
    )
    .unwrap();
    let node_fixture_literal =
        serde_json::to_string(node_fixture.to_string_lossy().as_ref()).unwrap();
    let ts_audit_literal = serde_json::to_string(ts_audit.to_string_lossy().as_ref()).unwrap();
    fs::write(
        &ts_driver,
        format!(
            r#"import {{ JsonRpcClient }} from {ts_client};
const notifications: string[] = [];
const client = new JsonRpcClient("node", [{node_fixture_literal}, {ts_audit_literal}], {{timeoutMs: 2000, onNotification: item => notifications.push(item.method)}});
const initialized = await client.initialize({{cwd:"/tmp",provider:"recorded",model:"test"}});
const prompted = await client.prompt({{sessionId:"session-1",contentBlocks:[{{type:"text",text:"hello"}}]}});
const cancelled = await client.cancel("session-1");
const shutdown = await client.shutdown();
console.log(JSON.stringify({{initialized,prompted,cancelled,shutdown,notifications}}));
"#
        ),
    )
    .unwrap();
    let python_client =
        serde_json::to_string(root.join("sdk/python/client.py").to_string_lossy().as_ref())
            .unwrap();
    let python_fixture_literal =
        serde_json::to_string(python_fixture.to_string_lossy().as_ref()).unwrap();
    let python_audit_literal =
        serde_json::to_string(python_audit.to_string_lossy().as_ref()).unwrap();
    fs::write(
        &python_driver,
        format!(
            r#"import asyncio, importlib.util, json, sys
spec = importlib.util.spec_from_file_location("sdk_client", {python_client})
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
notifications = []
def notify(item): notifications.append(item.method)
async def run():
    client = await module.JsonRpcClient.start(sys.executable, {python_fixture_literal}, {python_audit_literal}, timeout=2, on_notification=notify)
    initialized = await client.initialize({{"cwd":"/tmp","provider":"recorded","model":"test"}})
    prompted = await client.prompt("session-1", [{{"type":"text","text":"hello"}}])
    cancelled = await client.cancel("session-1")
    shutdown = await client.shutdown()
    print(json.dumps({{"initialized":initialized,"prompted":prompted,"cancelled":cancelled,"shutdown":shutdown,"notifications":notifications}}, separators=(",", ":")))
asyncio.run(run())
"#
        ),
    )
    .unwrap();

    let typescript = Command::new("bun")
        .args(["run", ts_driver.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        typescript.status.success(),
        "{}",
        String::from_utf8_lossy(&typescript.stderr)
    );
    let python = Command::new("python3")
        .arg(&python_driver)
        .output()
        .unwrap();
    assert!(
        python.status.success(),
        "{}",
        String::from_utf8_lossy(&python.stderr)
    );

    let expected = vec![
        initialize(1),
        request(
            2,
            "session/prompt",
            json!({"sessionId":"session-1","contentBlocks":[{"type":"text","text":"hello"}]}),
        ),
        request(
            3,
            "session/cancel",
            json!({"sessionId":"session-1","cause":{"kind":"user"}}),
        ),
        request(4, "shutdown", json!({})),
    ];
    let read_audit = |path: &std::path::Path| -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    assert_eq!(read_audit(&ts_audit), expected);
    assert_eq!(read_audit(&python_audit), expected);
    let snapshot = json!({
        "initialized": {"serverInfo":{"name":"deepseek-harness-sdk-runtime","version":"1"}},
        "prompted": {"messageId":"message-1"},
        "cancelled": true,
        "shutdown": {},
        "notifications": ["session.event"],
    });
    assert_eq!(
        serde_json::from_slice::<Value>(&typescript.stdout).unwrap(),
        snapshot
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&python.stdout).unwrap(),
        snapshot
    );
    fs::remove_dir_all(temporary).unwrap();
}

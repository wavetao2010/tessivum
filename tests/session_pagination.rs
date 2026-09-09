use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::{json, Value};
use tessivum::{
    api::{ApiServer, MAX_FRAME_BYTES},
    host::{HostApi, HostNotification, HostSessionInfo, HostSessionProjection, HostSessionSummary},
    protocol::{
        AgentCancelCause, InitializeParams, InitializeResult, MessageId, SdkServerInfo,
        SessionEvent, SessionId, SessionPromptParams, SessionPromptResult, SessionStatus,
    },
    workspace::WorkspaceId,
    TessivumError,
};
use tokio::sync::broadcast;

struct PaginationHost {
    summaries: Mutex<Vec<HostSessionSummary>>,
    events: Mutex<BTreeMap<SessionId, Vec<SessionEvent>>>,
    projections: Mutex<BTreeMap<SessionId, Vec<HostSessionProjection>>>,
    notifications: broadcast::Sender<HostNotification>,
}

impl PaginationHost {
    fn new(summaries: Vec<HostSessionSummary>) -> Self {
        let (notifications, _) = broadcast::channel(16);
        Self {
            summaries: Mutex::new(summaries),
            events: Mutex::new(BTreeMap::new()),
            projections: Mutex::new(BTreeMap::new()),
            notifications,
        }
    }

    fn mutate(&self, mutation: impl FnOnce(&mut Vec<HostSessionSummary>)) {
        mutation(&mut self.summaries.lock().expect("summary lock"));
    }
}

#[async_trait]
impl HostApi for PaginationHost {
    async fn initialize(
        &self,
        _params: InitializeParams,
    ) -> Result<InitializeResult, TessivumError> {
        Ok(InitializeResult {
            server_info: SdkServerInfo {
                name: "pagination-test".into(),
                version: "1".into(),
            },
        })
    }

    async fn prompt(
        &self,
        _params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        Ok(SessionPromptResult {
            message_id: MessageId::from("unused"),
        })
    }

    async fn cancel(
        &self,
        _session: SessionId,
        _cause: AgentCancelCause,
    ) -> Result<bool, TessivumError> {
        Ok(false)
    }

    async fn events(
        &self,
        session: SessionId,
        from_seq: u64,
    ) -> Result<Vec<SessionEvent>, TessivumError> {
        Ok(self
            .events
            .lock()
            .expect("event lock")
            .get(&session)
            .into_iter()
            .flatten()
            .filter(|event| event.seq >= from_seq)
            .cloned()
            .collect())
    }

    async fn session_projections(
        &self,
        session: SessionId,
    ) -> Result<Vec<HostSessionProjection>, TessivumError> {
        Ok(self
            .projections
            .lock()
            .expect("projection lock")
            .get(&session)
            .cloned()
            .unwrap_or_default())
    }

    async fn list_session_summaries(&self) -> Result<Vec<HostSessionSummary>, TessivumError> {
        Ok(self.summaries.lock().expect("summary lock").clone())
    }

    async fn status(&self, _session: SessionId) -> Result<Option<SessionStatus>, TessivumError> {
        Ok(None)
    }

    fn subscribe(&self) -> broadcast::Receiver<HostNotification> {
        self.notifications.subscribe()
    }

    async fn shutdown(&self) -> Result<(), TessivumError> {
        Ok(())
    }
}

fn summary(
    id: impl Into<SessionId>,
    cwd: String,
    title: Option<(&str, u64)>,
) -> HostSessionSummary {
    let id = id.into();
    HostSessionSummary {
        session: HostSessionInfo {
            session_id: id,
            workspace_id: None,
            created_at: 1,
            updated_at: 2,
            running: false,
            cwd: Some(cwd),
            parent_session: None,
            origin: None,
            agent_mode: None,
            event_count: 1,
            blank: false,
        },
        title: title.map(|(title, seq)| HostSessionProjection {
            key: "title".into(),
            value: json!(title),
            seq: Some(seq),
        }),
    }
}

fn event(seq: u64, event_type: &str, data: Value) -> SessionEvent {
    SessionEvent {
        event_type: event_type.into(),
        seq,
        time: seq,
        data,
        ignorable: None,
        source_event_seqs: None,
        surface_op: None,
    }
}

async fn start(host: Arc<PaginationHost>) -> (ApiServer, String) {
    let server = ApiServer::bind(host).await.expect("API binds");
    let base = format!("http://{}", server.local_addr());
    (server, base)
}

async fn call(base: &str, rpc_id: &str, payload: Value) -> (Vec<u8>, Value) {
    let response = reqwest::Client::new()
        .post(format!("{base}/api/session.list"))
        .json(&json!({
            "type": "client-request",
            "rpcId": rpc_id,
            "method": "session.list",
            "payload": payload,
        }))
        .send()
        .await
        .expect("session.list response");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let bytes = response.bytes().await.expect("session.list body").to_vec();
    let value = serde_json::from_slice(&bytes).expect("session.list JSON");
    (bytes, value)
}

async fn history(base: &str, session_id: &str) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/api/session.history"))
        .json(&json!({
            "type": "client-request",
            "rpcId": "history",
            "method": "session.history",
            "payload": {"sessionId": session_id, "maxMessages": 50},
        }))
        .send()
        .await
        .expect("session.history response")
        .json()
        .await
        .expect("session.history JSON")
}

fn value(response: &Value) -> &Value {
    &response["result"]["value"]
}

async fn first_cursor(base: &str) -> String {
    let (_, response) = call(base, "cursor", json!({"limit": 1})).await;
    value(&response)["nextCursor"]
        .as_str()
        .expect("next cursor")
        .to_owned()
}

#[tokio::test]
async fn session_list_pages_a_large_stable_summary_baseline_with_bounded_frames() {
    let mut summaries = (0..700)
        .rev()
        .map(|index| {
            let title = format!("Title {index:04}");
            summary(
                format!("session-{index:04}"),
                format!("/workspace/{index:04}/{}", "x".repeat(256)),
                Some((&title, index as u64)),
            )
        })
        .collect::<Vec<_>>();
    summaries.push(summary(
        "projection-heavy",
        "/workspace/heavy".into(),
        Some(("Visible title", 7)),
    ));
    let host = Arc::new(PaginationHost::new(summaries));
    host.events.lock().expect("event lock").insert(
        SessionId::from("projection-heavy"),
        vec![event(7, "session/title", json!({"title": "Visible title"}))],
    );
    host.projections.lock().expect("projection lock").insert(
        SessionId::from("projection-heavy"),
        vec![
            HostSessionProjection {
                key: "title".into(),
                value: json!("Visible title"),
                seq: Some(7),
            },
            HostSessionProjection {
                key: "unrelatedFullProjection".into(),
                value: json!("z".repeat(MAX_FRAME_BYTES + 1)),
                seq: Some(8),
            },
        ],
    );
    let (mut server, base) = start(Arc::clone(&host)).await;

    let mut cursor = None;
    let mut ids = Vec::new();
    let mut snapshot = None;
    loop {
        let payload = cursor.as_ref().map_or_else(
            || json!({"limit": 500}),
            |cursor| json!({"limit": 500, "cursor": cursor}),
        );
        let rpc_id = format!("{:r<128}", ids.len());
        let (bytes, response) = call(&base, &rpc_id, payload).await;
        assert!(
            bytes.len() <= MAX_FRAME_BYTES,
            "encoded response exceeded frame budget"
        );
        assert_eq!(response["result"]["ok"], true);
        let page = value(&response);
        assert!(page["items"].as_array().unwrap().len() <= 500);
        if let Some(expected) = &snapshot {
            assert_eq!(page["snapshot"], *expected);
        } else {
            snapshot = Some(page["snapshot"].clone());
        }
        ids.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["sessionId"].as_str().unwrap().to_owned()),
        );
        cursor = page["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    let mut expected = ids.clone();
    expected.sort();
    expected.dedup();
    assert_eq!(ids, expected, "pages are lexical and contain no duplicates");
    assert_eq!(ids.len(), 701);

    let (_, small) = call(&base, "small", json!({"limit": 1})).await;
    let listed = &value(&small)["items"][0];
    assert_eq!(
        listed["projections"]["values"].as_object().unwrap().len(),
        1
    );
    assert_eq!(listed["projections"]["asOfSeq"], 7);
    assert!(listed["projections"]["values"]
        .get("unrelatedFullProjection")
        .is_none());
    let opened = history(&base, "projection-heavy").await;
    assert_eq!(
        opened["result"]["value"]["projections"]["values"]["unrelatedFullProjection"]
            .as_str()
            .unwrap()
            .len(),
        MAX_FRAME_BYTES + 1
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_list_rejects_bad_and_stale_cursors_for_every_summary_mutation() {
    let host = Arc::new(PaginationHost::new(vec![
        summary("a", "/a".into(), Some(("A", 1))),
        summary("b", "/b".into(), Some(("B", 1))),
        summary("c", "/c".into(), Some(("C", 1))),
    ]));
    let (mut server, base) = start(Arc::clone(&host)).await;

    for payload in [
        json!({"cursor": "garbage"}),
        json!({"cursor": ""}),
        json!({"cursor": "x".repeat(161)}),
        json!({"cursor": null}),
        json!({"cursor": 1}),
        json!({"limit": 0}),
        json!({"limit": 501}),
        json!({"limit": "100"}),
        json!({"limit": null}),
    ] {
        let (_, response) = call(&base, "invalid", payload).await;
        assert_eq!(response["result"]["error"]["code"], "invalid-request");
    }
    for payload in [json!({}), json!({"limit": 2})] {
        let (_, response) = call(&base, "valid", payload).await;
        assert_eq!(response["result"]["ok"], true);
    }
    let genuine = first_cursor(&base).await;
    let (_, response) = call(&base, "valid-cursor", json!({"cursor": genuine.clone()})).await;
    assert_eq!(response["result"]["ok"], true);
    let mut forged = genuine;
    forged.replace_range(
        forged.len() - 1..,
        if forged.ends_with('0') { "1" } else { "0" },
    );
    let (_, response) = call(&base, "forged", json!({"cursor": forged})).await;
    assert_eq!(response["result"]["error"]["code"], "invalid-request");

    let cursor = first_cursor(&base).await;
    host.mutate(|rows| rows.push(summary("d", "/d".into(), Some(("D", 1)))));
    let (_, response) = call(&base, "added", json!({"cursor": cursor})).await;
    assert_eq!(response["result"]["error"]["code"], "stale-cursor");

    let cursor = first_cursor(&base).await;
    host.mutate(|rows| rows.retain(|row| row.session.session_id.as_str() != "d"));
    let (_, response) = call(&base, "removed", json!({"cursor": cursor})).await;
    assert_eq!(response["result"]["error"]["code"], "stale-cursor");

    let cursor = first_cursor(&base).await;
    host.mutate(|rows| {
        rows[0].title = Some(HostSessionProjection {
            key: "title".into(),
            value: json!("Renamed"),
            seq: Some(2),
        });
    });
    let (_, response) = call(&base, "title", json!({"cursor": cursor})).await;
    assert_eq!(response["result"]["error"]["code"], "stale-cursor");

    let cursor = first_cursor(&base).await;
    host.mutate(|rows| rows[0].session.running = true);
    let (_, response) = call(&base, "status", json!({"cursor": cursor})).await;
    assert_eq!(response["result"]["error"]["code"], "stale-cursor");

    let cursor = first_cursor(&base).await;
    host.mutate(|rows| rows[0].session.updated_at += 1);
    let (_, response) = call(&base, "activity", json!({"cursor": cursor})).await;
    assert_eq!(response["result"]["error"]["code"], "stale-cursor");

    let cursor = first_cursor(&base).await;
    host.mutate(|rows| rows[0].session.workspace_id = Some(WorkspaceId::from("moved")));
    let (_, response) = call(&base, "membership", json!({"cursor": cursor})).await;
    assert_eq!(response["result"]["error"]["code"], "stale-cursor");

    let cursor = first_cursor(&base).await;
    host.projections.lock().expect("projection lock").insert(
        SessionId::from("a"),
        vec![HostSessionProjection {
            key: "unrelated".into(),
            value: json!("changed"),
            seq: Some(99),
        }],
    );
    let (_, response) = call(&base, "unrelated", json!({"cursor": cursor})).await;
    assert_eq!(
        response["result"]["ok"], true,
        "non-summary projections do not stale a page"
    );

    let restart_cursor = first_cursor(&base).await;
    let (mut restarted, restarted_base) = start(Arc::clone(&host)).await;
    let (_, response) = call(
        &restarted_base,
        "restart",
        json!({"cursor": restart_cursor}),
    )
    .await;
    assert_eq!(response["result"]["error"]["code"], "stale-cursor");

    restarted.shutdown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_list_reports_a_single_oversized_summary_without_empty_progress() {
    let host = Arc::new(PaginationHost::new(vec![summary(
        "oversized",
        "x".repeat(MAX_FRAME_BYTES),
        Some(("Oversized", 1)),
    )]));
    let (mut server, base) = start(host).await;
    let (bytes, response) = call(&base, "oversized", json!({})).await;
    assert!(bytes.len() <= MAX_FRAME_BYTES);
    assert_eq!(response["result"]["error"]["code"], "response-too-large");
    assert_eq!(
        response["result"]["error"]["details"]["sessionId"],
        "oversized"
    );
    server.shutdown().await.unwrap();
}

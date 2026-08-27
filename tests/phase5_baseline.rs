use std::collections::BTreeSet;

use serde_json::{json, Value};
use tessivum::protocol::SessionHeader;

const LEGACY_SESSION_HEADERS: &str = include_str!("../fixtures/phase5/legacy-session-headers.json");
const BROWSER_AGENT_PRESET_RPC: &str =
    include_str!("../fixtures/phase5/browser-agent-preset-rpc.json");
const LEGACY_MODE_OBSERVATIONS: &str =
    include_str!("../fixtures/phase5/legacy-mode-observations.json");

fn fixture(name: &str, source: &str) -> Value {
    serde_json::from_str(source).unwrap_or_else(|error| panic!("{name} is JSON: {error}"))
}

fn assert_keys(value: &Value, expected: &[&str]) {
    let actual = value
        .as_object()
        .expect("fixture value is an object")
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    let expected = expected
        .iter()
        .map(|key| (*key).to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
}

fn strings(value: &Value, label: &str) -> Vec<String> {
    value
        .as_array()
        .unwrap_or_else(|| panic!("{label} is an array"))
        .iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("{label} contains strings"))
                .to_owned()
        })
        .collect()
}

#[test]
fn phase_five_baseline_fixtures_preserve_legacy_migration_inputs() {
    let legacy_ids = BTreeSet::from([
        "code".to_owned(),
        "cordis".to_owned(),
        "minimal".to_owned(),
        "standard".to_owned(),
    ]);

    let headers = fixture("legacy session headers", LEGACY_SESSION_HEADERS);
    assert_keys(&headers, &["schema", "legacyPresetIds", "headers"]);
    assert_eq!(headers["schema"], json!(1));
    assert_eq!(
        strings(&headers["legacyPresetIds"], "legacy preset IDs")
            .into_iter()
            .collect::<BTreeSet<_>>(),
        legacy_ids
    );
    assert_eq!(headers["legacyPresetIds"].as_array().unwrap().len(), legacy_ids.len());
    let header_rows = headers["headers"].as_array().expect("header rows");
    assert_eq!(header_rows.len(), 5);
    let mut cases = BTreeSet::new();
    let mut session_ids = BTreeSet::new();
    let mut presets = BTreeSet::new();
    for row in header_rows {
        assert_keys(row, &["case", "header"]);
        let case = row["case"].as_str().expect("header case");
        assert!(cases.insert(case.to_owned()), "duplicate header case: {case}");
        let raw = &row["header"];
        assert_keys(raw, &["version", "id", "createdAt", "agentPreset"]);
        let header: SessionHeader = serde_json::from_value(raw.clone())
            .expect("legacy SessionHeader deserializes through current code");
        header.validate().expect("legacy SessionHeader validates");
        assert!(
            session_ids.insert(header.id.as_str().to_owned()),
            "duplicate legacy session ID"
        );
        let preset = header.agent_preset.expect("legacy agentPreset");
        assert!(presets.insert(preset), "duplicate legacy preset input");
    }
    assert_eq!(
        cases,
        BTreeSet::from([
            "code".to_owned(),
            "cordis".to_owned(),
            "minimal".to_owned(),
            "standard".to_owned(),
            "unknown-custom".to_owned(),
        ])
    );
    assert_eq!(
        presets,
        BTreeSet::from([
            "code".to_owned(),
            "cordis".to_owned(),
            "minimal".to_owned(),
            "repository-maintainer".to_owned(),
            "standard".to_owned(),
        ])
    );

    let browser = fixture("browser agent preset RPC", BROWSER_AGENT_PRESET_RPC);
    assert_keys(&browser, &["schema", "legacyPresetIds", "calls"]);
    assert_eq!(browser["schema"], json!(1));
    assert_eq!(
        strings(&browser["legacyPresetIds"], "browser legacy preset IDs")
            .into_iter()
            .collect::<BTreeSet<_>>(),
        legacy_ids
    );
    assert_eq!(browser["legacyPresetIds"].as_array().unwrap().len(), legacy_ids.len());
    let calls = browser["calls"].as_array().expect("browser calls");
    assert_eq!(calls.len(), 6);
    let mut call_cases = BTreeSet::new();
    let mut rpc_ids = BTreeSet::new();
    let mut methods = BTreeSet::new();
    for call in calls {
        assert_keys(call, &["case", "request", "response"]);
        let case = call["case"].as_str().expect("browser case");
        assert!(call_cases.insert(case.to_owned()), "duplicate browser case: {case}");
        let request = &call["request"];
        assert_keys(request, &["path", "body"]);
        let body = &request["body"];
        assert_keys(body, &["type", "rpcId", "method", "payload"]);
        assert_eq!(body["type"], json!("client-request"));
        let rpc_id = body["rpcId"].as_str().expect("request RPC ID");
        assert!(rpc_ids.insert(rpc_id.to_owned()), "duplicate RPC ID: {rpc_id}");
        let method = body["method"].as_str().expect("request method");
        assert!(methods.insert(method.to_owned()), "duplicate method: {method}");
        assert_eq!(request["path"], json!(format!("/api/{method}")));

        let response = &call["response"];
        assert_keys(response, &["type", "rpcId", "result"]);
        assert_eq!(response["type"], json!("server-response"));
        assert_eq!(response["rpcId"], body["rpcId"]);
        assert_keys(&response["result"], &["ok", "value"]);
        assert_eq!(response["result"]["ok"], json!(true));

        match method {
            "agentPreset.list" => {
                assert_eq!(body["payload"], json!({}));
                let value = &response["result"]["value"];
                assert_keys(value, &["presets", "authorable", "hasDocument"]);
                let listed_ids = value["presets"]
                    .as_array()
                    .expect("listed presets")
                    .iter()
                    .map(|preset| {
                        assert_keys(preset, &["id", "trust", "isDefault", "name", "description"]);
                        preset["id"].as_str().expect("listed preset ID").to_owned()
                    })
                    .collect::<BTreeSet<_>>();
                assert_eq!(listed_ids, legacy_ids);
            }
            "agentPreset.read" => {
                assert_keys(&body["payload"], &["agentPreset"]);
                assert_eq!(body["payload"]["agentPreset"], json!("standard"));
                let value = &response["result"]["value"];
                assert_keys(value, &["agentPreset", "trust", "content", "name", "description"]);
                assert_eq!(value["agentPreset"], json!("standard"));
                assert!(!value["content"].as_str().expect("composition content").is_empty());
            }
            "agentPreset.copy" => {
                assert_keys(&body["payload"], &["from", "agentPreset", "name"]);
                assert_eq!(body["payload"]["from"], json!("standard"));
                assert_eq!(response["result"]["value"], json!({"agentPreset": "phase5-copy"}));
            }
            "agentPreset.remove" => {
                assert_keys(&body["payload"], &["agentPreset"]);
                assert_eq!(response["result"]["value"], json!({}));
            }
            "agentPreset.openDocument" => {
                assert_keys(&body["payload"], &["agentPreset"]);
                assert_eq!(response["result"]["value"], json!({"opened": true}));
            }
            "agentPreset.select" => {
                assert_keys(&body["payload"], &["sessionId", "agentPreset"]);
                assert_eq!(body["payload"]["agentPreset"], json!("cordis"));
                assert_eq!(response["result"]["value"], json!({"agentPreset": "cordis"}));
            }
            _ => panic!("unexpected browser migration method: {method}"),
        }
    }
    assert_eq!(
        methods,
        BTreeSet::from([
            "agentPreset.copy".to_owned(),
            "agentPreset.list".to_owned(),
            "agentPreset.openDocument".to_owned(),
            "agentPreset.read".to_owned(),
            "agentPreset.remove".to_owned(),
            "agentPreset.select".to_owned(),
        ])
    );

    let observations = fixture("legacy mode observations", LEGACY_MODE_OBSERVATIONS);
    assert_keys(
        &observations,
        &["schema", "legacyPresetIds", "status", "hostGlobalSwitch", "observations"],
    );
    assert_eq!(observations["schema"], json!(1));
    assert_eq!(
        strings(&observations["legacyPresetIds"], "observed legacy preset IDs")
            .into_iter()
            .collect::<BTreeSet<_>>(),
        legacy_ids
    );
    assert_eq!(
        observations["legacyPresetIds"].as_array().unwrap().len(),
        legacy_ids.len()
    );
    assert_eq!(
        observations["status"],
        json!("observed-legacy-behavior-not-target-contract")
    );
    let global = &observations["hostGlobalSwitch"];
    assert_keys(
        global,
        &["name", "whenAbsent", "whenPresent", "knownHostGlobalDefects"],
    );
    assert_eq!(global["name"], json!("codeRuntime"));
    assert_keys(&global["whenAbsent"], &["factory", "modelToolPresentation"]);
    assert_keys(&global["whenPresent"], &["factory", "modelToolPresentation"]);
    let mut defects = BTreeSet::new();
    for defect in global["knownHostGlobalDefects"]
        .as_array()
        .expect("known Host-global defects")
    {
        assert_keys(
            defect,
            &["id", "affectedLegacyPresetIds", "observation", "notDesiredBehavior"],
        );
        let id = defect["id"].as_str().expect("defect ID");
        assert!(defects.insert(id.to_owned()), "duplicate defect ID: {id}");
        assert_eq!(defect["notDesiredBehavior"], json!(true));
    }
    assert_eq!(defects.len(), 2);

    let rows = observations["observations"].as_array().expect("mode observations");
    assert_eq!(rows.len(), 4);
    let mut observed_presets = BTreeSet::new();
    for row in rows {
        assert_keys(
            row,
            &[
                "legacyPreset",
                "sessionHeader",
                "prompt",
                "toolsWhenCodeRuntimeAbsent",
                "toolsWhenCodeRuntimePresent",
            ],
        );
        let preset = row["legacyPreset"].as_str().expect("observed preset");
        assert!(observed_presets.insert(preset.to_owned()), "duplicate observation: {preset}");
        assert_eq!(row["sessionHeader"], json!({"agentPreset": preset}));
        assert_keys(&row["prompt"], &["completePersona", "modelHeaderSource", "text"]);
        let absent = strings(&row["toolsWhenCodeRuntimeAbsent"], "direct tool catalog");
        let present = strings(&row["toolsWhenCodeRuntimePresent"], "code tool catalog");
        assert!(!absent.is_empty());
        assert_eq!(present, vec!["run_code".to_owned()]);
        match preset {
            "minimal" => {
                assert_eq!(row["prompt"]["completePersona"], json!(true));
                assert_eq!(
                    row["prompt"]["text"],
                    json!("You are a helpful software engineer assistant.")
                );
                assert_eq!(absent, vec!["bash".to_owned(), "str_replace_editor".to_owned()]);
            }
            "standard" | "code" | "cordis" => {
                assert_eq!(row["prompt"]["completePersona"], json!(false));
                assert_eq!(row["prompt"]["text"], Value::Null);
                assert!(absent.len() > 2);
            }
            _ => panic!("unexpected observed preset: {preset}"),
        }
    }
    assert_eq!(observed_presets, legacy_ids);
}

use serde_json::{json, Value};
use tessivum_pdk::{export_guest, service_call, Envelope, Guest, Result, ABI};

struct RustMinimalGuest;

impl Guest for RustMinimalGuest {
    fn init(_: Envelope) -> Result<Value> {
        let _: Value = service_call(
            "logger@1",
            "log",
            json!({
                "level": "info",
                "message": "tessivum-rust-minimal initialized",
                "fields": {}
            }),
        )?;
        Ok(json!({ "abi": ABI, "initialized": true }))
    }

    fn call(request: Envelope) -> Result<Value> {
        match request.payload.get("mode").and_then(Value::as_str) {
            Some("denied") => {
                let denial = service_call::<_, Value>("settings@1", "describe", json!({}))
                    .expect_err("settings@1.describe is not declared");
                Ok(json!({ "denial": { "code": denial.code } }))
            }
            Some("trap") => panic!("rust-minimal trap"),
            _ => Ok(json!({ "echo": request.payload })),
        }
    }

    fn event(_: Envelope) -> Result<Value> {
        Ok(json!({ "accepted": true }))
    }

    fn update(_: Envelope) -> Result<Value> {
        Ok(json!({ "updated": true }))
    }

    fn stop(_: Envelope) -> Result<Value> {
        let _: Value = service_call(
            "logger@1",
            "log",
            json!({
                "level": "info",
                "message": "tessivum-rust-minimal stopped",
                "fields": {}
            }),
        )?;
        Ok(json!({ "stopped": true }))
    }
}

export_guest!(RustMinimalGuest);

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{json, Map, Value};

/// Explicit substitutions for nondeterministic opaque strings.
pub type ReplacementMap = BTreeMap<String, String>;

/// Serializes a durable session header and its ordered event log for comparison.
///
/// Only opaque identities, configured working directories, provider request IDs,
/// and event-envelope times are normalized. Everything else is retained verbatim.
pub fn normalize_session_trace<H, I, E>(
    header: H,
    events: I,
    replacements: &ReplacementMap,
) -> Result<Value, serde_json::Error>
where
    H: Serialize,
    I: IntoIterator<Item = E>,
    E: Serialize,
{
    let mut header = serde_json::to_value(header)?;
    normalize_header(&mut header, replacements);

    let events = events
        .into_iter()
        .map(|event| {
            let mut event = serde_json::to_value(event)?;
            normalize_event(&mut event, replacements);
            Ok(event)
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?;

    Ok(json!({"header": header, "events": events}))
}

fn normalize_header(header: &mut Value, replacements: &ReplacementMap) {
    let Some(header) = header.as_object_mut() else {
        return;
    };
    replace_member(header, "id", replacements);
    replace_member(header, "parentSession", replacements);
    replace_member(header, "cwd", replacements);
}

fn normalize_event(event: &mut Value, replacements: &ReplacementMap) {
    let mut normalized = {
        let Some(event) = event.as_object_mut() else {
            return;
        };
        if event.contains_key("time") {
            event.insert("time".into(), Value::from(0));
        }
        Value::Object(std::mem::take(event))
    };
    normalize_value(&mut normalized, replacements);
    *event = normalized;
}

fn normalize_value(value: &mut Value, replacements: &ReplacementMap) {
    match value {
        Value::Array(values) => {
            for value in values {
                normalize_value(value, replacements);
            }
        }
        Value::Object(object) => {
            replace_member(object, "cwd", replacements);
            replace_member(object, "sessionId", replacements);
            replace_member(object, "parentSession", replacements);
            replace_member(object, "parentSessionId", replacements);
            replace_member(object, "childSessionId", replacements);
            replace_member(object, "requestId", replacements);
            replace_member(object, "callId", replacements);
            replace_member(object, "toolCallId", replacements);

            if object.contains_key("role")
                && object.contains_key("content")
                && object.contains_key("source")
            {
                replace_member(object, "id", replacements);
            }
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("tool-call" | "tool-call-delta")
            ) {
                replace_member(object, "id", replacements);
            }

            for value in object.values_mut() {
                normalize_value(value, replacements);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn replace_member(object: &mut Map<String, Value>, name: &str, replacements: &ReplacementMap) {
    let Some(Value::String(value)) = object.get_mut(name) else {
        return;
    };
    let Some(replacement) = replacements.get(value) else {
        return;
    };
    *value = replacement.clone();
}

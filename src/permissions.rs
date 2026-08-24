//! Canonical permission presets and their durable session-state fold.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::{
    approval::ApprovalPolicy,
    protocol::SessionEvent,
    sandbox::SandboxMode,
    settings::{SettingsApplies, SettingsRegistration},
    TessivumError,
};

pub(crate) const PERMISSION_SETTINGS_NAMESPACE: &str = "permission";
pub(crate) const DEFAULT_PERMISSION_PRESET: &str = "workspace-write";
pub(crate) const CUSTOM_PERMISSION_PRESET: &str = "custom";

pub(crate) struct PermissionPreset {
    pub(crate) name: &'static str,
    pub(crate) sandbox: SandboxMode,
    pub(crate) approval: ApprovalPolicy,
    pub(crate) description: &'static str,
}

const PRESETS: [PermissionPreset; 3] = [
    PermissionPreset {
        name: "read-only",
        sandbox: SandboxMode::ReadOnly,
        approval: ApprovalPolicy::Ask,
        description: "Read files without modifying the workspace; broader writes require approval.",
    },
    PermissionPreset {
        name: "workspace-write",
        sandbox: SandboxMode::WorkspaceWrite,
        approval: ApprovalPolicy::Ask,
        description: "Write inside the workspace and permitted temporary directories; wider retries require approval.",
    },
    PermissionPreset {
        name: "danger-full-access",
        sandbox: SandboxMode::DangerFullAccess,
        approval: ApprovalPolicy::Never,
        description: "Full file access without approval prompts.",
    },
];

pub(crate) fn preset(name: &str) -> Option<&'static PermissionPreset> {
    PRESETS.iter().find(|preset| preset.name == name)
}

pub(crate) fn settings_registration(base: Value) -> SettingsRegistration {
    let choices = PRESETS.iter().map(|preset| preset.name).collect::<Vec<_>>();
    let mut refs = Map::new();
    for (index, choice) in choices.iter().enumerate() {
        refs.insert(
            (index + 1).to_string(),
            json!({"type": "const", "value": choice}),
        );
    }
    let union = choices.len() + 1;
    refs.insert(
        union.to_string(),
        json!({"type": "union", "list": (1..=choices.len()).collect::<Vec<_>>() }),
    );
    let object = union + 1;
    refs.insert(
        object.to_string(),
        json!({"type": "object", "dict": {"defaultPreset": union}}),
    );
    let validator_choices = choices.clone();
    SettingsRegistration::new(
        PERMISSION_SETTINGS_NAMESPACE,
        json!({"uid": object, "refs": refs}),
        json!({"defaultPreset": DEFAULT_PERMISSION_PRESET}),
        base,
    )
    // A change is intentionally pinned only while creating a session. It does
    // not mutate a current session's sandbox or approval policy.
    .with_applies(SettingsApplies::Restart)
    .with_validator(Arc::new(move |value| {
        let valid = value.as_object().is_some_and(|object| {
            object.len() == 1
                && object
                    .get("defaultPreset")
                    .and_then(Value::as_str)
                    .is_some_and(|candidate| validator_choices.contains(&candidate))
        });
        if valid {
            Ok(())
        } else {
            Err(TessivumError::new(
                "INVALID_SETTINGS_VALUE",
                format!(
                    "permission.defaultPreset must be one of {}",
                    validator_choices.join(", ")
                ),
                "settings",
                json!({"namespace": PERMISSION_SETTINGS_NAMESPACE, "field": "defaultPreset"}),
            ))
        }
    }))
}

#[derive(Default)]
pub(crate) struct PermissionKnobs {
    preset: Option<String>,
    sandbox: Option<SandboxMode>,
    approval: Option<ApprovalPolicy>,
}

impl PermissionKnobs {
    pub(crate) fn preset(&self) -> Option<&str> {
        self.preset.as_deref()
    }

    pub(crate) fn sandbox(&self) -> Option<SandboxMode> {
        self.sandbox
    }

    pub(crate) fn approval(&self) -> Option<ApprovalPolicy> {
        self.approval
    }

    pub(crate) fn select_preset(&mut self, name: impl Into<String>) {
        self.preset = Some(name.into());
    }
}

pub(crate) fn fold(events: &[SessionEvent]) -> PermissionKnobs {
    let mut state = PermissionKnobs::default();
    for event in events {
        apply(&mut state, event);
    }
    state
}

/// Renders the current durable policy as the model-visible runtime snapshot.
pub(crate) fn runtime_context(events: &[SessionEvent], workspace: Option<&str>) -> String {
    let state = fold(events);
    let policy = match state.sandbox.unwrap_or(SandboxMode::WorkspaceWrite) {
        SandboxMode::ReadOnly => "Current DSH file policy: read-only. Any available operation enforced by the DSH file sandbox cannot modify files in the standing mode. Do not refuse a required modification from this policy alone: try an available tool normally and follow any denial and escalation guidance it returns.".to_owned(),
        SandboxMode::WorkspaceWrite => format!(
            "Current DSH file policy: workspace-write. Any available operation enforced by the DSH file sandbox may modify files under the session workspace: {}. Some platform temporary areas may also be writable.",
            serde_json::to_string(workspace.unwrap_or(".")).expect("workspace path serializes"),
        ),
        SandboxMode::DangerFullAccess => "Current DSH file policy: danger-full-access. The DSH file sandbox does not restrict file modifications by available operations.".to_owned(),
    };
    let approval = match state.approval.unwrap_or(ApprovalPolicy::Ask) {
        ApprovalPolicy::Ask => "Approval policy: ask. Operations that require approval may ask through the configured answerers; without an available answerer, the request fails closed.",
        ApprovalPolicy::Never => "Approval prompts are disabled in this session: actions that require approval are rejected automatically — do not request sandbox escalation (do not set `sandbox_permissions`).",
    };
    let time_zone = events.iter().rev().find_map(|event| {
        (event.event_type == "user/message")
            .then(|| event.data.get("source")?.get("clientTimeZone")?.as_str())
            .flatten()
    });
    let browser_time = time_zone.map_or_else(
        || "Browser time zone for this request: unavailable. Ask the user to clarify otherwise-unqualified dates and times.".to_owned(),
        |time_zone| format!("Browser time zone for this request: {time_zone}. Interpret otherwise-unqualified dates and times in this zone."),
    );
    format!("Current runtime context. This snapshot supersedes earlier runtime-context snapshots.\n\n{policy}\n\n{approval}\n\n{browser_time}")
}

/// Applies the three durable permission facts. Malformed or unknown values do
/// not alter the last valid fact, preserving the fail-closed defaults.
pub(crate) fn apply(state: &mut PermissionKnobs, event: &SessionEvent) -> bool {
    match event.event_type.as_str() {
        "permission/preset" => {
            if let Some(name) = event.data.get("preset").and_then(Value::as_str) {
                state.preset = Some(name.to_owned());
            }
        }
        "sandbox/mode" => match event.data.get("mode").and_then(Value::as_str) {
            Some("read-only") => state.sandbox = Some(SandboxMode::ReadOnly),
            Some("workspace-write") => state.sandbox = Some(SandboxMode::WorkspaceWrite),
            Some("danger-full-access") => state.sandbox = Some(SandboxMode::DangerFullAccess),
            _ => {}
        },
        "approval/policy" => match event.data.get("policy").and_then(Value::as_str) {
            Some("ask") => state.approval = Some(ApprovalPolicy::Ask),
            Some("never") => state.approval = Some(ApprovalPolicy::Never),
            _ => {}
        },
        _ => return false,
    }
    true
}

pub(crate) fn current(state: &PermissionKnobs) -> &'static str {
    let sandbox = state.sandbox.unwrap_or(SandboxMode::WorkspaceWrite);
    let approval = state.approval.unwrap_or(ApprovalPolicy::Ask);
    if let Some(selected) = state.preset.as_deref().and_then(preset) {
        if selected.sandbox == sandbox && selected.approval == approval {
            return selected.name;
        }
    }
    PRESETS
        .iter()
        .find(|preset| preset.sandbox == sandbox && preset.approval == approval)
        .map_or(CUSTOM_PERMISSION_PRESET, |preset| preset.name)
}

pub(crate) fn select(state: &PermissionKnobs) -> Value {
    let current_value = current(state);
    let mut options = PRESETS
        .iter()
        .map(|preset| {
            json!({
                "value": preset.name,
                "name": preset.name,
                "description": preset.description,
            })
        })
        .collect::<Vec<_>>();
    if current_value == CUSTOM_PERMISSION_PRESET {
        options.push(json!({
            "value": CUSTOM_PERMISSION_PRESET,
            "name": "Custom",
            "description": "Current sandbox and approval settings do not match a preset.",
        }));
    }
    json!({"options": options, "currentValue": current_value})
}

pub(crate) fn preset_names() -> impl Iterator<Item = &'static str> {
    PRESETS.iter().map(|preset| preset.name)
}

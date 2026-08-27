use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard},
};

use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};

use crate::{
    code_runtime::{CodeRunRequest, CodeRuntime, ProcessCodeRuntime, ProcessCodeRuntimeConfig},
    host::{HostNotification, HostRemoteEvent},
    SessionId, TessivumError,
};

const MAX_CORDIS_VALUE_BYTES: usize = 64 * 1024;
const MAX_INSPECT_PROVIDERS: usize = 64;
const MAX_INSPECT_METHODS: usize = 64;
const MAX_WAITING_FOR: usize = 64;

#[derive(Clone)]
pub struct DynamicCordisRegistry {
    inner: Arc<Mutex<State>>,
    notices: broadcast::Sender<HostNotification>,
    code: ProcessCodeRuntime,
}


#[derive(Default)]
struct State {
    next_plugin: u64,
    next_package: u64,
    next_run: u64,
    next_request: u64,
    next_inspect_request: u64,
    plugins: Vec<Plugin>,
    inspect_manifest: Vec<InspectProvider>,
    pending_inspects: BTreeMap<String, PendingInspect>,
}

struct Plugin {
    plugin_id: String,
    session_id: SessionId,
    packages: Vec<Package>,
    approved_client_packages: BTreeSet<String>,
    approve_future_versions: bool,
    current_package_id: Option<String>,
    next_package_id: Option<String>,
    active_run: Option<Run>,
    pending: Option<PendingRun>,
    latest_run: Option<Value>,
}

#[derive(Clone)]
struct Package {
    package_id: String,
    name: String,
    purpose: String,
    host_code: Option<String>,
    client_code: Option<String>,
}

#[derive(Clone)]
struct Run {
    plugin_run_id: String,
    package_id: String,
    mode: String,
    started_for_request: Option<String>,
    settled: bool,
    handlers: BTreeSet<String>,
    started: bool,
    render_failure: Option<Value>,
    reported_runtime_errors: BTreeSet<String>,
}

#[derive(Clone)]
struct PendingRun {
    request_id: String,
    plugin_run_id: String,
    package_id: String,
    mode: String,
    requires_approval: bool,
}

#[derive(Clone)]
struct InspectProvider {
    id: String,
    description: String,
    methods: Vec<InspectMethod>,
}

#[derive(Clone)]
struct InspectMethod {
    name: String,
    description: String,
    input_schema: Value,
    output_schema: Value,
}

struct PendingInspect {
    agent_id: SessionId,
    output_schema: Value,
    sender: oneshot::Sender<Result<Value, TessivumError>>,
}
impl DynamicCordisRegistry {
    pub fn new(notices: broadcast::Sender<HostNotification>) -> Result<Self, TessivumError> {
        let code = ProcessCodeRuntimeConfig::ptc_javascript()
            .and_then(ProcessCodeRuntime::new)
            .map_err(|error| {
                TessivumError::new(
                    error
                        .diagnostic_code()
                        .unwrap_or("CORDIS_RUNTIME_CONFIG_FAILED"),
                    error.to_string(),
                    "cordis",
                    Value::Null,
                )
            })?;
        Ok(Self {
            inner: Arc::new(Mutex::new(State::default())),
            notices,
            code,
        })
    }


    pub fn inventory(&self) -> Value {
        let state = lock(&self.inner);
        Value::Array(state.plugins.iter().map(inventory_row).collect())
    }
    pub fn pending_request_owner(&self, request_id: &str) -> Option<SessionId> {
        lock(&self.inner)
            .plugins
            .iter()
            .find(|plugin| {
                plugin
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == request_id)
            })
            .map(|plugin| plugin.session_id.clone())
    }

    pub async fn run_host_half(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let agent_id = required_str(args, "agentId")?;
        let plugin_id = required_str(args, "pluginId")?;
        let package_id = required_str(args, "packageId")?;
        let mode = required_mode(args)?;
        let request_id = required_nullable_str(args, "requestId")?;
        let approve_future_versions = required_bool(args, "approveFutureVersions")?;

        let direct_run_id = if request_id.is_none() {
            let mut state = lock(&self.inner);
            state.next_run += 1;
            Some(format!("run-{}", state.next_run))
        } else {
            None
        };
        let (definition, run_id, previous_run) = {
            let mut state = lock(&self.inner);
            let plugin = owned_plugin_mut(&mut state, agent_id, plugin_id)?;
            let definition = package(plugin, package_id)?.clone();
            validate_mode(plugin, package_id, mode)?;
            if let Some(request_id) = request_id {
                let pending = plugin
                    .pending
                    .as_ref()
                    .filter(|pending| {
                        pending.request_id == request_id
                            && pending.package_id == package_id
                            && pending.mode == mode
                    })
                    .cloned()
                    .ok_or_else(|| {
                        cordis_error(
                            "CORDIS_REQUEST_MISSING",
                            "run request no longer authorizes this activation",
                        )
                    })?;
                if pending.requires_approval {
                    plugin.approved_client_packages.insert(package_id.into());
                    if approve_future_versions {
                        plugin.approve_future_versions = true;
                    }
                }
                if let Some(run) = plugin.active_run.as_ref() {
                    if run.plugin_run_id == pending.plugin_run_id {
                        if run.started {
                            return Ok(host_started(
                                plugin_id,
                                package_id,
                                &run.plugin_run_id,
                                false,
                            ));
                        }
                        return Err(cordis_error(
                            "CORDIS_TRANSITION_IN_FLIGHT",
                            "dynamic Cordis run is already starting",
                        ));
                    }
                }
                let prior = plugin.active_run.take();
                plugin.next_package_id = Some(package_id.into());
                plugin.active_run = Some(Run::starting(
                    pending.plugin_run_id.clone(),
                    package_id.into(),
                    mode.into(),
                    Some(request_id.into()),
                ));
                (definition, pending.plugin_run_id, prior)
            } else {
                if plugin.pending.is_some() {
                    return Err(cordis_error(
                        "CORDIS_TRANSITION_IN_FLIGHT",
                        "dynamic Cordis plugin has a pending run request",
                    ));
                }
                if let Some(run) = plugin.active_run.as_ref() {
                    if run.package_id == package_id && run.started {
                        return Ok(host_started(
                            plugin_id,
                            package_id,
                            &run.plugin_run_id,
                            false,
                        ));
                    }
                    if !run.started {
                        return Err(cordis_error(
                            "CORDIS_TRANSITION_IN_FLIGHT",
                            "dynamic Cordis run is already starting",
                        ));
                    }
                }
                if definition.client_code.is_some() {
                    plugin.approved_client_packages.insert(package_id.into());
                    if approve_future_versions {
                        plugin.approve_future_versions = true;
                    }
                }
                let run_id = direct_run_id.clone().expect("direct run id was minted");
                let prior = plugin.active_run.take();
                plugin.next_package_id = Some(package_id.into());
                plugin.active_run = Some(Run::starting(
                    run_id.clone(),
                    package_id.into(),
                    mode.into(),
                    None,
                ));
                (definition, run_id, prior)
            }
        };
        if let Some(run) = previous_run {
            self.retract(plugin_id, &run);
        }
        let handlers = match self.start_host(&definition).await {
            Ok(handlers) => handlers,
            Err(message) => {
                let mut state = lock(&self.inner);
                if let Ok(plugin) = owned_plugin_mut(&mut state, agent_id, plugin_id) {
                    if plugin
                        .active_run
                        .as_ref()
                        .is_some_and(|run| run.plugin_run_id == run_id)
                    {
                        plugin.active_run = None;
                        plugin.latest_run = Some(attempt(
                            package_id,
                            &run_id,
                            mode,
                            "failed",
                            &definition,
                            Some(("host-load", &message)),
                        ));
                    }
                }
                return Ok(json!({"ok":false,"message":message}));
            }
        };
        let mut state = lock(&self.inner);
        let plugin = owned_plugin_mut(&mut state, agent_id, plugin_id)?;
        {
            let run = plugin
                .active_run
                .as_mut()
                .filter(|run| run.plugin_run_id == run_id)
                .ok_or_else(|| {
                    cordis_error(
                        "CORDIS_RUN_STALE",
                        "dynamic Cordis activation was cancelled while starting",
                    )
                })?;
            run.handlers = handlers;
            run.started = true;
        }
        let status = if definition.client_code.is_some() {
            "client-pending"
        } else {
            "running"
        };
        plugin.latest_run = Some(attempt(
            package_id,
            &run_id,
            mode,
            status,
            &definition,
            None,
        ));
        if definition.client_code.is_none() {
            plugin.current_package_id = Some(package_id.into());
            plugin.next_package_id = None;
        }
        drop(state);
        self.notify("cordis/dynamic-package", json!({
            "pluginId": plugin_id, "packageId": package_id, "pluginRunId": run_id, "name": definition.name,
        }));
        Ok(host_started(plugin_id, package_id, &run_id, true))
    }

    pub fn get_client_code(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let agent_id = required_str(args, "agentId")?;
        let plugin_id = required_str(args, "pluginId")?;
        let plugin_run_id = required_str(args, "pluginRunId")?;
        let state = lock(&self.inner);
        let plugin = owned_plugin(&state, agent_id, plugin_id)?;
        let run = plugin
            .active_run
            .as_ref()
            .filter(|run| run.started && run.plugin_run_id == plugin_run_id)
            .ok_or_else(|| {
                cordis_error(
                    "CORDIS_RUN_MISSING",
                    "dynamic Cordis activation was not found",
                )
            })?;
        let package = package(plugin, &run.package_id)?;
        let code = package.client_code.as_ref().ok_or_else(|| {
            cordis_error(
                "CORDIS_CLIENT_MISSING",
                "dynamic Cordis package has no Client half",
            )
        })?;
        Ok(
            json!({"code":code,"name":package.name,"pluginId":plugin_id,"packageId":package.package_id,"pluginRunId":plugin_run_id}),
        )
    }

    pub async fn resolve_request_run(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let request_id = required_str(args, "requestId")?;
        let resolution =
            checked_resolution(args.get("resolution").ok_or_else(|| {
                cordis_error("INVALID_CORDIS_REQUEST", "resolution is required")
            })?)?;
        let (plugin_id, run_to_retract, outcome, context) = {
            let mut state = lock(&self.inner);
            let Some(plugin) = state.plugins.iter_mut().find(|plugin| {
                plugin
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == request_id)
            }) else {
                return Ok(json!({"accepted":false}));
            };
            let pending = plugin
                .pending
                .as_ref()
                .expect("pending request exists")
                .clone();
            let run_matches = plugin.active_run.as_ref().is_some_and(|run| {
                run.started
                    && run.plugin_run_id == pending.plugin_run_id
                    && run.started_for_request.as_deref() == Some(request_id)
                    && !run.settled
            });
            if resolution.get("ok") == Some(&Value::Bool(true)) {
                if !run_matches
                    || resolution.get("pluginRunId").and_then(Value::as_str)
                        != Some(pending.plugin_run_id.as_str())
                {
                    return Ok(json!({"accepted":false}));
                }
                let definition = package(plugin, &pending.package_id)?.clone();
                plugin.pending = None;
                if let Some(run) = plugin.active_run.as_mut() {
                    run.settled = true;
                }
                plugin.current_package_id = Some(pending.package_id.clone());
                plugin.next_package_id = None;
                plugin.latest_run = Some(attempt(
                    &pending.package_id,
                    &pending.plugin_run_id,
                    &pending.mode,
                    "running",
                    &definition,
                    None,
                ));
                let context = format!(
                    "Cordis {} {}/{} ({}) completed successfully. currentPackageId is {}. Continue using the running Plugin.",
                    pending.mode,
                    plugin.plugin_id,
                    pending.package_id,
                    pending.plugin_run_id,
                    pending.package_id,
                );
                (
                    plugin.plugin_id.clone(),
                    None,
                    if pending.requires_approval {
                        "approved"
                    } else {
                        "completed"
                    },
                    context,
                )
            } else {
                let supplied = resolution.get("pluginRunId").and_then(Value::as_str);
                if supplied.is_some()
                    && (!run_matches || supplied != Some(pending.plugin_run_id.as_str()))
                {
                    return Ok(json!({"accepted":false}));
                }
                let definition = package(plugin, &pending.package_id)?.clone();
                plugin.pending = None;
                if let Some(run) = plugin.active_run.as_mut() {
                    run.settled = true;
                }
                let reason = resolution
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("client-half-failed");
                let retract = if resolution.get("startedHere") != Some(&Value::Bool(false)) {
                    plugin.active_run.take()
                } else {
                    None
                };
                let message = resolution
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or(reason);
                plugin.latest_run = Some(attempt(
                    &pending.package_id,
                    &pending.plugin_run_id,
                    &pending.mode,
                    if reason == "rejected" {
                        "rejected"
                    } else {
                        "failed"
                    },
                    &definition,
                    Some((
                        if reason == "rejected" {
                            "approval"
                        } else {
                            "client-apply"
                        },
                        message,
                    )),
                ));
                let context = if reason == "rejected" {
                    format!(
                        "The user rejected Cordis {} {}/{} ({}). Do not request the same activation again unless the user asks.",
                        pending.mode, plugin.plugin_id, pending.package_id, pending.plugin_run_id,
                    )
                } else {
                    format!(
                        "Dynamic Cordis {} {}/{} ({}) failed after activation returned {}: {message}",
                        pending.mode,
                        plugin.plugin_id,
                        pending.package_id,
                        pending.plugin_run_id,
                        if pending.requires_approval {
                            "awaiting-approval"
                        } else {
                            "starting"
                        },
                    )
                };
                (
                    plugin.plugin_id.clone(),
                    retract,
                    if reason == "rejected" {
                        "rejected"
                    } else {
                        "failed"
                    },
                    context,
                )
            }
        };
        if let Some(run) = run_to_retract {
            self.retract(&plugin_id, &run);
        }
        self.notify(
            "cordis/request-run-resolved",
            json!({"requestId":request_id,"outcome":outcome}),
        );
        Ok(json!({"accepted":true,"_context":context}))
    }

    pub async fn settle_user_run(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let agent_id = required_str(args, "agentId")?;
        let plugin_id = required_str(args, "pluginId")?;
        let resolution =
            checked_resolution(args.get("resolution").ok_or_else(|| {
                cordis_error("INVALID_CORDIS_REQUEST", "resolution is required")
            })?)?;
        let (response, run_to_retract) = {
            let mut state = lock(&self.inner);
            let plugin = owned_plugin_mut(&mut state, agent_id, plugin_id)?;
            let Some(run) = plugin
                .active_run
                .as_ref()
                .filter(|run| run.started)
                .cloned()
            else {
                return Ok(
                    json!({"ok":false,"reason":"not-running","message":"dynamic Cordis plugin is not running"}),
                );
            };
            if run.started_for_request.is_some() {
                return Err(cordis_error(
                    "CORDIS_REQUEST_MANAGED",
                    "request-managed activation must be settled through resolveRequestRun",
                ));
            }
            if run.settled {
                return Err(cordis_error(
                    "CORDIS_SETTLEMENT_STALE",
                    "dynamic Cordis activation has already been settled",
                ));
            }
            if resolution.get("ok") == Some(&Value::Bool(true)) {
                if resolution.get("pluginRunId").and_then(Value::as_str)
                    != Some(run.plugin_run_id.as_str())
                {
                    return Ok(
                        json!({"ok":false,"reason":"client-half-failed","message":"activation is no longer active"}),
                    );
                }
                let definition = package(plugin, &run.package_id)?.clone();
                if let Some(active) = plugin.active_run.as_mut() {
                    active.settled = true;
                }
                plugin.current_package_id = Some(run.package_id.clone());
                plugin.next_package_id = None;
                plugin.latest_run = Some(attempt(
                    &run.package_id,
                    &run.plugin_run_id,
                    &run.mode,
                    "running",
                    &definition,
                    None,
                ));
                (run_response(plugin, &run), None)
            } else {
                let supplied = resolution.get("pluginRunId").and_then(Value::as_str);
                if supplied.is_some() && supplied != Some(run.plugin_run_id.as_str()) {
                    return Ok(
                        json!({"ok":false,"reason":"client-half-failed","message":"activation is no longer active"}),
                    );
                }
                let definition = package(plugin, &run.package_id)?.clone();
                let reason = resolution
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("client-half-failed");
                let message = resolution
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or(reason)
                    .to_owned();
                if let Some(active) = plugin.active_run.as_mut() {
                    active.settled = true;
                }
                let retract = if resolution.get("startedHere") != Some(&Value::Bool(false)) {
                    plugin.active_run.take()
                } else {
                    None
                };
                plugin.latest_run = Some(attempt(
                    &run.package_id,
                    &run.plugin_run_id,
                    &run.mode,
                    if reason == "rejected" {
                        "rejected"
                    } else {
                        "failed"
                    },
                    &definition,
                    Some((
                        if reason == "rejected" {
                            "approval"
                        } else {
                            "client-apply"
                        },
                        &message,
                    )),
                ));
                (
                    json!({"ok":false,"reason":reason,"message":message}),
                    retract,
                )
            }
        };
        if let Some(run) = run_to_retract {
            self.retract(plugin_id, &run);
        }
        Ok(response)
    }
    pub fn stop_from_panel(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let agent_id = required_str(args, "agentId")?;
        let plugin_id = required_str(args, "pluginId")?;
        self.stop_owned(agent_id, plugin_id)
    }

    pub fn undefine_from_panel(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let agent_id = required_str(args, "agentId")?;
        let plugin_id = required_str(args, "pluginId")?;
        let (removed, run) = {
            let mut state = lock(&self.inner);
            let index = state
                .plugins
                .iter()
                .position(|plugin| {
                    plugin.plugin_id == plugin_id && plugin.session_id.as_str() == agent_id
                })
                .ok_or_else(|| {
                    cordis_error(
                        "CORDIS_PLUGIN_MISSING",
                        "dynamic Cordis plugin was not found",
                    )
                })?;
            let plugin = state.plugins.remove(index);
            (true, plugin.active_run)
        };
        if let Some(run) = &run {
            self.retract(plugin_id, run);
        }
        Ok(json!({"ok":removed,"wasRunning":run.is_some()}))
    }

    pub fn sync_inspect_manifest(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let providers = args
            .get("providers")
            .and_then(Value::as_array)
            .ok_or_else(|| cordis_error("INVALID_CORDIS_MANIFEST", "providers must be an array"))?;
        if providers.len() > MAX_INSPECT_PROVIDERS {
            return Err(cordis_error(
                "CORDIS_PAYLOAD_TOO_LARGE",
                "too many inspect providers",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut manifest = Vec::with_capacity(providers.len());
        for provider in providers {
            let object = provider.as_object().ok_or_else(|| {
                cordis_error(
                    "INVALID_CORDIS_MANIFEST",
                    "inspect provider must be an object",
                )
            })?;
            let id = required_field(object, "id", "inspect provider")?.to_owned();
            let description = required_field(object, "description", "inspect provider")?.to_owned();
            if !ids.insert(id.clone()) {
                return Err(cordis_error(
                    "INVALID_CORDIS_MANIFEST",
                    "inspect provider ids must be unique",
                ));
            }
            let methods = object
                .get("methods")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    cordis_error(
                        "INVALID_CORDIS_MANIFEST",
                        "inspect provider methods must be an array",
                    )
                })?;
            if methods.len() > MAX_INSPECT_METHODS {
                return Err(cordis_error(
                    "CORDIS_PAYLOAD_TOO_LARGE",
                    "too many inspect methods",
                ));
            }
            let mut names = BTreeSet::new();
            let mut stored = Vec::with_capacity(methods.len());
            for method in methods {
                let method = method.as_object().ok_or_else(|| {
                    cordis_error(
                        "INVALID_CORDIS_MANIFEST",
                        "inspect method must be an object",
                    )
                })?;
                let name = required_field(method, "name", "inspect method")?.to_owned();
                let description =
                    required_field(method, "description", "inspect method")?.to_owned();
                if !names.insert(name.clone()) {
                    return Err(cordis_error(
                        "INVALID_CORDIS_MANIFEST",
                        "inspect method names must be unique",
                    ));
                }
                let input_schema = method.get("inputSchema").cloned().ok_or_else(|| {
                    cordis_error("INVALID_CORDIS_MANIFEST", "inspect inputSchema is required")
                })?;
                let output_schema = method.get("outputSchema").cloned().ok_or_else(|| {
                    cordis_error(
                        "INVALID_CORDIS_MANIFEST",
                        "inspect outputSchema is required",
                    )
                })?;
                stored.push(InspectMethod {
                    name,
                    description,
                    input_schema,
                    output_schema,
                });
            }
            manifest.push(InspectProvider {
                id,
                description,
                methods: stored,
            });
        }
        lock(&self.inner).inspect_manifest = manifest;
        Ok(Value::Null)
    }
    pub fn resolve_inspect_query(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let agent_id = required_str(args, "agentId")?;
        let request_id = required_str(args, "requestId")?;
        let resolution =
            checked_inspect_resolution(args.get("resolution").ok_or_else(|| {
                cordis_error("INVALID_CORDIS_REQUEST", "resolution is required")
            })?)?;
        let data = resolution.get("data").cloned().unwrap_or(Value::Null);
        let (pending, result) = {
            let mut state = lock(&self.inner);
            let Some(pending) = state.pending_inspects.get(request_id) else {
                return Ok(json!({"accepted":false}));
            };
            if pending.agent_id.as_str() != agent_id {
                return Ok(json!({"accepted":false}));
            }
            if resolution.get("ok") == Some(&Value::Bool(true))
                && !schema_allows(&pending.output_schema, &data)
            {
                return Ok(json!({"accepted":false}));
            }
            let pending = state
                .pending_inspects
                .remove(request_id)
                .expect("pending inspect exists");
            let result = if resolution.get("ok") == Some(&Value::Bool(true)) {
                Ok(data)
            } else {
                Err(cordis_error(
                    "CORDIS_INSPECT_CLIENT_FAILED",
                    resolution
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Client Cordis inspect query failed"),
                ))
            };
            (pending, result)
        };
        let _ = pending.sender.send(result);
        self.notify(
            "cordis/inspect-query-resolved",
            json!({"requestId":request_id}),
        );
        Ok(json!({"accepted":true}))
    }

    pub fn report_render_failure(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let agent_id = required_str(args, "agentId")?;
        let plugin_id = required_str(args, "pluginId")?;
        let run_id = required_str(args, "pluginRunId")?;
        let failure = checked_render_failure(
            args.get("failure")
                .ok_or_else(|| cordis_error("INVALID_CORDIS_REQUEST", "failure is required"))?,
        )?;
        let mut state = lock(&self.inner);
        let plugin = owned_plugin_mut(&mut state, agent_id, plugin_id)?;
        let first = {
            let run = plugin
                .active_run
                .as_mut()
                .filter(|run| run.started && run.plugin_run_id == run_id)
                .ok_or_else(|| {
                    cordis_error(
                        "CORDIS_RUN_MISSING",
                        "dynamic Cordis activation was not found",
                    )
                })?;
            let first = run.render_failure.is_none();
            run.render_failure = Some(failure.clone());
            first
        };
        if first {
            let latest = mark_runtime_failure(plugin, "client-render", &failure);
            plugin.latest_run = Some(latest);
        }
        Ok(Value::Null)
    }

    pub fn report_client_guard_failure(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let agent_id = required_str(args, "agentId")?;
        let plugin_id = required_str(args, "pluginId")?;
        let run_id = required_str(args, "pluginRunId")?;
        let failure = checked_error_details(
            args.get("failure")
                .ok_or_else(|| cordis_error("INVALID_CORDIS_REQUEST", "failure is required"))?,
        )?;
        let mut state = lock(&self.inner);
        let plugin = owned_plugin_mut(&mut state, agent_id, plugin_id)?;
        let run = plugin
            .active_run
            .as_mut()
            .filter(|run| run.started && run.plugin_run_id == run_id)
            .ok_or_else(|| {
                cordis_error(
                    "CORDIS_RUN_MISSING",
                    "dynamic Cordis activation was not found",
                )
            })?;
        run.reported_runtime_errors
            .insert(format!("Client guard\0{}", failure["message"]));
        Ok(Value::Null)
    }
    pub async fn invoke(&self, args: &Value) -> Result<Value, TessivumError> {
        ensure_bounded(args)?;
        let plugin_id = required_str(args, "pluginId")?;
        let run_id = required_str(args, "pluginRunId")?;
        let method = required_str(args, "method")?;
        let arguments = args
            .get("args")
            .cloned()
            .ok_or_else(|| cordis_error("INVALID_CORDIS_REQUEST", "args is required"))?;
        let (source, known) = {
            let state = lock(&self.inner);
            let Some(plugin) = state
                .plugins
                .iter()
                .find(|plugin| plugin.plugin_id == plugin_id)
            else {
                return Ok(invoke_failure(
                    "plugin-not-running",
                    "dynamic Cordis plugin is not running",
                ));
            };
            let Some(run) = plugin.active_run.as_ref().filter(|run| run.started) else {
                return Ok(invoke_failure(
                    "plugin-not-running",
                    "dynamic Cordis plugin is not running",
                ));
            };
            if run.plugin_run_id != run_id {
                return Ok(invoke_failure(
                    "stale-run",
                    "dynamic Cordis activation is no longer active",
                ));
            }
            if !run.handlers.contains(method) {
                return Ok(invoke_failure(
                    "method-not-found",
                    "dynamic Cordis plugin registered no Host method with that name",
                ));
            }
            (package(plugin, &run.package_id)?.host_code.clone(), true)
        };
        if !known {
            return Ok(invoke_failure(
                "method-not-found",
                "dynamic Cordis plugin registered no Host method with that name",
            ));
        }
        let Some(source) = source else {
            return Ok(invoke_failure(
                "method-not-found",
                "dynamic Cordis package has no Host half",
            ));
        };
        match self.call_host(&source, method, &arguments).await {
            Ok(value) => Ok(json!({"ok":true,"value":value})),
            Err(message) => {
                let mut state = lock(&self.inner);
                if let Some(plugin) = state
                    .plugins
                    .iter_mut()
                    .find(|plugin| plugin.plugin_id == plugin_id)
                {
                    if let Some(run) = plugin
                        .active_run
                        .as_mut()
                        .filter(|run| run.plugin_run_id == run_id)
                    {
                        run.reported_runtime_errors
                            .insert(format!("Host handler\0{method}\0{message}"));
                    }
                }
                Ok(json!({"ok":false,"code":"handler-error","message":message}))
            }
        }
    }


    fn stop_owned(&self, agent_id: &str, plugin_id: &str) -> Result<Value, TessivumError> {
        let run = {
            let mut state = lock(&self.inner);
            let plugin = owned_plugin_mut(&mut state, agent_id, plugin_id)?;
            if plugin.active_run.is_none() && plugin.pending.is_none() {
                return Ok(
                    json!({"ok":false,"reason":"not-running","message":"dynamic Cordis plugin is not running"}),
                );
            }
            plugin.pending = None;
            plugin.next_package_id = None;
            let run = plugin.active_run.take();
            if let Some(latest) = plugin.latest_run.as_mut() {
                latest["status"] = json!("stopped");
            }
            run
        };
        if let Some(run) = &run {
            self.retract(plugin_id, run);
        }
        Ok(json!({"ok":true}))
    }

    async fn start_host(&self, definition: &Package) -> Result<BTreeSet<String>, String> {
        let Some(source) = &definition.host_code else {
            return Ok(BTreeSet::new());
        };
        let program = format!(
            "const handlers = new Map();\nconst harness = {{ handle(method, fn) {{ if (typeof method !== 'string' || method.length === 0 || typeof fn !== 'function') throw new Error('harness.handle(method, fn) needs a non-empty method and function'); if (handlers.has(method)) throw new Error('duplicate Host handler: ' + method); handlers.set(method, fn); }} }};\nconst plugin = await (async () => {{\n{source}\n}})();\nif (typeof plugin === 'function') await plugin({{}}); else if (plugin && typeof plugin.apply === 'function') await plugin.apply({{}}); else throw new Error('Host half must return a Plugin function or an object with apply(ctx)');\nreturn [...handlers.keys()];"
        );
        let result = self
            .code
            .run(CodeRunRequest::new(program, vec![]))
            .await
            .map_err(|error| error.to_string())?;
        if let Some(error) = result.error {
            return Err(error.message);
        }
        let values = result
            .value
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| "Host half did not return a handler list".to_owned())?;
        let mut handlers = BTreeSet::new();
        for value in values {
            let Some(name) = value
                .as_str()
                .filter(|name| !name.is_empty() && name.len() <= 128)
            else {
                return Err("Host half registered an invalid handler name".into());
            };
            if !handlers.insert(name.into()) {
                return Err("Host half registered a duplicate handler".into());
            }
        }
        Ok(handlers)
    }

    async fn call_host(&self, source: &str, method: &str, args: &Value) -> Result<Value, String> {
        let encoded_method =
            serde_json::to_string(method).map_err(|_| "invalid Host method".to_owned())?;
        let encoded_args =
            serde_json::to_string(args).map_err(|_| "invalid Host arguments".to_owned())?;
        let program = format!(
            "const handlers = new Map();\nconst harness = {{ handle(method, fn) {{ if (typeof method !== 'string' || typeof fn !== 'function') throw new Error('harness.handle(method, fn) needs a string and function'); handlers.set(method, fn); }} }};\nconst plugin = await (async () => {{\n{source}\n}})();\nif (typeof plugin === 'function') await plugin({{}}); else if (plugin && typeof plugin.apply === 'function') await plugin.apply({{}}); else throw new Error('Host half must return a Plugin function or an object with apply(ctx)');\nconst handler = handlers.get({encoded_method});\nif (!handler) throw new Error('Host handler was not registered during invocation');\nreturn await handler({encoded_args});"
        );
        let result = self
            .code
            .run(CodeRunRequest::new(program, vec![]))
            .await
            .map_err(|error| error.to_string())?;
        if let Some(error) = result.error {
            return Err(error.message);
        }
        let value = result.value.unwrap_or(Value::Null);
        ensure_bounded(&value).map_err(|error| error.message)?;
        Ok(value)
    }

    fn retract(&self, plugin_id: &str, run: &Run) {
        self.notify("cordis/dynamic-retract", json!({"pluginId":plugin_id,"packageId":run.package_id,"pluginRunId":run.plugin_run_id}));
    }

    fn notify(&self, event: &str, payload: Value) {
        let _ = self
            .notices
            .send(HostNotification::RemoteEvent(HostRemoteEvent {
                event: event.into(),
                args: vec![payload],
            }));
    }
}
impl Run {
    fn starting(
        plugin_run_id: String,
        package_id: String,
        mode: String,
        started_for_request: Option<String>,
    ) -> Self {
        Self {
            plugin_run_id,
            package_id,
            mode,
            started_for_request,
            settled: false,
            handlers: BTreeSet::new(),
            started: false,
            render_failure: None,
            reported_runtime_errors: BTreeSet::new(),
        }
    }
}


fn inventory_row(plugin: &Plugin) -> Value {
    let mut row = json!({"pluginId":plugin.plugin_id,"agentId":plugin.session_id,"packages":plugin.packages.iter().map(|package| json!({"packageId":package.package_id,"name":package.name,"purpose":package.purpose,"hasHostHalf":package.host_code.is_some(),"hasClientHalf":package.client_code.is_some()})).collect::<Vec<_>>()});
    if let Some(value) = &plugin.current_package_id {
        row["currentPackageId"] = json!(value);
    }
    if let Some(value) = &plugin.next_package_id {
        row["nextPackageId"] = json!(value);
    }
    if let Some(run) = plugin.active_run.as_ref().filter(|run| run.started) {
        row["activeRun"] = json!({"pluginRunId":run.plugin_run_id,"packageId":run.package_id});
    }
    if let Some(latest) = &plugin.latest_run {
        row["latestRun"] = latest.clone();
    }
    row
}

fn attempt(
    package_id: &str,
    run_id: &str,
    mode: &str,
    status: &str,
    package: &Package,
    error: Option<(&str, &str)>,
) -> Value {
    let host = if package.host_code.is_some() {
        if status == "failed" {
            "failed"
        } else {
            "running"
        }
    } else {
        "absent"
    };
    let client = if package.client_code.is_some() {
        match status {
            "awaiting-approval" | "starting-host" | "client-pending" => "pending",
            "failed" | "rejected" => "failed",
            "stopped" => "stopped",
            _ => "running",
        }
    } else {
        "absent"
    };
    let mut value = json!({"pluginRunId":run_id,"packageId":package_id,"mode":mode,"status":status,"host":{"status":host,"waitingFor":[]},"client":{"status":client,"waitingFor":[]}});
    if let Some((phase, message)) = error {
        value["error"] = json!({"phase":phase,"message":message,"pluginId":"","packageId":package_id,"pluginRunId":run_id});
    }
    value
}

fn mark_runtime_failure(plugin: &Plugin, phase: &str, failure: &Value) -> Value {
    let Some(run) = plugin.active_run.as_ref() else {
        return Value::Null;
    };
    let package = package(plugin, &run.package_id).expect("active package exists");
    attempt(
        &run.package_id,
        &run.plugin_run_id,
        &run.mode,
        "failed",
        package,
        Some((
            phase,
            failure
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("runtime failure"),
        )),
    )
}

fn host_started(plugin_id: &str, package_id: &str, run_id: &str, started_here: bool) -> Value {
    json!({"ok":true,"pluginId":plugin_id,"packageId":package_id,"pluginRunId":run_id,"waitingFor":[],"startedHere":started_here})
}

fn run_response(plugin: &Plugin, run: &Run) -> Value {
    json!({"ok":true,"status":"running","pluginId":plugin.plugin_id,"packageId":run.package_id,"pluginRunId":run.plugin_run_id,"waitingFor":[],"currentPackageId":plugin.current_package_id,"mode":run.mode})
}

fn invoke_failure(code: &str, message: &str) -> Value {
    json!({"ok":false,"code":code,"message":message})
}


fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, TessivumError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| {
            cordis_error(
                "INVALID_CORDIS_REQUEST",
                &format!("{key} must be a non-empty bounded string"),
            )
        })
}

fn required_nullable_str<'a>(
    value: &'a Value,
    key: &str,
) -> Result<Option<&'a str>, TessivumError> {
    match value.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.is_empty() && value.len() <= 256 => Ok(Some(value)),
        _ => Err(cordis_error(
            "INVALID_CORDIS_REQUEST",
            &format!("{key} must be null or a non-empty bounded string"),
        )),
    }
}

fn required_bool(value: &Value, key: &str) -> Result<bool, TessivumError> {
    value.get(key).and_then(Value::as_bool).ok_or_else(|| {
        cordis_error(
            "INVALID_CORDIS_REQUEST",
            &format!("{key} must be a boolean"),
        )
    })
}

fn required_mode(value: &Value) -> Result<&str, TessivumError> {
    match required_str(value, "mode")? {
        "run" | "update" => Ok(required_str(value, "mode")?),
        _ => Err(cordis_error(
            "INVALID_CORDIS_MODE",
            "mode must be run or update",
        )),
    }
}

fn required_field<'a>(
    value: &'a serde_json::Map<String, Value>,
    key: &str,
    scope: &str,
) -> Result<&'a str, TessivumError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && value.len() <= 4096)
        .ok_or_else(|| {
            cordis_error(
                "INVALID_CORDIS_MANIFEST",
                &format!("{scope}.{key} must be a non-empty bounded string"),
            )
        })
}


fn package<'a>(plugin: &'a Plugin, package_id: &str) -> Result<&'a Package, TessivumError> {
    plugin
        .packages
        .iter()
        .find(|value| value.package_id == package_id)
        .ok_or_else(|| {
            cordis_error(
                "CORDIS_PACKAGE_MISSING",
                "dynamic Cordis package was not found",
            )
        })
}

fn validate_mode(plugin: &Plugin, package_id: &str, mode: &str) -> Result<(), TessivumError> {
    match (mode, plugin.current_package_id.as_deref()) {
        ("update", None) => Err(cordis_error(
            "INVALID_CORDIS_MODE",
            "plugin has no successful version; use mode run",
        )),
        ("update", Some(current)) if current == package_id => Err(cordis_error(
            "INVALID_CORDIS_MODE",
            "package is already current; use mode run",
        )),
        ("run", Some(current)) if current != package_id => Err(cordis_error(
            "INVALID_CORDIS_MODE",
            "package differs from current version; use mode update",
        )),
        ("run" | "update", _) => Ok(()),
        _ => Err(cordis_error(
            "INVALID_CORDIS_MODE",
            "mode must be run or update",
        )),
    }
}

fn checked_resolution(value: &Value) -> Result<Value, TessivumError> {
    let object = value
        .as_object()
        .ok_or_else(|| cordis_error("INVALID_CORDIS_REQUEST", "resolution must be an object"))?;
    let ok = object
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or_else(|| cordis_error("INVALID_CORDIS_REQUEST", "resolution.ok must be a boolean"))?;
    let allowed = if ok {
        ["ok", "pluginRunId", "waitingFor"].as_slice()
    } else {
        [
            "ok",
            "reason",
            "pluginRunId",
            "startedHere",
            "message",
            "stack",
        ]
        .as_slice()
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(cordis_error(
            "INVALID_CORDIS_REQUEST",
            "resolution has unsupported fields",
        ));
    }
    if ok {
        required_str(value, "pluginRunId")?;
        if let Some(waiting) = object.get("waitingFor") {
            checked_string_list(waiting, "resolution.waitingFor")?;
        }
    } else {
        match required_str(value, "reason")? {
            "rejected" | "host-half-failed" | "client-half-failed" => {}
            _ => {
                return Err(cordis_error(
                    "INVALID_CORDIS_REQUEST",
                    "resolution.reason is invalid",
                ))
            }
        }
        for key in ["pluginRunId", "message", "stack"] {
            if let Some(value) = object.get(key) {
                value
                    .as_str()
                    .filter(|value| value.len() <= MAX_CORDIS_VALUE_BYTES)
                    .ok_or_else(|| {
                        cordis_error(
                            "INVALID_CORDIS_REQUEST",
                            &format!("resolution.{key} must be a bounded string"),
                        )
                    })?;
            }
        }
        if let Some(value) = object.get("startedHere") {
            value.as_bool().ok_or_else(|| {
                cordis_error(
                    "INVALID_CORDIS_REQUEST",
                    "resolution.startedHere must be a boolean",
                )
            })?;
        }
    }
    Ok(value.clone())
}

fn checked_inspect_resolution(value: &Value) -> Result<Value, TessivumError> {
    let object = value
        .as_object()
        .ok_or_else(|| cordis_error("INVALID_CORDIS_REQUEST", "resolution must be an object"))?;
    let ok = object
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or_else(|| cordis_error("INVALID_CORDIS_REQUEST", "resolution.ok must be a boolean"))?;
    let allowed = if ok {
        ["ok", "data"].as_slice()
    } else {
        ["ok", "reason", "message"].as_slice()
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(cordis_error(
            "INVALID_CORDIS_REQUEST",
            "resolution has unsupported fields",
        ));
    }
    if ok {
        if !object.contains_key("data") {
            return Err(cordis_error(
                "INVALID_CORDIS_REQUEST",
                "resolution.data is required",
            ));
        }
        ensure_bounded(object.get("data").expect("checked data"))?;
    } else {
        match required_str(value, "reason")? {
            "provider-missing" | "method-missing" | "invalid-input" | "provider-error"
            | "cancelled" => {}
            _ => {
                return Err(cordis_error(
                    "INVALID_CORDIS_REQUEST",
                    "resolution.reason is invalid",
                ))
            }
        };
        required_str(value, "message")?;
    }
    Ok(value.clone())
}

fn checked_render_failure(value: &Value) -> Result<Value, TessivumError> {
    let object = value
        .as_object()
        .ok_or_else(|| cordis_error("INVALID_CORDIS_REQUEST", "failure must be an object"))?;
    if object
        .keys()
        .any(|key| !["slot", "message", "stack", "abdicated"].contains(&key.as_str()))
    {
        return Err(cordis_error(
            "INVALID_CORDIS_REQUEST",
            "failure has unsupported fields",
        ));
    }
    required_str(value, "slot")?;
    required_str(value, "message")?;
    required_bool(value, "abdicated")?;
    if let Some(stack) = object.get("stack") {
        stack
            .as_str()
            .filter(|value| value.len() <= MAX_CORDIS_VALUE_BYTES)
            .ok_or_else(|| {
                cordis_error(
                    "INVALID_CORDIS_REQUEST",
                    "failure.stack must be a bounded string",
                )
            })?;
    }
    Ok(value.clone())
}

fn checked_error_details(value: &Value) -> Result<Value, TessivumError> {
    let object = value
        .as_object()
        .ok_or_else(|| cordis_error("INVALID_CORDIS_REQUEST", "failure must be an object"))?;
    if object
        .keys()
        .any(|key| !["message", "stack"].contains(&key.as_str()))
    {
        return Err(cordis_error(
            "INVALID_CORDIS_REQUEST",
            "failure has unsupported fields",
        ));
    }
    required_str(value, "message")?;
    if let Some(stack) = object.get("stack") {
        stack
            .as_str()
            .filter(|value| value.len() <= MAX_CORDIS_VALUE_BYTES)
            .ok_or_else(|| {
                cordis_error(
                    "INVALID_CORDIS_REQUEST",
                    "failure.stack must be a bounded string",
                )
            })?;
    }
    Ok(value.clone())
}

fn checked_string_list(value: &Value, field: &str) -> Result<(), TessivumError> {
    let values = value
        .as_array()
        .filter(|values| values.len() <= MAX_WAITING_FOR)
        .ok_or_else(|| {
            cordis_error(
                "INVALID_CORDIS_REQUEST",
                &format!("{field} must be a bounded string array"),
            )
        })?;
    if values.iter().any(|value| {
        value
            .as_str()
            .is_none_or(|value| value.is_empty() || value.len() > 256)
    }) {
        return Err(cordis_error(
            "INVALID_CORDIS_REQUEST",
            &format!("{field} must contain bounded non-empty strings"),
        ));
    }
    Ok(())
}

fn schema_allows(schema: &Value, value: &Value) -> bool {
    let Some(schema) = schema.as_object() else {
        return true;
    };
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("number") => value.is_number(),
        Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
        Some("boolean") => value.is_boolean(),
        Some("null") => value.is_null(),
        _ => true,
    }
}

fn ensure_bounded(value: &Value) -> Result<(), TessivumError> {
    if serde_json::to_vec(value).map_or(usize::MAX, |value| value.len()) > MAX_CORDIS_VALUE_BYTES {
        Err(cordis_error(
            "CORDIS_PAYLOAD_TOO_LARGE",
            "dynamic Cordis payload exceeds the 64 KiB limit",
        ))
    } else {
        Ok(())
    }
}

fn owned_plugin<'a>(
    state: &'a State,
    session_id: &str,
    plugin_id: &str,
) -> Result<&'a Plugin, TessivumError> {
    state
        .plugins
        .iter()
        .find(|plugin| plugin.plugin_id == plugin_id && plugin.session_id.as_str() == session_id)
        .ok_or_else(|| {
            cordis_error(
                "CORDIS_PLUGIN_MISSING",
                "dynamic Cordis plugin was not found",
            )
        })
}

fn owned_plugin_mut<'a>(
    state: &'a mut State,
    session_id: &str,
    plugin_id: &str,
) -> Result<&'a mut Plugin, TessivumError> {
    state
        .plugins
        .iter_mut()
        .find(|plugin| plugin.plugin_id == plugin_id && plugin.session_id.as_str() == session_id)
        .ok_or_else(|| {
            cordis_error(
                "CORDIS_PLUGIN_MISSING",
                "dynamic Cordis plugin was not found",
            )
        })
}

fn cordis_error(code: &str, message: &str) -> TessivumError {
    TessivumError::new(code, message, "cordis", Value::Null)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

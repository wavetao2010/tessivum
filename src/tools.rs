use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tessivum_core::{CancellationToken, ContextHandle, CoreError, ServiceHandle, ServiceKey};

use crate::{ContentBlock, SessionId, TessivumError, ToolCallId, ToolSchema};

/// Stable key for the model-callable tools service.
pub fn tools_service_key() -> ServiceKey {
    ServiceKey::new("harness.tools", "1")
}

/// The input and cancellation identity of one model tool call.
#[derive(Clone, Debug)]
pub struct ToolRunContext {
    pub session: SessionId,
    pub call: ToolCallId,
    pub cancellation: CancellationToken,
}

/// The completed, model-visible output of one tool call.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

impl ToolOutput {
    pub fn new(content: Vec<ContentBlock>, is_error: bool, meta: Value) -> Self {
        Self {
            content,
            is_error,
            meta,
        }
    }

    /// Converts the output to the frozen protocol result block for `call`.
    pub fn into_content_block(self, call: ToolCallId) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_call_id: call,
            content: self.content,
            is_error: Some(self.is_error),
        }
    }

    fn failure(code: impl Into<String>, message: impl Into<String>, details: Value) -> Self {
        let code = code.into();
        let message = message.into();
        Self {
            content: vec![ContentBlock::Text {
                text: message.clone(),
            }],
            is_error: true,
            meta: json!({"code": code, "details": details}),
        }
    }

    fn handler_failure(error: TessivumError) -> Self {
        let TessivumError {
            code,
            message,
            phase,
            details,
        } = error;
        Self {
            content: vec![ContentBlock::Text {
                text: message.clone(),
            }],
            is_error: true,
            meta: json!({"code": code, "phase": phase, "details": details}),
        }
    }
}

pub type ToolHandlerResult = Result<ToolOutput, TessivumError>;

/// The executable part of a tool definition.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult;
}

#[async_trait]
impl<T> ToolHandler for Arc<T>
where
    T: ToolHandler + ?Sized,
{
    async fn run(&self, context: ToolRunContext, arguments: Value) -> ToolHandlerResult {
        (**self).run(context, arguments).await
    }
}

/// A tool's model-visible contract and its private native handler.
#[derive(Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub handler: Arc<dyn ToolHandler>,
}

impl fmt::Debug for ToolDefinition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolDefinition")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("parameters", &self.parameters)
            .finish_non_exhaustive()
    }
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: impl ToolHandler + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            handler: Arc::new(handler),
        }
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

/// The action allowed for a named tool in a runtime scope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolAccess {
    Allow,
    Deny,
    Ask,
}

impl ToolAccess {
    fn rank(self) -> u8 {
        match self {
            Self::Deny => 0,
            Self::Ask => 1,
            Self::Allow => 2,
        }
    }
}

/// A narrowing policy for a derived tool runtime scope.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRestrictions {
    /// When set, only these names remain directly allowed; `ask` and `deny` can narrow them.
    pub allow: Option<BTreeSet<String>>,
    #[serde(default)]
    pub deny: BTreeSet<String>,
    #[serde(default)]
    pub ask: BTreeSet<String>,
}

impl ToolRestrictions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allow_only<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allow: Some(names.into_iter().map(Into::into).collect()),
            ..Self::default()
        }
    }

    pub fn allow(mut self, name: impl Into<String>) -> Self {
        self.allow
            .get_or_insert_with(BTreeSet::new)
            .insert(name.into());
        self
    }

    pub fn deny(mut self, name: impl Into<String>) -> Self {
        self.deny.insert(name.into());
        self
    }

    pub fn ask(mut self, name: impl Into<String>) -> Self {
        self.ask.insert(name.into());
        self
    }

    fn validate(&self) -> Result<(), TessivumError> {
        for name in self
            .allow
            .iter()
            .flatten()
            .chain(self.deny.iter())
            .chain(self.ask.iter())
        {
            if name.trim().is_empty() {
                return Err(tool_error(
                    "INVALID_TOOL_RESTRICTION",
                    "tool restriction names must not be blank",
                    json!({"name": name}),
                ));
            }
        }
        if let Some(name) = self.deny.intersection(&self.ask).next() {
            return Err(tool_error(
                "INVALID_TOOL_RESTRICTION",
                "a tool cannot be both denied and approval-gated",
                json!({"name": name}),
            ));
        }
        Ok(())
    }
}

/// The optional approval response for a call gated by [`ToolAccess::Ask`].
pub type ToolApprovalResult = Result<Option<bool>, TessivumError>;

/// An async, fail-closed approval decision for a tool call.
#[async_trait]
pub trait ToolApproval: Send + Sync {
    async fn approve(
        &self,
        context: &ToolRunContext,
        schema: &ToolSchema,
        arguments: &Value,
    ) -> ToolApprovalResult;
}

#[async_trait]
impl<T> ToolApproval for Arc<T>
where
    T: ToolApproval + ?Sized,
{
    async fn approve(
        &self,
        context: &ToolRunContext,
        schema: &ToolSchema,
        arguments: &Value,
    ) -> ToolApprovalResult {
        (**self).approve(context, schema, arguments).await
    }
}

/// A registration change visible without exposing handlers or results.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ToolChange {
    Registered { schema: ToolSchema },
    Removed { name: String },
}

/// Synchronous observer for settled tool calls.
pub trait ToolResultObserver: Send + Sync {
    fn observe(&self, context: &ToolRunContext, output: &ToolOutput);
}

impl<F> ToolResultObserver for F
where
    F: for<'a, 'b> Fn(&'a ToolRunContext, &'b ToolOutput) + Send + Sync,
{
    fn observe(&self, context: &ToolRunContext, output: &ToolOutput) {
        self(context, output);
    }
}

/// Synchronous observer for tool registration changes.
pub trait ToolChangeObserver: Send + Sync {
    fn observe(&self, change: &ToolChange);
}

impl<F> ToolChangeObserver for F
where
    F: for<'a> Fn(&'a ToolChange) + Send + Sync,
{
    fn observe(&self, change: &ToolChange) {
        self(change);
    }
}

#[derive(Clone)]
struct ToolScope {
    default: ToolAccess,
    named: BTreeMap<String, ToolAccess>,
}

impl ToolScope {
    fn root() -> Self {
        Self {
            default: ToolAccess::Allow,
            named: BTreeMap::new(),
        }
    }

    fn access(&self, name: &str) -> ToolAccess {
        self.named.get(name).copied().unwrap_or(self.default)
    }

    fn narrowed(&self, restrictions: &ToolRestrictions) -> Result<Self, TessivumError> {
        restrictions.validate()?;
        let default = if restrictions.allow.is_some() {
            ToolAccess::Deny
        } else {
            self.default
        };
        if default.rank() > self.default.rank() {
            return Err(tool_error(
                "TOOL_RESTRICTION_BROADENS_SCOPE",
                "tool restrictions may only narrow parent visibility",
                Value::Null,
            ));
        }

        let mut names: BTreeSet<String> = self.named.keys().cloned().collect();
        if let Some(allow) = &restrictions.allow {
            names.extend(allow.iter().cloned());
        }
        names.extend(restrictions.deny.iter().cloned());
        names.extend(restrictions.ask.iter().cloned());

        let mut named = BTreeMap::new();
        for name in names {
            let parent = self.access(&name);
            let requested = if restrictions.deny.contains(&name) {
                ToolAccess::Deny
            } else if restrictions.ask.contains(&name) {
                ToolAccess::Ask
            } else if let Some(allow) = &restrictions.allow {
                if allow.contains(&name) {
                    ToolAccess::Allow
                } else {
                    ToolAccess::Deny
                }
            } else {
                parent
            };
            if requested.rank() > parent.rank() {
                return Err(tool_error(
                    "TOOL_RESTRICTION_BROADENS_SCOPE",
                    "tool restrictions may only narrow parent visibility",
                    json!({"name": name}),
                ));
            }
            if requested != default {
                named.insert(name, requested);
            }
        }
        Ok(Self { default, named })
    }
}

struct RegisteredTool {
    id: u64,
    definition: ToolDefinition,
}

struct RuntimeState {
    next_id: u64,
    tools: BTreeMap<String, RegisteredTool>,
    approval: Option<Arc<dyn ToolApproval>>,
    result_observers: BTreeMap<u64, Arc<dyn ToolResultObserver>>,
    change_observers: BTreeMap<u64, Arc<dyn ToolChangeObserver>>,
}

struct RuntimeInner {
    state: Mutex<RuntimeState>,
}

/// Thread-safe tool registry and execution service.
#[derive(Clone)]
pub struct ToolRuntime {
    inner: Arc<RuntimeInner>,
    scope: Arc<ToolScope>,
}

impl Default for ToolRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ToolRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRuntime")
            .field("schemas", &self.schemas())
            .finish()
    }
}

impl ToolRuntime {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                state: Mutex::new(RuntimeState {
                    next_id: 1,
                    tools: BTreeMap::new(),
                    approval: None,
                    result_observers: BTreeMap::new(),
                    change_observers: BTreeMap::new(),
                }),
            }),
            scope: Arc::new(ToolScope::root()),
        }
    }

    /// Publishes this runtime into `context` under [`tools_service_key`].
    pub fn publish(
        &self,
        context: &ContextHandle,
    ) -> Result<ServiceHandle<ToolRuntime>, CoreError> {
        context.provide(tools_service_key(), self.clone())
    }

    /// Creates a child view whose policy is no broader than this view's policy.
    pub fn scoped(&self, restrictions: ToolRestrictions) -> Result<Self, TessivumError> {
        Ok(Self {
            inner: Arc::clone(&self.inner),
            scope: Arc::new(self.scope.narrowed(&restrictions)?),
        })
    }

    pub fn access(&self, name: &str) -> ToolAccess {
        self.scope.access(name)
    }

    pub fn set_approval(&self, approval: Option<Arc<dyn ToolApproval>>) {
        lock(&self.inner.state).approval = approval;
    }

    pub fn with_approval(self, approval: Arc<dyn ToolApproval>) -> Self {
        self.set_approval(Some(approval));
        self
    }

    /// Registers a model tool until the returned handle is closed or dropped.
    pub fn register(&self, definition: ToolDefinition) -> Result<ToolRegistration, TessivumError> {
        validate_definition(&definition)?;
        let schema = definition.schema();
        let name = definition.name.clone();
        let id = {
            let mut state = lock(&self.inner.state);
            if state.tools.contains_key(&name) {
                return Err(tool_error(
                    "DUPLICATE_TOOL_NAME",
                    "a tool with this name is already registered",
                    json!({"name": name}),
                ));
            }
            let id = state.next_id;
            state.next_id = state.next_id.checked_add(1).unwrap_or(1);
            state
                .tools
                .insert(name.clone(), RegisteredTool { id, definition });
            id
        };
        self.notify_change(ToolChange::Registered { schema });
        Ok(ToolRegistration {
            inner: Arc::downgrade(&self.inner),
            name,
            id,
            closed: AtomicBool::new(false),
        })
    }

    /// Returns only model-visible tool schemas in deterministic name order.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        let state = lock(&self.inner.state);
        state
            .tools
            .values()
            .filter(|tool| self.access(&tool.definition.name) != ToolAccess::Deny)
            .map(|tool| tool.definition.schema())
            .collect()
    }

    pub fn on_result<F>(&self, observer: F) -> ToolObserverRegistration
    where
        F: ToolResultObserver + 'static,
    {
        self.observe_results(Arc::new(observer))
    }

    pub fn observe_results(
        &self,
        observer: Arc<dyn ToolResultObserver>,
    ) -> ToolObserverRegistration {
        let id = {
            let mut state = lock(&self.inner.state);
            let id = state.next_id;
            state.next_id = state.next_id.checked_add(1).unwrap_or(1);
            state.result_observers.insert(id, observer);
            id
        };
        ToolObserverRegistration::result(Arc::downgrade(&self.inner), id)
    }

    pub fn on_change<F>(&self, observer: F) -> ToolObserverRegistration
    where
        F: ToolChangeObserver + 'static,
    {
        self.observe_changes(Arc::new(observer))
    }

    pub fn observe_changes(
        &self,
        observer: Arc<dyn ToolChangeObserver>,
    ) -> ToolObserverRegistration {
        let id = {
            let mut state = lock(&self.inner.state);
            let id = state.next_id;
            state.next_id = state.next_id.checked_add(1).unwrap_or(1);
            state.change_observers.insert(id, observer);
            id
        };
        ToolObserverRegistration::change(Arc::downgrade(&self.inner), id)
    }

    /// Settles one call exactly once, including denied, invalid, and cancelled calls.
    pub async fn execute(
        &self,
        context: ToolRunContext,
        name: impl AsRef<str>,
        arguments: Value,
    ) -> ToolOutput {
        if context.cancellation.is_cancelled() {
            return self.settle(
                context,
                ToolOutput::failure("CANCELLED", "tool call was cancelled", Value::Null),
            );
        }

        let name = name.as_ref();
        let (definition, approval) = {
            let state = lock(&self.inner.state);
            let Some(tool) = state.tools.get(name) else {
                return self.settle(
                    context,
                    ToolOutput::failure(
                        "TOOL_NOT_FOUND",
                        "requested tool is not registered",
                        json!({"name": name}),
                    ),
                );
            };
            (tool.definition.clone(), state.approval.clone())
        };

        if let Err(error) = validate_instance(&definition.parameters, &arguments, "$") {
            return self.settle(context, ToolOutput::handler_failure(error));
        }

        match self.access(name) {
            ToolAccess::Deny => {
                return self.settle(
                    context,
                    ToolOutput::failure(
                        "TOOL_DENIED",
                        "tool is not visible in this scope",
                        json!({"name": name}),
                    ),
                );
            }
            ToolAccess::Ask => {
                let approved = if let Some(approval) = approval {
                    approval
                        .approve(&context, &definition.schema(), &arguments)
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or(false)
                } else {
                    false
                };
                if context.cancellation.is_cancelled() {
                    return self.settle(
                        context,
                        ToolOutput::failure("CANCELLED", "tool call was cancelled", Value::Null),
                    );
                }
                if !approved {
                    return self.settle(
                        context,
                        ToolOutput::failure(
                            "TOOL_APPROVAL_DENIED",
                            "tool call was not approved",
                            json!({"name": name}),
                        ),
                    );
                }
            }
            ToolAccess::Allow => {}
        }

        let output = match definition.handler.run(context.clone(), arguments).await {
            Ok(output) => output,
            Err(error) => ToolOutput::handler_failure(error),
        };
        if context.cancellation.is_cancelled() {
            self.settle(
                context,
                ToolOutput::failure("CANCELLED", "tool call was cancelled", Value::Null),
            )
        } else {
            self.settle(context, output)
        }
    }

    fn settle(&self, context: ToolRunContext, output: ToolOutput) -> ToolOutput {
        self.notify_result(&context, &output);
        output
    }

    fn notify_result(&self, context: &ToolRunContext, output: &ToolOutput) {
        let observers: Vec<_> = lock(&self.inner.state)
            .result_observers
            .values()
            .cloned()
            .collect();
        for observer in observers {
            let _ = catch_unwind(AssertUnwindSafe(|| observer.observe(context, output)));
        }
    }

    fn notify_change(&self, change: ToolChange) {
        let observers: Vec<_> = lock(&self.inner.state)
            .change_observers
            .values()
            .cloned()
            .collect();
        for observer in observers {
            let _ = catch_unwind(AssertUnwindSafe(|| observer.observe(&change)));
        }
    }
}
impl fmt::Debug for ToolRegistration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRegistration")
            .field("name", &self.name)
            .field("id", &self.id)
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish()
    }
}

/// Lifetime-owned registration for one tool name.
pub struct ToolRegistration {
    inner: Weak<RuntimeInner>,
    name: String,
    id: u64,
    closed: AtomicBool,
}

impl ToolRegistration {
    /// Removes this registration once. Later calls are no-ops.
    pub fn close(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        let removed = {
            let mut state = lock(&inner.state);
            if state
                .tools
                .get(&self.name)
                .is_some_and(|tool| tool.id == self.id)
            {
                state.tools.remove(&self.name);
                true
            } else {
                false
            }
        };
        if removed {
            let runtime = ToolRuntime {
                inner,
                scope: Arc::new(ToolScope::root()),
            };
            runtime.notify_change(ToolChange::Removed {
                name: self.name.clone(),
            });
        }
        removed
    }
}

impl Drop for ToolRegistration {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Clone, Copy)]
enum ObserverKind {
    Result,
    Change,
}

/// Lifetime-owned observer registration.
pub struct ToolObserverRegistration {
    inner: Weak<RuntimeInner>,
    id: u64,
    kind: ObserverKind,
    closed: AtomicBool,
}

impl ToolObserverRegistration {
    fn result(inner: Weak<RuntimeInner>, id: u64) -> Self {
        Self {
            inner,
            id,
            kind: ObserverKind::Result,
            closed: AtomicBool::new(false),
        }
    }

    fn change(inner: Weak<RuntimeInner>, id: u64) -> Self {
        Self {
            inner,
            id,
            kind: ObserverKind::Change,
            closed: AtomicBool::new(false),
        }
    }

    pub fn close(&self) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
            return false;
        }
        let Some(inner) = self.inner.upgrade() else {
            return false;
        };
        match self.kind {
            ObserverKind::Result => lock(&inner.state)
                .result_observers
                .remove(&self.id)
                .is_some(),
            ObserverKind::Change => lock(&inner.state)
                .change_observers
                .remove(&self.id)
                .is_some(),
        }
    }
}

impl Drop for ToolObserverRegistration {
    fn drop(&mut self) {
        self.close();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn tool_error(
    code: impl Into<String>,
    message: impl Into<String>,
    details: Value,
) -> TessivumError {
    TessivumError::new(code, message, "tools", details)
}

fn validate_definition(definition: &ToolDefinition) -> Result<(), TessivumError> {
    if definition.name.trim().is_empty() {
        return Err(tool_error(
            "INVALID_TOOL_NAME",
            "tool name must not be blank",
            Value::Null,
        ));
    }
    validate_schema(&definition.parameters, "$")
}

fn validate_schema(schema: &Value, path: &str) -> Result<(), TessivumError> {
    let Value::Object(object) = schema else {
        return Err(schema_error(path, "schema must be an object"));
    };
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "type"
                | "properties"
                | "required"
                | "additionalProperties"
                | "items"
                | "enum"
                | "oneOf"
        ) {
            return Err(schema_error(path, format!("unsupported keyword {key:?}")));
        }
    }

    let type_name = match object.get("type") {
        Some(Value::String(value)) if is_supported_type(value) => Some(value.as_str()),
        Some(Value::String(_)) => return Err(schema_error(path, "type is not supported")),
        Some(_) => return Err(schema_error(path, "type must be a string")),
        None => None,
    };
    let one_of = object.get("oneOf");
    if type_name.is_none() && one_of.is_none() {
        return Err(schema_error(path, "schema requires type or oneOf"));
    }

    if let Some(Value::Array(options)) = one_of {
        if options.is_empty() {
            return Err(schema_error(path, "oneOf must not be empty"));
        }
        for (index, option) in options.iter().enumerate() {
            validate_schema(option, &format!("{path}.oneOf[{index}]"))?;
        }
    } else if one_of.is_some() {
        return Err(schema_error(path, "oneOf must be an array"));
    }

    if let Some(values) = object.get("enum") {
        let Value::Array(values) = values else {
            return Err(schema_error(path, "enum must be an array"));
        };
        if values.is_empty() {
            return Err(schema_error(path, "enum must not be empty"));
        }
        for value in values {
            if let Some(type_name) = type_name {
                if !value_has_type(value, type_name) {
                    return Err(schema_error(path, "enum value does not match type"));
                }
            }
        }
    }

    match type_name {
        Some("object") => validate_object_schema(object, path)?,
        Some("array") => validate_array_schema(object, path)?,
        Some(_) | None => {
            for key in ["properties", "required", "additionalProperties", "items"] {
                if object.contains_key(key) {
                    return Err(schema_error(path, format!("{key} is not valid here")));
                }
            }
        }
    }
    Ok(())
}

fn validate_object_schema(
    object: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), TessivumError> {
    let properties = match object.get("properties") {
        Some(Value::Object(properties)) => properties,
        Some(_) => return Err(schema_error(path, "properties must be an object")),
        None => return Err(schema_error(path, "object schemas require properties")),
    };
    for (name, schema) in properties {
        validate_schema(schema, &format!("{path}.properties.{name}"))?;
    }
    if let Some(required) = object.get("required") {
        let Value::Array(required) = required else {
            return Err(schema_error(path, "required must be an array"));
        };
        let mut names = BTreeSet::new();
        for name in required {
            let Value::String(name) = name else {
                return Err(schema_error(path, "required entries must be strings"));
            };
            if !names.insert(name) {
                return Err(schema_error(path, "required entries must be unique"));
            }
            if !properties.contains_key(name) {
                return Err(schema_error(path, "required property is not declared"));
            }
        }
    }
    if let Some(value) = object.get("additionalProperties") {
        if !value.is_boolean() {
            return Err(schema_error(path, "additionalProperties must be a boolean"));
        }
    }
    Ok(())
}

fn validate_array_schema(
    object: &serde_json::Map<String, Value>,
    path: &str,
) -> Result<(), TessivumError> {
    let Some(items) = object.get("items") else {
        return Err(schema_error(path, "array schemas require items"));
    };
    validate_schema(items, &format!("{path}.items"))
}

fn is_supported_type(value: &str) -> bool {
    matches!(
        value,
        "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
    )
}

fn value_has_type(value: &Value, type_name: &str) -> bool {
    match type_name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn validate_instance(schema: &Value, value: &Value, path: &str) -> Result<(), TessivumError> {
    let object = schema
        .as_object()
        .expect("registered schemas are validated as objects");
    if let Some(options) = object.get("oneOf").and_then(Value::as_array) {
        let matched = options
            .iter()
            .filter(|option| validate_instance(option, value, path).is_ok())
            .count();
        if matched != 1 {
            return Err(argument_error(
                path,
                "value must match exactly one oneOf option",
            ));
        }
    }
    if let Some(type_name) = object.get("type").and_then(Value::as_str) {
        if !value_has_type(value, type_name) {
            return Err(argument_error(path, format!("expected {type_name}")));
        }
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        if !values.iter().any(|candidate| candidate == value) {
            return Err(argument_error(path, "value is not in enum"));
        }
    }

    match object.get("type").and_then(Value::as_str) {
        Some("object") => {
            let value = value
                .as_object()
                .expect("object values passed the type validation");
            let properties = object
                .get("properties")
                .and_then(Value::as_object)
                .expect("registered object schemas have properties");
            for required in object
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = required
                    .as_str()
                    .expect("registered required entries are strings");
                if !value.contains_key(name) {
                    return Err(argument_error(
                        &format!("{path}.{name}"),
                        "property is required",
                    ));
                }
            }
            let additional = object
                .get("additionalProperties")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            for (name, value) in value {
                if let Some(schema) = properties.get(name) {
                    validate_instance(schema, value, &format!("{path}.{name}"))?;
                } else if !additional {
                    return Err(argument_error(
                        &format!("{path}.{name}"),
                        "additional property is not allowed",
                    ));
                }
            }
        }
        Some("array") => {
            let items = object
                .get("items")
                .expect("registered array schemas have items");
            for (index, value) in value
                .as_array()
                .expect("array values passed the type validation")
                .iter()
                .enumerate()
            {
                validate_instance(items, value, &format!("{path}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn schema_error(path: &str, message: impl Into<String>) -> TessivumError {
    tool_error("INVALID_TOOL_SCHEMA", message, json!({"path": path}))
}

fn argument_error(path: &str, message: impl Into<String>) -> TessivumError {
    tool_error("INVALID_TOOL_ARGUMENTS", message, json!({"path": path}))
}

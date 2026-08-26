//! Shared owner for the web and JSON-RPC transports.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tessivum_core::{ContextHandle, CoreError, EntryTree, Loader, PackageResolver, ServiceHandle};
use tessivum_extism::{Capability, CapabilityHandler, CapabilityRegistry, ResourceLimits};
use tessivum_node_bridge::{ClientConfig, HostCommand};
use thiserror::Error;
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::{broadcast, Mutex as AsyncMutex, Notify},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{
    agent::{
        AgentError, AgentFactoryRegistration, AgentHandle, AgentOptions, AgentRegistry,
        AgentStatus, InboxReservationResult, InboxTarget, InboxUpdate,
    },
    agent_loop::AgentLoopFactory,
    agent_preset::{
        AgentPresetDocument, AgentPresetRoot, AgentPresetService, AgentPresetSummary,
        AgentPresetTrust,
    },
    approval::{
        ApprovalAsked, ApprovalDecision, ApprovalError, ApprovalNotification, ApprovalRequested,
        ApprovalResolved, ApprovalService, HostApprovalError, HostApprovalRegistration,
        HostApprovalRegistry,
    },
    attachments::{
        attachments_service_key, decode_inline_image, AttachmentData, AttachmentError,
        AttachmentId, AttachmentInput, AttachmentLimits, AttachmentRef, AttachmentStore,
    },
    bridge::{BridgeServices, DomainBridge, WasmPolicyRegistry},
    builtin_tools::{BashJobOwners, BuiltinTools, BuiltinToolsConfig, HostToolServices},
    code_runtime::{register_code_tool, CodeRuntime, ProcessCodeRuntime},
    compaction::{CompactionConfig, CompactionService},
    compatible_api::CompatibleApiAdapter,
    credentials::{
        credentials_service_key, CredentialEvent, CredentialRef, Credentials, YamlCredentialFile,
    },
    dynamic_cordis::{DynamicCordisRegistry, DynamicCordisTools},
    goal::{GoalError, GoalService, GoalToolRouter, GoalTools},
    jobs::{JobObserverRegistration, JobOwner, JobSnapshot, JobTools, LocalJobRegistry},
    legacy::{product_loader, LegacyProfile, ProductPackageResolver, WasmProductRuntime},
    llm::{
        LlmAdapter, LlmProviderRegistration, LlmRetryPolicy, LlmRetryPolicyConfig, LlmRuntime,
        LlmStream, RecordedLlmAdapter,
    },
    openai_responses::{
        ProviderSnapshot, ResponsesModel, ResponsesReasoningEffort, ResponsesRoute,
        ResponsesRouteResolver, RESPONSES_IMAGE_MODALITY, RESPONSES_TEXT_MODALITY,
    },
    permissions::{
        current as current_permission, fold as permission_knobs, preset as permission_preset,
        preset_names as permission_preset_names,
        settings_registration as permission_settings_registration, CUSTOM_PERMISSION_PRESET,
        PERMISSION_SETTINGS_NAMESPACE,
    },
    persistence_jsonl::JsonlSessionPersistence,
    planning::{PlanMode, PlanningError, PlanningService, PlanningToolRouter, PlanningTools},
    plugin_manager::{plugin_profile_root, PnpmProfileBoundary},
    projection::{ProjectionDefinition, ProjectionRegistry},
    protocol::{
        AgentCancelCause, ContentBlock, InitializeParams, InitializeResult, Message, MessageId,
        MessageRole, MessageSource, SdkRunStatus, SdkServerInfo, SessionEvent,
        SessionEventNotification, SessionHeader, SessionId, SessionModelSelection, SessionOrigin,
        SessionPromptParams, SessionPromptResult, SessionStatus, SessionStatusNotification,
        SubagentFinishedNotification, SubagentStartedNotification, SurfaceOp, MAX_SAFE_INTEGER,
        SESSION_FORMAT_VERSION,
    },
    question::{
        register_ask_user_question_tool, HostQuestionRegistration, HostQuestionRegistry,
        QuestionNotification, QuestionRequested, QuestionResolvedNotice,
    },
    sandbox::Sandbox,
    schedule::{ScheduleOwner, ScheduleOwners, ScheduleTools},
    session::{
        session_service_key, RestoreMode, Session, SessionError, SessionPersistence,
        SessionRawArtifact, SessionStore,
    },
    session_query::{SessionQuery, SESSION_SEARCH_RESULT_LIMIT},
    settings::{
        settings_service_key, Settings, SettingsApplies, SettingsError, SettingsEvent,
        SettingsPathOp, SettingsRegistration, SettingsSnapshot, YamlSettingsProvider,
        AGENT_DEFAULT_MODEL_NAMESPACE, LLM_PI_AI_NAMESPACE,
    },
    skills::{
        skill_session_scopes, AllowSkillInvocation, FilesystemSkillProvider,
        SkillProviderRegistration, SkillRuntime, SkillSessionScopes, SkillTools,
    },
    subagent::{
        NativeSubagentProvider, SubagentCatalog, SubagentDelegationTools, SubagentDeleteRequest,
        SubagentDeleteResult, SubagentDescriptor, SubagentError, SubagentHistoryRequest,
        SubagentHistoryResult, SubagentInterruptRequest, SubagentInterruptResult,
        SubagentPromptRequest, SubagentPromptResult, SubagentProviderRegistration,
        SubagentRunStatus, SubagentService, SubagentTools,
    },
    subprocess::SubprocessRuntime,
    system_prompt::{PromptRegistration, PromptSection, SystemPrompt},
    telemetry::TelemetryCoordinator,
    tools::{ToolRegistration, ToolRestrictions, ToolRuntime},
    web::{
        DeepSeekSearchProvider, HttpFetchConfig, HttpFetchProvider, WebFetchProviderRegistration,
        WebRuntime, WebSearchProviderRegistration, DEEPSEEK_SEARCH_PROVIDER,
    },
    workflow::{register_workflow_tool, NativeWorkflowEngine, WorkflowRuntime, WorkflowTools},
    workspace::{SessionResourceResolver, WorkspaceError, WorkspaceId, WorkspaceRegistry},
    TessivumError,
};
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostModelReasoningEffort {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostModelReasoning {
    pub efforts: Vec<HostModelReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostModelInfo {
    pub provider: String,
    pub id: String,
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_modalities: Vec<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<HostModelReasoning>,
    pub routable: bool,
}

/// A validated upstream model row exposed only by `llm.providerModels`.
///
/// This remains distinct from [`HostModelInfo`], whose compatibility projection
/// has a different, route-oriented wire shape.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostProviderModel {
    pub id: String,
    pub name: String,
    pub context_window: u64,
    pub max_output: u64,
    pub reasoning: bool,
    pub input: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostProviderModels {
    pub provider: String,
    pub models: Vec<HostProviderModel>,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostModelGroup {
    pub provider: String,
    pub display_name: String,
    pub models: Vec<HostModelInfo>,
    pub credential_configured: bool,
    pub routable: bool,
    pub failure: Option<HostRouteFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostRouteFailure {
    pub provider: String,
    pub model: Option<String>,
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionModels {
    pub current: Option<SessionModelSelection>,
    pub routable: bool,
    pub groups: Vec<HostModelGroup>,
    pub failures: Vec<HostRouteFailure>,
}

#[derive(Clone, Debug)]
struct DynamicRouteResolver {
    routes: Arc<Mutex<Arc<BTreeMap<String, ResponsesRoute>>>>,
    credentials: Arc<Credentials>,
}

impl ResponsesRouteResolver for DynamicRouteResolver {
    fn resolve(&self, provider: &str, model: &str) -> Result<ProviderSnapshot, TessivumError> {
        let route = lock(&self.routes).get(provider).cloned().ok_or_else(|| {
            model_error(
                "LLM_PROVIDER_NOT_FOUND",
                "provider route is not registered",
                provider,
                Some(model),
            )
        })?;
        let model_descriptor = route
            .models
            .iter()
            .find(|candidate| candidate.id == model)
            .cloned()
            .ok_or_else(|| {
                model_error(
                    "LLM_MODEL_NOT_FOUND",
                    "model is not declared by provider route",
                    provider,
                    Some(model),
                )
            })?;
        if route.credential_ref.is_empty() {
            return ProviderSnapshot::without_key(route, model_descriptor);
        }
        let credential_ref = CredentialRef::new(route.credential_ref.clone()).map_err(|error| {
            TessivumError::new(
                "INVALID_CREDENTIAL_REF",
                error.to_string(),
                "host",
                Value::Null,
            )
        })?;
        let api_key = resolve_credential_sync(Arc::clone(&self.credentials), credential_ref)?;
        match api_key {
            Some(api_key) => ProviderSnapshot::new(route, model_descriptor, api_key),
            None => ProviderSnapshot::without_key(route, model_descriptor),
        }
    }
}

#[derive(Default)]
struct RouteState {
    routes: Arc<BTreeMap<String, ResponsesRoute>>,
    retry_policies: Arc<BTreeMap<String, LlmRetryPolicy>>,
    registrations: BTreeMap<String, LlmProviderRegistration>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostProviderDirectoryEntry {
    pub route: ResponsesRoute,
    pub credential_configured: bool,
    pub namespace: String,
    pub settings_path: Vec<String>,
    pub active: bool,
    pub declared: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostProviderEnabled {
    pub provider: String,
    pub enabled: bool,
}

const LLM_DEEPSEEK_NAMESPACE: &str = "llm-deepseek";
const DEEPSEEK_PROVIDER: &str = "deepseek-official";

/// The pi-ai provider vocabulary pinned by the bundled Settings client.
///
/// These entries are dormant until their profile is materialized in
/// `llm-pi-ai`; the endpoint/model facts let an empty native-auth profile
/// become a real route without copying a test fixture into the user document.
#[derive(Clone, Copy)]
struct BuiltinPiAiProvider {
    id: &'static str,
    base_url: &'static str,
    model: &'static str,
}

const BUILTIN_PI_AI_PROVIDERS: &[BuiltinPiAiProvider] = &[
    BuiltinPiAiProvider {
        id: "amazon-bedrock",
        base_url: "https://bedrock-runtime.us-east-1.amazonaws.com",
        model: "anthropic.claude-3-5-sonnet-20241022-v2:0",
    },
    BuiltinPiAiProvider {
        id: "ant-ling",
        base_url: "https://api.antgroup.com/v1",
        model: "Ling-1T",
    },
    BuiltinPiAiProvider {
        id: "anthropic",
        base_url: "https://api.anthropic.com/v1",
        model: "claude-sonnet-4-5",
    },
    BuiltinPiAiProvider {
        id: "azure-openai-responses",
        base_url: "https://openai.azure.com/openai/v1",
        model: "gpt-4.1",
    },
    BuiltinPiAiProvider {
        id: "cerebras",
        base_url: "https://api.cerebras.ai/v1",
        model: "gpt-oss-120b",
    },
    BuiltinPiAiProvider {
        id: "cloudflare-ai-gateway",
        base_url: "https://gateway.ai.cloudflare.com/v1",
        model: "@cf/meta/llama-3.1-8b-instruct",
    },
    BuiltinPiAiProvider {
        id: "cloudflare-workers-ai",
        base_url: "https://api.cloudflare.com/client/v4",
        model: "@cf/meta/llama-3.1-8b-instruct",
    },
    BuiltinPiAiProvider {
        id: "deepseek",
        base_url: "https://api.deepseek.com/v1",
        model: "deepseek-chat",
    },
    BuiltinPiAiProvider {
        id: "fireworks",
        base_url: "https://api.fireworks.ai/inference/v1",
        model: "accounts/fireworks/models/llama-v3p1-8b-instruct",
    },
    BuiltinPiAiProvider {
        id: "github-copilot",
        base_url: "https://api.githubcopilot.com",
        model: "gpt-4.1",
    },
    BuiltinPiAiProvider {
        id: "google",
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        model: "gemini-2.5-pro",
    },
    BuiltinPiAiProvider {
        id: "google-vertex",
        base_url: "https://aiplatform.googleapis.com/v1",
        model: "gemini-2.5-pro",
    },
    BuiltinPiAiProvider {
        id: "groq",
        base_url: "https://api.groq.com/openai/v1",
        model: "llama-3.3-70b-versatile",
    },
    BuiltinPiAiProvider {
        id: "huggingface",
        base_url: "https://router.huggingface.co/v1",
        model: "meta-llama/Llama-3.3-70B-Instruct",
    },
    BuiltinPiAiProvider {
        id: "kimi-coding",
        base_url: "https://api.kimi.com/coding/v1",
        model: "kimi-for-coding",
    },
    BuiltinPiAiProvider {
        id: "minimax",
        base_url: "https://api.minimax.io/v1",
        model: "MiniMax-M2.7",
    },
    BuiltinPiAiProvider {
        id: "minimax-cn",
        base_url: "https://api.minimaxi.com/v1",
        model: "MiniMax-M2.7",
    },
    BuiltinPiAiProvider {
        id: "mistral",
        base_url: "https://api.mistral.ai/v1",
        model: "mistral-large-latest",
    },
    BuiltinPiAiProvider {
        id: "moonshotai",
        base_url: "https://api.moonshot.ai/v1",
        model: "kimi-k2.5",
    },
    BuiltinPiAiProvider {
        id: "moonshotai-cn",
        base_url: "https://api.moonshot.cn/v1",
        model: "kimi-k2.5",
    },
    BuiltinPiAiProvider {
        id: "nvidia",
        base_url: "https://integrate.api.nvidia.com/v1",
        model: "meta/llama-3.3-70b-instruct",
    },
    BuiltinPiAiProvider {
        id: "openai",
        base_url: "https://api.openai.com/v1",
        model: "gpt-4.1",
    },
    BuiltinPiAiProvider {
        id: "opencode",
        base_url: "https://opencode.ai/zen/v1",
        model: "gpt-5",
    },
    BuiltinPiAiProvider {
        id: "opencode-go",
        base_url: "https://opencode.ai/zen/v1",
        model: "gpt-5",
    },
    BuiltinPiAiProvider {
        id: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        model: "openai/gpt-4.1",
    },
    BuiltinPiAiProvider {
        id: "qwen-token-plan",
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        model: "qwen3-max",
    },
    BuiltinPiAiProvider {
        id: "qwen-token-plan-cn",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        model: "qwen3-max",
    },
    BuiltinPiAiProvider {
        id: "together",
        base_url: "https://api.together.xyz/v1",
        model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
    },
    BuiltinPiAiProvider {
        id: "vercel-ai-gateway",
        base_url: "https://ai-gateway.vercel.sh/v1",
        model: "openai/gpt-4.1",
    },
    BuiltinPiAiProvider {
        id: "xai",
        base_url: "https://api.x.ai/v1",
        model: "grok-4",
    },
    BuiltinPiAiProvider {
        id: "xiaomi",
        base_url: "https://api.xiaomimimo.com/v1",
        model: "mimo-v2-flash",
    },
    BuiltinPiAiProvider {
        id: "xiaomi-token-plan-ams",
        base_url: "https://api.xiaomimimo.com/v1",
        model: "mimo-v2-flash",
    },
    BuiltinPiAiProvider {
        id: "xiaomi-token-plan-cn",
        base_url: "https://api.xiaomimimo.com/v1",
        model: "mimo-v2-flash",
    },
    BuiltinPiAiProvider {
        id: "xiaomi-token-plan-sgp",
        base_url: "https://api.xiaomimimo.com/v1",
        model: "mimo-v2-flash",
    },
    BuiltinPiAiProvider {
        id: "zai",
        base_url: "https://api.z.ai/api/paas/v4",
        model: "glm-4.7",
    },
    BuiltinPiAiProvider {
        id: "zai-coding-cn",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        model: "glm-4.7",
    },
];

fn builtin_pi_ai_provider(id: &str) -> Option<BuiltinPiAiProvider> {
    BUILTIN_PI_AI_PROVIDERS
        .iter()
        .copied()
        .find(|provider| provider.id == id)
}

fn builtin_pi_ai_route(provider: BuiltinPiAiProvider) -> ResponsesRoute {
    ResponsesRoute::new(
        provider.id,
        provider.id,
        provider.base_url,
        "",
        vec![ResponsesModel::new(provider.model)],
    )
}

fn default_deepseek_models() -> Vec<ResponsesModel> {
    [
        ("deepseek-v4-flash", "DeepSeek-V4-Flash"),
        ("deepseek-v4-pro", "DeepSeek-V4-Pro"),
    ]
    .into_iter()
    .map(|(id, name)| ResponsesModel {
        id: id.into(),
        name: Some(name.into()),
        description: None,
        input: vec![RESPONSES_TEXT_MODALITY.into()],
        context_window: Some(1_000_000),
        max_tokens: None,
        reasoning_efforts: Vec::new(),
        default_reasoning_effort: None,
    })
    .collect()
}

fn default_deepseek_route() -> ResponsesRoute {
    ResponsesRoute::new(
        DEEPSEEK_PROVIDER,
        "DeepSeek",
        "https://api.deepseek.com",
        "DEEPSEEK_API_KEY",
        default_deepseek_models(),
    )
}

fn credential_configured(credentials: &Arc<Credentials>, reference: &str) -> bool {
    !reference.is_empty()
        && CredentialRef::new(reference.to_owned())
            .ok()
            .and_then(|reference| resolve_credential_sync(Arc::clone(credentials), reference).ok())
            .flatten()
            .is_some()
}

const MAX_FRAME_BYTES: usize = 1_048_576;
const MAX_PROMPT_BLOCKS: usize = 128;
const MAX_PROFILE_BYTES: usize = 128;
const MAX_NOTIFICATIONS: usize = 4_096;
const MAX_LIVE_SESSIONS: usize = 1_024;
const MAX_ORPHAN_SWEEP_SESSIONS: usize = 1_024;
const CODE_MODE_PROMPT: &str = concat!(
    "## Writing code for run_code\n",
    "Use the `run_code` tool for model-directed actions. Its JavaScript program receives the native tool ",
    "SDK as `declare const tools`; call `await tools.<name>(arguments)` for each operation and return a ",
    "JSON-serializable result.",
);

const MAX_ORPHAN_SWEEP_ENTRIES: usize = 1_024;

/// Injectable boundary for opening a host-selected filesystem target.
#[async_trait]
pub trait HostPathOpener: Send + Sync {
    /// Whether this deployment can reach a user-visible desktop application.
    fn can_open_path(&self) -> bool;

    /// Hand a canonical target to the platform's default application.
    async fn open_path(&self, path: PathBuf) -> Result<(), TessivumError>;

    /// Hand a prepared text document to a native editor.
    async fn open_text_file(&self, path: PathBuf) -> Result<(), TessivumError> {
        self.open_path(path).await
    }
}
#[async_trait]
impl<F> HostPathOpener for F
where
    F: Fn(&Path) -> Result<(), TessivumError> + Send + Sync,
{
    fn can_open_path(&self) -> bool {
        true
    }

    async fn open_path(&self, path: PathBuf) -> Result<(), TessivumError> {
        self(path.as_path())
    }
}

/// Injectable native single-directory chooser.
#[async_trait]
pub trait HostDirectoryPicker: Send + Sync {
    /// Returns `None` only when the operator cancels the chooser.
    async fn pick_directory(&self) -> Result<Option<PathBuf>, TessivumError>;
}

/// The production shell-free opener. Tests inject [`HostPathOpener`] instead.
#[derive(Default)]
pub struct SystemPathOpener;

impl SystemPathOpener {
    async fn run(&self, program: &str, args: Vec<OsString>) -> Result<(), TessivumError> {
        let output = tokio::process::Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|error| {
                TessivumError::new(
                    "PATH_OPEN_FAILED",
                    format!("could not start native path opener: {error}"),
                    "host",
                    Value::Null,
                )
            })?;
        if output.status.success() {
            return Ok(());
        }
        Err(TessivumError::new(
            "PATH_OPEN_FAILED",
            format!("native path opener exited with {}", output.status),
            "host",
            Value::Null,
        ))
    }

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    async fn open_windows_path(&self, path: PathBuf) -> Result<(), TessivumError> {
        self.run(
            "powershell.exe",
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-Command"),
                OsString::from("Invoke-Item -LiteralPath $args[0]"),
                path.into_os_string(),
            ],
        )
        .await
    }

    #[cfg(target_os = "linux")]
    fn is_wsl() -> bool {
        std::env::var_os("WSL_DISTRO_NAME").is_some_and(|value| !value.is_empty())
            || std::env::var_os("WSL_INTEROP").is_some_and(|value| !value.is_empty())
            || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
    }
}

#[async_trait]
impl HostPathOpener for SystemPathOpener {
    fn can_open_path(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            return true;
        }
        #[cfg(target_os = "linux")]
        {
            return Self::is_wsl()
                || std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty())
                || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
        }
        #[allow(unreachable_code)]
        false
    }

    async fn open_path(&self, path: PathBuf) -> Result<(), TessivumError> {
        #[cfg(target_os = "macos")]
        return self.run("open", vec![path.into_os_string()]).await;

        #[cfg(target_os = "windows")]
        return self.open_windows_path(path).await;

        #[cfg(target_os = "linux")]
        {
            if Self::is_wsl() {
                let output = tokio::process::Command::new("wslpath")
                    .args(["-w", path.to_string_lossy().as_ref()])
                    .kill_on_drop(true)
                    .output()
                    .await
                    .map_err(|error| {
                        TessivumError::new(
                            "PATH_OPEN_FAILED",
                            format!("could not translate WSL path: {error}"),
                            "host",
                            Value::Null,
                        )
                    })?;
                if !output.status.success() {
                    return Err(TessivumError::new(
                        "PATH_OPEN_FAILED",
                        format!("WSL path translation exited with {}", output.status),
                        "host",
                        Value::Null,
                    ));
                }
                let translated = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if translated.is_empty() {
                    return Err(TessivumError::new(
                        "PATH_OPEN_FAILED",
                        "WSL path translation returned no Windows path",
                        "host",
                        Value::Null,
                    ));
                }
                return self.open_windows_path(PathBuf::from(translated)).await;
            }
            return self.run("xdg-open", vec![path.into_os_string()]).await;
        }

        #[allow(unreachable_code)]
        Err(TessivumError::new(
            "PATH_OPENER_UNAVAILABLE",
            "native path opener is unsupported on this platform",
            "host",
            Value::Null,
        ))
    }

    async fn open_text_file(&self, path: PathBuf) -> Result<(), TessivumError> {
        #[cfg(target_os = "macos")]
        return self
            .run("open", vec![OsString::from("-t"), path.into_os_string()])
            .await;

        #[cfg(not(target_os = "macos"))]
        self.open_path(path).await
    }
}
#[derive(Default)]
pub struct SystemDirectoryPicker;

#[async_trait]
impl HostDirectoryPicker for SystemDirectoryPicker {
    async fn pick_directory(&self) -> Result<Option<PathBuf>, TessivumError> {
        #[cfg(target_os = "macos")]
        let output = tokio::process::Command::new("osascript")
            .args([
                "-e",
                "set selectedFolder to choose folder with prompt \"Select Workspace Directory\"",
                "-e",
                "POSIX path of selectedFolder",
            ])
            .kill_on_drop(true)
            .output()
            .await;
        #[cfg(target_os = "linux")]
        let output = {
            let first = tokio::process::Command::new("zenity")
                .args([
                    "--file-selection",
                    "--directory",
                    "--title=Select Workspace Directory",
                ])
                .kill_on_drop(true)
                .output()
                .await;
            match first {
                Ok(result) => Ok(result),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::process::Command::new("kdialog")
                        .args([
                            "--getexistingdirectory",
                            ".",
                            "--title",
                            "Select Workspace Directory",
                        ])
                        .kill_on_drop(true)
                        .output()
                        .await
                }
                Err(error) => Err(error),
            }
        };
        #[cfg(target_os = "windows")]
        let output = Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "native directory picker is unsupported on this build",
        ));
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        let output = Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "native directory picker is unsupported on this platform",
        ));

        let output = output.map_err(|error| {
            TessivumError::new(
                "DIRECTORY_PICKER_UNAVAILABLE",
                error.to_string(),
                "host",
                json!({"capability": "native"}),
            )
        })?;
        if !output.status.success() {
            #[cfg(target_os = "macos")]
            if output.status.code() == Some(1) && {
                let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
                stderr.contains("user canceled") || stderr.contains("-128")
            } {
                return Ok(None);
            }
            #[cfg(not(target_os = "macos"))]
            if output.status.code() == Some(1) {
                return Ok(None);
            }
            return Err(TessivumError::new(
                "DIRECTORY_PICKER_FAILED",
                format!("native picker exited with {}", output.status),
                "host",
                Value::Null,
            ));
        }
        let selected = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if selected.is_empty() {
            return Ok(None);
        }
        Ok(Some(PathBuf::from(selected)))
    }
}

/// A factory is called once at boot and must never retry durable work.
pub trait HostLlmAdapterFactory: Send + Sync {
    fn create(&self, provider: &str, model: &str) -> Result<Arc<dyn LlmAdapter>, TessivumError>;
}

/// Canonical path and profile identity attached to every host-owned session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostIdentity {
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
    pub profile: String,
}

#[derive(Clone, Debug)]
pub struct LegacyHostConfig {
    pub command: HostCommand,
    pub client: ClientConfig,
}

/// Boot inputs. JSON patches are applied bundle → profile → home → CLI → telemetry.
#[derive(Clone)]
pub struct HostConfig {
    pub cwd: PathBuf,
    pub data_dir: PathBuf,
    /// Host-selected writable settings file. `None` uses `data_dir/settings.yaml`.
    pub settings_path: Option<PathBuf>,
    /// Host-selected writable credentials file. `None` uses `data_dir/credentials.yaml`.
    pub credentials_path: Option<PathBuf>,
    pub agent_preset_roots: Vec<AgentPresetRoot>,
    pub include_user_preset_root: bool,
    pub profile: String,
    pub provider: String,
    pub model: String,
    pub max_tokens: Option<u64>,
    pub recorded_replay: Option<String>,
    pub recorded_replay_override: Option<String>,
    pub recorded_replay_pace_ms: u64,
    pub recorded_replay_context_window: Option<u64>,
    pub adapter_factory: Option<Arc<dyn HostLlmAdapterFactory>>,
    pub path_opener: Option<Arc<dyn HostPathOpener>>,
    pub directory_picker: Option<Arc<dyn HostDirectoryPicker>>,
    pub bundle_patch: Value,
    pub profile_patch: Value,
    pub home_patch: Value,
    pub cli_patches: Vec<Value>,
    pub telemetry_patch: Value,
    pub system_prompt: Option<String>,
    pub enable_trusted_bash: bool,
    pub approval_required_tools: BTreeSet<String>,
    pub notification_capacity: usize,
    pub max_live_sessions: usize,
    pub entries: Option<EntryTree>,
    pub legacy_profile: Option<LegacyProfile>,
    pub legacy_host: Option<LegacyHostConfig>,
    pub package_resolver: Option<Arc<dyn PackageResolver>>,
    pub wasm_limits: ResourceLimits,
    pub telemetry: Option<TelemetryCoordinator>,
    pub code_runtime: Option<ProcessCodeRuntime>,
    pub dynamic_cordis: bool,
}

impl std::fmt::Debug for HostConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostConfig")
            .field("cwd", &self.cwd)
            .field("data_dir", &self.data_dir)
            .field("settings_path", &self.settings_path)
            .field("credentials_path", &self.credentials_path)
            .field("agent_preset_roots", &self.agent_preset_roots)
            .field("include_user_preset_root", &self.include_user_preset_root)
            .field("has_path_opener", &self.path_opener.is_some())
            .field("profile", &self.profile)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("has_recording", &self.recorded_replay.is_some())
            .field(
                "has_recording_override",
                &self.recorded_replay_override.is_some(),
            )
            .field("recording_pace_ms", &self.recorded_replay_pace_ms)
            .field(
                "recording_context_window",
                &self.recorded_replay_context_window,
            )
            .field("has_adapter_factory", &self.adapter_factory.is_some())
            .field("has_directory_picker", &self.directory_picker.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for HostConfig {
    fn default() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(cwd.clone(), cwd.join(".tessivum"))
    }
}

impl HostConfig {
    pub fn new(cwd: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            data_dir: data_dir.into(),
            settings_path: None,
            credentials_path: None,
            agent_preset_roots: Vec::new(),
            include_user_preset_root: true,
            profile: "default".into(),
            provider: "recorded".into(),
            model: "recorded".into(),
            max_tokens: None,
            recorded_replay: None,
            recorded_replay_override: None,
            recorded_replay_pace_ms: 0,
            recorded_replay_context_window: None,
            adapter_factory: None,
            path_opener: None,
            directory_picker: None,
            bundle_patch: json!({}),
            profile_patch: json!({}),
            home_patch: json!({}),
            cli_patches: Vec::new(),
            telemetry_patch: json!({}),
            system_prompt: None,
            enable_trusted_bash: false,
            approval_required_tools: BTreeSet::new(),
            notification_capacity: 128,
            max_live_sessions: 128,
            entries: None,
            legacy_profile: None,
            package_resolver: None,
            legacy_host: None,
            wasm_limits: ResourceLimits::default(),
            telemetry: None,
            code_runtime: None,
            dynamic_cordis: false,
        }
    }
    pub fn with_settings_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.settings_path = Some(path.into());
        self
    }
    pub fn with_credentials_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.credentials_path = Some(path.into());
        self
    }
    pub fn with_path_opener(mut self, opener: Arc<dyn HostPathOpener>) -> Self {
        self.path_opener = Some(opener);
        self
    }
    pub fn with_directory_picker(mut self, picker: Arc<dyn HostDirectoryPicker>) -> Self {
        self.directory_picker = Some(picker);
        self
    }
    pub fn with_recorded_replay(mut self, replay: impl Into<String>) -> Self {
        self.recorded_replay = Some(replay.into());
        self
    }
    pub fn with_recorded_replay_override(mut self, override_document: impl Into<String>) -> Self {
        self.recorded_replay_override = Some(override_document.into());
        self
    }
    pub fn with_recorded_replay_pace_ms(mut self, pace_ms: u64) -> Self {
        self.recorded_replay_pace_ms = pace_ms;
        self
    }
    pub fn with_adapter_factory(mut self, factory: Arc<dyn HostLlmAdapterFactory>) -> Self {
        self.adapter_factory = Some(factory);
        self
    }
    pub fn with_agent_preset_root(
        mut self,
        path: impl Into<PathBuf>,
        trust: AgentPresetTrust,
    ) -> Self {
        self.agent_preset_roots.push(AgentPresetRoot {
            path: path.into(),
            trust,
        });
        self
    }
    pub fn with_include_user_preset_root(mut self, include: bool) -> Self {
        self.include_user_preset_root = include;
        self
    }

    pub fn with_approval_required_tool(mut self, tool: impl Into<String>) -> Self {
        self.approval_required_tools.insert(tool.into());
        self
    }
    pub fn with_cli_patch(mut self, patch: Value) -> Self {
        self.cli_patches.push(patch);
        self
    }
    pub fn with_dynamic_cordis(mut self, enabled: bool) -> Self {
        self.dynamic_cordis = enabled;
        self
    }
    pub fn compose_profile(&self) -> Result<Value, HostError> {
        validate_config(self)?;
        let mut result = Map::new();
        for patch in std::iter::once(&self.bundle_patch)
            .chain(std::iter::once(&self.profile_patch))
            .chain(std::iter::once(&self.home_patch))
            .chain(self.cli_patches.iter())
            .chain(std::iter::once(&self.telemetry_patch))
        {
            merge_object(
                &mut result,
                patch.as_object().expect("validated patch object"),
            );
        }
        Ok(Value::Object(result))
    }
}

/// Internal composition failures. The public [`HostApi`] boundary uses the
/// existing wire-stable [`TessivumError`] envelope.
#[derive(Debug, Error)]
pub enum HostError {
    #[error("invalid host configuration: {0}")]
    InvalidConfiguration(String),
    #[error("cannot canonicalize {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot create host data directory {path}: {source}")]
    CreateDataDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("host is shutting down")]
    ShuttingDown,
    #[error("host initialization conflicts with the boot route")]
    InitializationConflict,
    #[error("host live-session capacity is exhausted")]
    SessionCapacity,
    #[error("host shutdown failed: {0}")]
    Shutdown(String),
    #[error("message feedback storage failed: {0}")]
    MessageFeedback(String),
    #[error(transparent)]
    Runtime(#[from] TessivumError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error(transparent)]
    Approval(#[from] ApprovalError),
    #[error(transparent)]
    ApprovalRegistry(#[from] HostApprovalError),
    #[error(transparent)]
    Attachment(#[from] AttachmentError),
    #[error(transparent)]
    Goal(#[from] GoalError),
    #[error(transparent)]
    Planning(#[from] PlanningError),

    #[error("session {session_id} is durable but ungrouped")]
    SessionUngrouped { session_id: SessionId },
    #[error("failed to attach session {session_id} to workspace {workspace_id}: {source}")]
    WorkspaceAttach {
        session_id: SessionId,
        workspace_id: WorkspaceId,
        #[source]
        source: WorkspaceError,
    },
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
}

impl HostError {
    fn invalid(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Runtime(TessivumError::new(code, message, "host", Value::Null))
    }

    pub fn code(&self) -> &str {
        match self {
            Self::InvalidConfiguration(_) => "INVALID_HOST_CONFIG",
            Self::Canonicalize { .. } => "INVALID_HOST_PATH",
            Self::CreateDataDir { .. } => "HOST_DATA_DIR_CREATE_FAILED",
            Self::ShuttingDown => "HOST_SHUTTING_DOWN",
            Self::InitializationConflict => "HOST_INITIALIZATION_CONFLICT",
            Self::Shutdown(_) => "HOST_SHUTDOWN_FAILED",
            Self::MessageFeedback(_) => "MESSAGE_FEEDBACK_STORAGE_FAILED",
            Self::SessionCapacity => "HOST_SESSION_CAPACITY",
            Self::SessionUngrouped { .. } => "SESSION_UNGROUPED",
            Self::WorkspaceAttach { .. } => "WORKSPACE_ATTACH_FAILED",
            Self::Workspace(error) => error.code(),
            Self::Runtime(error) => &error.code,
            Self::Settings(error) => error.code(),
            Self::Session(error) => error.code(),
            Self::Agent(AgentError::Cancelled) => "CANCELLED",
            Self::Agent(_) => "HOST_AGENT_ERROR",
            Self::Core(_) => "HOST_CORE_ERROR",
            Self::Approval(_) => "HOST_APPROVAL_ERROR",
            Self::ApprovalRegistry(_) => "HOST_APPROVAL_REGISTRY_ERROR",
            Self::Goal(error) => error.code(),
            Self::Planning(error) => error.code(),
            Self::Attachment(error) => error.code(),
        }
    }

    fn wire(self) -> TessivumError {
        match self {
            Self::Runtime(error) => error,
            Self::Settings(error) => error.as_tessivum_error(),
            Self::SessionUngrouped { session_id } => TessivumError::new(
                "SESSION_UNGROUPED",
                format!("session {session_id} is durable but ungrouped"),
                "host",
                json!({"sessionId": session_id}),
            ),
            Self::WorkspaceAttach {
                session_id,
                workspace_id,
                source: _,
            } => TessivumError::new(
                "WORKSPACE_ATTACH_FAILED",
                format!("failed to attach session {session_id} to workspace {workspace_id}"),
                "host",
                json!({"sessionId": session_id, "workspaceId": workspace_id}),
            ),
            Self::Workspace(error) => TessivumError::new(
                error.code(),
                "workspace operation failed",
                "host",
                Value::Null,
            ),
            Self::Approval(error) => TessivumError::new(
                "HOST_APPROVAL_ERROR",
                error.to_string(),
                "host",
                Value::Null,
            ),
            Self::ApprovalRegistry(error) => TessivumError::new(
                "HOST_APPROVAL_REGISTRY_ERROR",
                error.to_string(),
                "host",
                Value::Null,
            ),
            error => {
                let code = error.code().to_owned();
                TessivumError::new(code, error.to_string(), "host", Value::Null)
            }
        }
    }
}

/// A settings write routed through the host so live provider routes are updated
/// before the transport reports success.
pub enum HostSettingsMutation {
    Update {
        patch: Value,
        expected_revision: Option<u64>,
    },
    Replace {
        user: Value,
        expected_revision: Option<u64>,
    },
    Mutate {
        ops: Vec<SettingsPathOp>,
        expected_revision: Option<u64>,
    },
}
#[derive(Clone, Debug)]
pub enum HostNotification {
    SessionEvent(SessionEventNotification),
    SessionProjection(HostSessionProjectionNotification),
    SessionStatus(SessionStatusNotification),
    /// Full, current pending inbox state after any queue mutation or claim.
    SessionQueue(HostSessionQueueNotification),
    SessionJobs(HostSessionJobsNotification),
    /// Generic legacy Host event relayed to browser Cordis clients.
    RemoteEvent(HostRemoteEvent),
    SubagentStarted(SubagentStartedNotification),
    SubagentFinished(SubagentFinishedNotification),
    ApprovalRequested(ApprovalRequested),
    ApprovalResolved(ApprovalResolved),
    QuestionRequested(QuestionRequested),
    QuestionResolved(QuestionResolvedNotice),
    SettingsChanged(SettingsEvent),
    CredentialsChanged(CredentialEvent),
    ModelsChanged,
    AdaptersUpdated,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostRemoteEvent {
    pub event: String,
    pub args: Vec<Value>,
}

/// One higher-sequence frame from a registered session projection.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionProjectionNotification {
    pub session_id: SessionId,
    pub key: String,
    pub value: Value,
    pub seq: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionJobsNotification {
    pub session_id: SessionId,
    pub jobs: Vec<JobSnapshot>,
}

/// Durable session metadata needed by reconnecting transports without replaying logs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSessionInfo {
    pub session_id: SessionId,
    pub workspace_id: Option<WorkspaceId>,
    pub created_at: u64,
    pub updated_at: u64,
    pub running: bool,
    pub cwd: Option<String>,
    pub parent_session: Option<SessionId>,
    pub origin: Option<SessionOrigin>,
    pub agent_preset: Option<String>,
    pub event_count: u64,
    pub blank: bool,
}

/// One browser-visible semantic session search hit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSessionSearchHit {
    pub session_id: SessionId,
    pub snippet: String,
}

/// Bounded browser-visible semantic session search page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSessionSearchResult {
    pub items: Vec<HostSessionSearchHit>,
    pub has_more: bool,
}

/// Durable explicit title update result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSessionRenameResult {
    pub title: String,
    pub seq: u64,
}

/// Immutable identity and model route exposed to transports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostDescriptor {
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub max_tokens: Option<u64>,
}
/// Exact result of opening a user-authored preset directory.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostAgentPresetDocument {
    pub opened: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// One slash-command entry advertised to browser clients.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct HostCommandDescriptor {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<HostCommandInputDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct HostCommandInputDescriptor {
    pub hint: String,
}

/// Settled slash-command result on the generated Remote wire.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum HostCommandResult {
    Success {
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(rename = "sourceEventSeq", skip_serializing_if = "Option::is_none")]
        source_event_seq: Option<u64>,
    },
    Error {
        text: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostCommandExecution {
    pub command_id: String,
    pub result: HostCommandResult,
}

/// One session queue operation accepted by the browser API.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionQueueAction {
    Edit { content: Vec<ContentBlock> },
    Remove,
    Steer,
}

/// Id-addressed request for a still-pending user inbox item.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionUpdateQueueParams {
    pub session_id: SessionId,
    pub item_id: MessageId,
    pub action: SessionQueueAction,
}

/// Stable acknowledgement for an accepted queue operation.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateQueueResult {
    pub accepted: bool,
}

/// One resolved pending inbox occurrence in a complete queue snapshot.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionQueueItem {
    pub id: MessageId,
    pub placement: String,
    pub message: Message,
}

/// Authoritative live queue state for one session.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSessionQueueNotification {
    pub session_id: SessionId,
    pub items: Vec<HostSessionQueueItem>,
}

/// One browser-visible value from a registered session projection.
#[derive(Clone, Debug)]
pub struct HostSessionProjection {
    pub key: String,
    pub value: Value,
    pub seq: Option<u64>,
}

/// One object-safe host contract for API and SDK transports.
#[async_trait]
pub trait HostApi: Send + Sync {
    async fn initialize(&self, params: InitializeParams)
        -> Result<InitializeResult, TessivumError>;
    async fn prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError>;
    async fn steer(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.prompt(params).await
    }
    async fn cancel(
        &self,
        session: SessionId,
        cause: AgentCancelCause,
    ) -> Result<bool, TessivumError>;
    async fn events(
        &self,
        session: SessionId,
        from_seq: u64,
    ) -> Result<Vec<SessionEvent>, TessivumError>;
    async fn session_projections(
        &self,
        _session: SessionId,
    ) -> Result<Vec<HostSessionProjection>, TessivumError> {
        Ok(Vec::new())
    }

    async fn status(&self, session: SessionId) -> Result<Option<SessionStatus>, TessivumError>;

    /// Flushes a live session and reads its backend-owned artifact verbatim.
    async fn read_raw_session(
        &self,
        _session: SessionId,
    ) -> Result<Option<SessionRawArtifact>, TessivumError> {
        Err(TessivumError::new(
            "SESSION_EXPORT_UNAVAILABLE",
            "session log export is unavailable: missing session persistence or attachments service",
            "host",
            Value::Null,
        ))
    }

    async fn update_queue(
        &self,
        _params: SessionUpdateQueueParams,
    ) -> Result<SessionUpdateQueueResult, TessivumError> {
        Err(TessivumError::new(
            "QUEUE_UNAVAILABLE",
            "this host does not support queue updates",
            "host",
            Value::Null,
        ))
    }
    async fn subagent_list(
        &self,
        _parent_session_id: SessionId,
    ) -> Result<SubagentCatalog, TessivumError> {
        Err(TessivumError::new(
            "SUBAGENT_RPC_UNSUPPORTED",
            "this host does not support subagent catalogs",
            "host",
            Value::Null,
        ))
    }
    async fn subagent_history(
        &self,
        _params: SubagentHistoryRequest,
    ) -> Result<SubagentHistoryResult, TessivumError> {
        Err(TessivumError::new(
            "SUBAGENT_RPC_UNSUPPORTED",
            "this host does not support subagent history",
            "host",
            Value::Null,
        ))
    }
    async fn subagent_prompt(
        &self,
        _params: SubagentPromptRequest,
    ) -> Result<SubagentPromptResult, TessivumError> {
        Err(TessivumError::new(
            "SUBAGENT_RPC_UNSUPPORTED",
            "this host does not support subagent prompts",
            "host",
            Value::Null,
        ))
    }
    async fn subagent_interrupt(
        &self,
        _params: SubagentInterruptRequest,
    ) -> Result<SubagentInterruptResult, TessivumError> {
        Err(TessivumError::new(
            "SUBAGENT_RPC_UNSUPPORTED",
            "this host does not support subagent interrupts",
            "host",
            Value::Null,
        ))
    }
    async fn subagent_delete(
        &self,
        _params: SubagentDeleteRequest,
    ) -> Result<SubagentDeleteResult, TessivumError> {
        Err(TessivumError::new(
            "SUBAGENT_RPC_UNSUPPORTED",
            "this host does not support subagent deletion",
            "host",
            Value::Null,
        ))
    }
    async fn plugin_inventory(&self) -> Result<Vec<Value>, TessivumError> {
        Ok(Vec::new())
    }

    async fn goal_service(&self, _session: SessionId) -> Result<GoalService, TessivumError> {
        Err(TessivumError::new(
            "GOAL_SERVICE_ABSENT",
            "goal service is absent: neither this session's agent preset nor the host composition mounts @deepseek-ai/dsh-goal",
            "goals",
            Value::Null,
        ))
    }
    async fn command_list(
        &self,
        _session: SessionId,
    ) -> Result<Vec<HostCommandDescriptor>, TessivumError> {
        Ok(Vec::new())
    }
    async fn command_execute(
        &self,
        _session: SessionId,
        _line: String,
    ) -> Result<Option<HostCommandExecution>, TessivumError> {
        Ok(None)
    }
    async fn message_feedback_list(&self, _session: SessionId) -> Result<Value, TessivumError> {
        Err(TessivumError::new(
            "MESSAGE_FEEDBACK_UNAVAILABLE",
            "this host does not support message feedback",
            "host",
            Value::Null,
        ))
    }
    async fn message_feedback_put(
        &self,
        _session: SessionId,
        _message_id: String,
        _rating: String,
        _note: Option<String>,
        _if_version: Option<String>,
    ) -> Result<Value, TessivumError> {
        Err(TessivumError::new(
            "MESSAGE_FEEDBACK_UNAVAILABLE",
            "this host does not support message feedback",
            "host",
            Value::Null,
        ))
    }
    async fn message_feedback_delete(
        &self,
        _session: SessionId,
        _message_id: String,
        _if_version: String,
    ) -> Result<Value, TessivumError> {
        Err(TessivumError::new(
            "MESSAGE_FEEDBACK_UNAVAILABLE",
            "this host does not support message feedback",
            "host",
            Value::Null,
        ))
    }
    async fn create_session(
        &self,
        session_id: SessionId,
    ) -> Result<HostSessionInfo, TessivumError> {
        Ok(HostSessionInfo {
            session_id,
            workspace_id: None,
            created_at: 0,
            updated_at: 0,
            running: false,
            cwd: None,
            parent_session: None,
            origin: None,
            agent_preset: None,
            event_count: 0,
            blank: true,
        })
    }
    async fn create_session_in(
        &self,
        session_id: SessionId,
        _workspace_id: WorkspaceId,
    ) -> Result<HostSessionInfo, TessivumError> {
        self.create_session(session_id).await
    }
    async fn delete_workspace(&self, _workspace_id: WorkspaceId) -> Result<bool, TessivumError> {
        Ok(false)
    }
    async fn list_sessions(&self) -> Result<Vec<HostSessionInfo>, TessivumError> {
        Ok(Vec::new())
    }
    async fn search_sessions(
        &self,
        _query: String,
    ) -> Result<HostSessionSearchResult, TessivumError> {
        Err(TessivumError::new(
            "SESSION_SEARCH_UNSUPPORTED",
            "this host does not support session search",
            "host",
            Value::Null,
        ))
    }
    async fn rename_session(
        &self,
        _session_id: SessionId,
        _title: String,
    ) -> Result<HostSessionRenameResult, TessivumError> {
        Err(TessivumError::new(
            "SESSION_RENAME_UNSUPPORTED",
            "this host does not support session rename",
            "host",
            Value::Null,
        ))
    }
    async fn fork_session(
        &self,
        _session_id: SessionId,
        _at_seq: Option<u64>,
    ) -> Result<SessionId, TessivumError> {
        Err(TessivumError::new(
            "SESSION_FORK_UNSUPPORTED",
            "this host does not support session fork",
            "host",
            Value::Null,
        ))
    }
    fn provider_directory(&self) -> Vec<HostProviderDirectoryEntry> {
        Vec::new()
    }
    fn model_groups(&self, _provider: &str) -> Vec<HostModelGroup> {
        Vec::new()
    }
    async fn provider_models(
        &self,
        provider: String,
        _config: Value,
    ) -> Result<HostProviderModels, TessivumError> {
        Err(TessivumError::new(
            "PROVIDER_MODELS_UNSUPPORTED",
            "this host does not support provider model discovery",
            "host",
            json!({
                "provider": provider,
                "attempts": 0,
                "retries": 0,
                "retryable": false,
            }),
        ))
    }
    async fn set_provider_enabled(
        &self,
        _provider: String,
        _enabled: bool,
    ) -> Result<HostProviderEnabled, TessivumError> {
        if self.settings().is_none() {
            return Err(TessivumError::new(
                "SETTINGS_UNAVAILABLE",
                "settings service is unavailable",
                "settings",
                json!({"namespace": LLM_PI_AI_NAMESPACE}),
            ));
        }
        Err(TessivumError::new(
            "PROVIDER_ENABLE_UNSUPPORTED",
            "this host does not support provider enablement",
            "host",
            Value::Null,
        ))
    }
    async fn session_models(
        &self,
        _session: SessionId,
    ) -> Result<HostSessionModels, TessivumError> {
        Ok(HostSessionModels {
            current: None,
            routable: false,
            groups: Vec::new(),
            failures: Vec::new(),
        })
    }
    async fn select_model(
        &self,
        _session: SessionId,
        _provider: String,
        _model: String,
        _reasoning_effort: Option<String>,
    ) -> Result<SessionModelSelection, TessivumError> {
        Err(TessivumError::new(
            "MODEL_SELECTION_UNSUPPORTED",
            "this host does not support model selection",
            "host",
            Value::Null,
        ))
    }
    fn attachment_limits(&self) -> AttachmentLimits {
        AttachmentLimits::default()
    }
    async fn upload_attachment(
        &self,
        _data: Vec<u8>,
        _name: Option<String>,
    ) -> Result<AttachmentRef, TessivumError> {
        Err(TessivumError::new(
            "ATTACHMENTS_UNSUPPORTED",
            "this host does not support attachments",
            "host",
            Value::Null,
        ))
    }
    async fn read_attachment(
        &self,
        _session: SessionId,
        _attachment_id: AttachmentId,
    ) -> Result<AttachmentData, TessivumError> {
        Err(TessivumError::new(
            "ATTACHMENTS_UNSUPPORTED",
            "this host does not support attachments",
            "host",
            Value::Null,
        ))
    }
    async fn normalize_prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptParams, TessivumError> {
        Ok(params)
    }
    async fn mutate_settings(
        &self,
        namespace: String,
        mutation: HostSettingsMutation,
    ) -> Result<SettingsSnapshot, SettingsError> {
        let settings = self.settings().ok_or(SettingsError::Closed)?;
        match mutation {
            HostSettingsMutation::Update {
                patch,
                expected_revision,
            } => settings.update(&namespace, patch, expected_revision).await,
            HostSettingsMutation::Replace {
                user,
                expected_revision,
            } => settings.replace(&namespace, user, expected_revision).await,
            HostSettingsMutation::Mutate {
                ops,
                expected_revision,
            } => settings.mutate(&namespace, ops, expected_revision).await,
        }
    }
    async fn agent_preset_list(&self) -> Result<Vec<AgentPresetSummary>, TessivumError> {
        Ok(Vec::new())
    }
    async fn agent_preset_read(
        &self,
        _agent_preset: String,
    ) -> Result<AgentPresetDocument, TessivumError> {
        Err(TessivumError::new(
            "AGENT_PRESET_NOT_FOUND",
            "agent preset was not found",
            "host",
            Value::Null,
        ))
    }
    async fn agent_preset_copy(
        &self,
        _from: String,
        _agent_preset: String,
        _name: Option<String>,
    ) -> Result<String, TessivumError> {
        Err(TessivumError::new(
            "AGENT_PRESET_UNSUPPORTED",
            "agent preset authoring is unavailable",
            "host",
            Value::Null,
        ))
    }
    async fn agent_preset_remove(&self, _agent_preset: String) -> Result<(), TessivumError> {
        Err(TessivumError::new(
            "AGENT_PRESET_UNSUPPORTED",
            "agent preset authoring is unavailable",
            "host",
            Value::Null,
        ))
    }
    async fn agent_preset_path(
        &self,
        _agent_preset: String,
    ) -> Result<(String, String), TessivumError> {
        Err(TessivumError::new(
            "AGENT_PRESET_NOT_FOUND",
            "agent preset was not found",
            "host",
            Value::Null,
        ))
    }
    async fn agent_preset_open_document(
        &self,
        agent_preset: String,
    ) -> Result<HostAgentPresetDocument, TessivumError> {
        let (trust, path) = self.agent_preset_path(agent_preset.clone()).await?;
        if trust != "user" {
            return Err(TessivumError::new(
                "agent-preset-read-only",
                "system presets are read-only",
                "host",
                json!({"agentPreset": agent_preset, "reason": "system presets are read-only"}),
            ));
        }
        Ok(HostAgentPresetDocument {
            opened: false,
            path: Some(path),
        })
    }
    async fn agent_preset_select(
        &self,
        _session: SessionId,
        _agent_preset: String,
    ) -> Result<String, TessivumError> {
        Err(TessivumError::new(
            "AGENT_PRESET_UNSUPPORTED",
            "agent preset selection is unavailable",
            "host",
            Value::Null,
        ))
    }
    fn agent_preset_capabilities(&self) -> (bool, bool) {
        (false, false)
    }
    fn descriptor(&self) -> HostDescriptor {
        HostDescriptor {
            cwd: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .to_string_lossy()
                .into_owned(),
            provider: "recorded".into(),
            model: "recorded".into(),
            max_tokens: None,
        }
    }
    fn settings(&self) -> Option<Arc<Settings>> {
        None
    }
    fn credentials(&self) -> Option<Arc<Credentials>> {
        None
    }
    fn dynamic_cordis_inventory(&self) -> Result<Value, TessivumError> {
        Ok(Value::Array(Vec::new()))
    }
    async fn dynamic_cordis_run_host_half(&self, _args: Value) -> Result<Value, TessivumError> {
        Err(TessivumError::new(
            "CORDIS_UNAVAILABLE",
            "dynamic Cordis compatibility is disabled",
            "cordis",
            Value::Null,
        ))
    }
    async fn dynamic_cordis_call(
        &self,
        method: &str,
        _args: Value,
    ) -> Result<Value, TessivumError> {
        if method == "syncInspectManifest" {
            return Ok(Value::Null);
        }
        Err(TessivumError::new(
            "CORDIS_UNAVAILABLE",
            format!("dynamic Cordis method {method} is unavailable"),
            "cordis",
            Value::Null,
        ))
    }
    fn subscribe(&self) -> broadcast::Receiver<HostNotification>;
    fn approval_registry(&self) -> Option<HostApprovalRegistry> {
        None
    }
    fn question_registry(&self) -> Option<HostQuestionRegistry> {
        None
    }
    fn workspace_registry(&self) -> Option<WorkspaceRegistry> {
        None
    }
    fn default_workspace_id(&self) -> Option<WorkspaceId> {
        None
    }
    fn can_open_path(&self) -> bool {
        false
    }
    fn has_settings_document(&self) -> bool {
        self.settings()
            .is_some_and(|settings| settings.document_path().is_some())
    }
    async fn shutdown(&self) -> Result<(), TessivumError>;
    async fn pick_directory(&self) -> Result<Option<String>, TessivumError> {
        Err(TessivumError::new(
            "DIRECTORY_PICKER_UNAVAILABLE",
            "native directory picker is unavailable",
            "host",
            json!({"capability": "absent"}),
        ))
    }
    async fn open_path(&self, _path: String) -> Result<(), TessivumError> {
        Err(TessivumError::new(
            "PATH_OPENER_UNAVAILABLE",
            "native path opener is unavailable",
            "host",
            Value::Null,
        ))
    }
    async fn open_settings_document(&self) -> Result<(), TessivumError> {
        Err(TessivumError::new(
            "SETTINGS_DOCUMENT_UNAVAILABLE",
            "settings document opener is unavailable",
            "host",
            Value::Null,
        ))
    }
}

/// Owns services and their graceful shutdown order.
pub struct HostRuntime {
    handle: HostHandle,
}

/// Transport-safe host handle with one admission fence shared by all clones.
#[derive(Clone)]
pub struct HostHandle {
    inner: Arc<HostInner>,
}

struct HostInner {
    identity: HostIdentity,
    profile: Value,
    config: HostConfig,
    settings: Arc<Settings>,
    credentials: Arc<Credentials>,
    path_opener: Arc<dyn HostPathOpener>,
    directory_picker: Arc<dyn HostDirectoryPicker>,
    llm: LlmRuntime,
    attachments: Arc<AttachmentStore>,
    route_adapter: Arc<dyn LlmAdapter>,
    dynamic_routes: bool,
    route_resolver: Arc<DynamicRouteResolver>,
    route_state: Mutex<RouteState>,
    route_gate: AsyncMutex<()>,

    cancellation: tessivum_core::CancellationToken,
    sessions: SessionStore,
    persistence: Arc<dyn SessionPersistence>,
    workspace_registry: WorkspaceRegistry,
    default_workspace_id: Option<WorkspaceId>,
    agent_presets: Arc<AgentPresetService>,
    message_feedback: MessageFeedbackStore,
    resources: Arc<SessionResourceResolver>,
    registry: AgentRegistry,
    subagents: SubagentService,
    approvals: HostApprovalRegistry,
    questions: HostQuestionRegistry,
    jobs: LocalJobRegistry,
    job_owners: BashJobOwners,
    schedule_owners: ScheduleOwners,
    skills: SkillRuntime,
    skill_scopes: SkillSessionScopes,
    skill_providers: Mutex<BTreeMap<PathBuf, SkillProviderRegistration>>,
    job_delivery: JobCompletionDelivery,
    telemetry: Option<TelemetryCoordinator>,
    code_runtime: Option<ProcessCodeRuntime>,
    subprocesses: SubprocessRuntime,
    legacy: Option<LegacyProfile>,
    loader: AsyncMutex<Option<Loader>>,
    services: Services,
    projections: ProjectionRegistry,
    dynamic_cordis: Option<DynamicCordisRegistry>,
    goal_tools: GoalToolRouter,
    planning_tools: PlanningToolRouter,
    owned_agents: Mutex<BTreeMap<SessionId, OwnedAgent>>,
    state: Mutex<State>,
    // ponytail: one Host-wide gate serializes session create/delete and agent handoff; shard by session only if contention matters.
    setup: AsyncMutex<()>,
    // ponytail: one Host-wide command gate keeps lifecycle and policy events contiguous; shard by session if command throughput matters.
    commands: AsyncMutex<()>,
    admission: Mutex<AdmissionState>,
    drained: Notify,
    shutdown: AsyncMutex<()>,
    notices: broadcast::Sender<HostNotification>,
    relay_stop: Notify,
    relays_closed: AtomicBool,
    relays: Mutex<Vec<JoinHandle<()>>>,
}
const MAX_MESSAGE_FEEDBACK_NOTE_BYTES: usize = 8_192;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageFeedbackItem {
    message_id: String,
    rating: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    version: String,
    created_at: u64,
    updated_at: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageFeedbackIdentity {
    created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct MessageFeedbackRow {
    session: MessageFeedbackIdentity,
    items: Vec<MessageFeedbackItem>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageFeedbackDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anonymous_user_id: Option<String>,
    #[serde(default)]
    sessions: BTreeMap<SessionId, MessageFeedbackRow>,
}

struct MessageFeedbackStore {
    path: PathBuf,
    document: AsyncMutex<MessageFeedbackDocument>,
}

enum MessageFeedbackPut {
    Item(MessageFeedbackItem),
    Conflict(Option<MessageFeedbackItem>),
}

enum MessageFeedbackDelete {
    Absent,
    Conflict(MessageFeedbackItem),
}

impl MessageFeedbackStore {
    async fn open(path: PathBuf) -> Result<Self, HostError> {
        let document = match fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                HostError::MessageFeedback(format!("cannot decode {}: {error}", path.display()))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                MessageFeedbackDocument::default()
            }
            Err(error) => {
                return Err(HostError::MessageFeedback(format!(
                    "cannot read {}: {error}",
                    path.display()
                )));
            }
        };
        Ok(Self {
            path,
            document: AsyncMutex::new(document),
        })
    }

    async fn anonymous_user_id(&self) -> Result<String, HostError> {
        let mut document = self.document.lock().await;
        if let Some(user_id) = &document.anonymous_user_id {
            return Ok(user_id.clone());
        }
        let mut next = document.clone();
        let user_id = Uuid::new_v4().to_string();
        next.anonymous_user_id = Some(user_id.clone());
        persist_message_feedback(&self.path, &next).await?;
        *document = next;
        Ok(user_id)
    }

    async fn list(
        &self,
        session_id: &SessionId,
        header: &SessionHeader,
    ) -> Vec<MessageFeedbackItem> {
        let document = self.document.lock().await;
        document
            .sessions
            .get(session_id)
            .filter(|row| message_feedback_identity_matches(&row.session, header))
            .map_or_else(Vec::new, |row| row.items.clone())
    }

    async fn put(
        &self,
        session_id: SessionId,
        header: &SessionHeader,
        message_id: String,
        rating: String,
        note: Option<String>,
        if_version: Option<String>,
    ) -> Result<MessageFeedbackPut, HostError> {
        let mut document = self.document.lock().await;
        let current = document
            .sessions
            .get(&session_id)
            .filter(|row| message_feedback_identity_matches(&row.session, header));
        let existing = current
            .and_then(|row| row.items.iter().find(|item| item.message_id == message_id))
            .cloned();
        if if_version.as_deref() != existing.as_ref().map(|item| item.version.as_str()) {
            return Ok(MessageFeedbackPut::Conflict(existing));
        }
        if existing
            .as_ref()
            .is_some_and(|item| item.rating == rating && item.note == note)
        {
            return Ok(MessageFeedbackPut::Item(existing.expect("checked above")));
        }
        let timestamp = now();
        let item = MessageFeedbackItem {
            message_id: message_id.clone(),
            rating,
            note,
            version: Uuid::new_v4().to_string(),
            created_at: existing.as_ref().map_or(timestamp, |item| item.created_at),
            updated_at: existing
                .as_ref()
                .map_or(timestamp, |item| timestamp.max(item.updated_at)),
        };
        let mut next = document.clone();
        let row = next
            .sessions
            .entry(session_id)
            .or_insert_with(|| MessageFeedbackRow {
                session: message_feedback_identity(header),
                items: Vec::new(),
            });
        if !message_feedback_identity_matches(&row.session, header) {
            *row = MessageFeedbackRow {
                session: message_feedback_identity(header),
                items: Vec::new(),
            };
        }
        if let Some(index) = row
            .items
            .iter()
            .position(|entry| entry.message_id == message_id)
        {
            row.items[index] = item.clone();
        } else {
            row.items.push(item.clone());
        }
        persist_message_feedback(&self.path, &next).await?;
        *document = next;
        Ok(MessageFeedbackPut::Item(item))
    }

    async fn delete(
        &self,
        session_id: SessionId,
        header: &SessionHeader,
        message_id: &str,
        if_version: &str,
    ) -> Result<MessageFeedbackDelete, HostError> {
        let mut document = self.document.lock().await;
        let current = document
            .sessions
            .get(&session_id)
            .filter(|row| message_feedback_identity_matches(&row.session, header));
        let existing = current
            .and_then(|row| row.items.iter().find(|item| item.message_id == message_id))
            .cloned();
        let Some(existing) = existing else {
            return Ok(MessageFeedbackDelete::Absent);
        };
        if if_version != existing.version {
            return Ok(MessageFeedbackDelete::Conflict(existing));
        }
        let mut next = document.clone();
        if let Some(row) = next.sessions.get_mut(&session_id) {
            row.items.retain(|item| item.message_id != message_id);
        }
        persist_message_feedback(&self.path, &next).await?;
        *document = next;
        Ok(MessageFeedbackDelete::Absent)
    }
}

fn message_feedback_identity(header: &SessionHeader) -> MessageFeedbackIdentity {
    MessageFeedbackIdentity {
        created_at: header.created_at,
        cwd: header.cwd.clone(),
    }
}

fn message_feedback_identity_matches(
    identity: &MessageFeedbackIdentity,
    header: &SessionHeader,
) -> bool {
    identity.created_at == header.created_at && identity.cwd == header.cwd
}

async fn persist_message_feedback(
    path: &Path,
    document: &MessageFeedbackDocument,
) -> Result<(), HostError> {
    let encoded = serde_json::to_vec(document)
        .map_err(|error| HostError::MessageFeedback(format!("cannot encode feedback: {error}")))?;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = async {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).await.map_err(|error| {
            HostError::MessageFeedback(format!("cannot create {}: {error}", temporary.display()))
        })?;
        file.write_all(&encoded).await.map_err(|error| {
            HostError::MessageFeedback(format!("cannot write {}: {error}", temporary.display()))
        })?;
        file.sync_all().await.map_err(|error| {
            HostError::MessageFeedback(format!("cannot flush {}: {error}", temporary.display()))
        })?;
        drop(file);
        fs::rename(&temporary, path).await.map_err(|error| {
            HostError::MessageFeedback(format!("cannot replace {}: {error}", path.display()))
        })
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temporary).await;
    }
    result
}

struct OwnedAgent {
    _approval: HostApprovalRegistration,
    _question: HostQuestionRegistration,
    _agent: AgentHandle,
    goals: GoalService,
    planning: PlanningService,
    jobs: JobOwner,
    schedule: ScheduleOwner,
}

const MAX_CONSECUTIVE_JOB_WAKES: u8 = 3;

#[derive(Clone)]
struct JobCompletionDelivery {
    sessions: SessionStore,
    registry: AgentRegistry,
    owners: BashJobOwners,
    cancellation: tessivum_core::CancellationToken,
    wakeups: Arc<Mutex<BTreeMap<SessionId, u8>>>,
}

impl JobCompletionDelivery {
    fn new(
        sessions: SessionStore,
        registry: AgentRegistry,
        owners: BashJobOwners,
        cancellation: tessivum_core::CancellationToken,
    ) -> Self {
        Self {
            sessions,
            registry,
            owners,
            cancellation,
            wakeups: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn reset(&self, session: &SessionId) {
        lock(&self.wakeups).remove(session);
    }

    fn observe(&self, job: JobSnapshot) {
        let delivery = self.clone();
        tokio::spawn(async move {
            delivery.deliver(job).await;
        });
    }

    async fn deliver(&self, job: JobSnapshot) {
        let Some(owner) = lock(&self.owners).get(&job.owner).cloned() else {
            return;
        };
        let Some(agent) = self.registry.get(&job.owner) else {
            return;
        };
        if !owner.matches_authority(&agent.authority()) {
            return;
        }
        let Ok(Some(notice)) = owner.completion_notice(&job.id) else {
            return;
        };
        let Some(session) = self.sessions.get(&job.owner) else {
            return;
        };
        if session
            .append_next(
                |seq| notice.session_event(seq, now()),
                self.cancellation.clone(),
            )
            .await
            .is_err()
        {
            return;
        }
        let message = Message {
            id: MessageId::from(format!("job-done-{}", job.id)),
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: format!(
                    "background job {} ({}: {}) finished [status: {}]. Read its output with jobs.read.",
                    job.id,
                    job.kind,
                    job.label,
                    job.status.as_str(),
                ),
            }],
            source: MessageSource::Plugin {
                plugin: "@deepseek-ai/dsh-tool-jobs".into(),
                compaction_id: None,
                form: Some(crate::protocol::ContextForm::Notice),
                sections: None,
                summary: Some(format!(
                    "{} {} [status: {}]",
                    job.kind,
                    job.label,
                    job.status.as_str(),
                )),
            },
        };
        let wake = agent.status() == AgentStatus::Idle && {
            let mut wakeups = lock(&self.wakeups);
            let count = wakeups.entry(job.owner.clone()).or_default();
            if *count >= MAX_CONSECUTIVE_JOB_WAKES {
                false
            } else {
                *count += 1;
                true
            }
        };
        let target = if wake {
            InboxTarget::Followup
        } else {
            InboxTarget::Inject
        };
        if session
            .append_next(
                |seq| inbox_enqueued_event(seq, target, &message),
                self.cancellation.clone(),
            )
            .await
            .is_err()
        {
            return;
        }
        let delivered = if wake {
            agent.followup(message).await
        } else {
            agent.inject(message).await
        };
        if delivered.is_ok() {
            let _ = owner.report(&job.id);
        }
    }
}

enum ImagePlan {
    Reference(AttachmentRef),
    Inline(AttachmentRef),
}
struct Services {
    root: ContextHandle,
    _sessions: ServiceHandle<SessionStore>,
    _llm: ServiceHandle<LlmRuntime>,
    _prompt: ServiceHandle<SystemPrompt>,
    _compaction: ServiceHandle<CompactionService>,
    _web: ServiceHandle<WebRuntime>,
    _web_fetch: WebFetchProviderRegistration,
    _web_search: WebSearchProviderRegistration,
    _tools: ServiceHandle<ToolRuntime>,
    _agents: ServiceHandle<AgentRegistry>,
    _subagents: ServiceHandle<SubagentService>,
    _subagent_provider: SubagentProviderRegistration,
    _subagent_tools: SubagentTools,
    _subagent_delegation_tools: SubagentDelegationTools,
    _workflow: ServiceHandle<WorkflowRuntime>,
    _workflow_tools: WorkflowTools,
    _sandbox: ServiceHandle<Sandbox>,
    _jobs: ServiceHandle<LocalJobRegistry>,
    _skills: ServiceHandle<SkillRuntime>,
    _job_observer: JobObserverRegistration,
    _job_done_observer: JobObserverRegistration,
    _job_tools: JobTools,
    _skill_tools: SkillTools,
    _schedule_tools: ScheduleTools,
    _subprocesses: ServiceHandle<SubprocessRuntime>,
    _settings: ServiceHandle<Arc<Settings>>,
    _credentials: ServiceHandle<Arc<Credentials>>,
    _attachments: ServiceHandle<Arc<AttachmentStore>>,
    _telemetry: Option<ServiceHandle<TelemetryCoordinator>>,
    _code: Option<ServiceHandle<ProcessCodeRuntime>>,
    _code_tool: Option<ToolRegistration>,
    _question_tool: ToolRegistration,
    _prompt_registration: Option<PromptRegistration>,
    _builtin_tools: BuiltinTools,
    _dynamic_cordis_tools: Option<DynamicCordisTools>,
    _goal_tools: GoalTools,
    _planning_tools: PlanningTools,
    _factory: AgentFactoryRegistration,
}

#[derive(Default)]
struct State {
    initialized: Option<InitializeParams>,
    statuses: BTreeMap<SessionId, SessionStatus>,
    relayed: BTreeSet<SessionId>,
    queue_relayed: BTreeSet<SessionId>,
    subagents: BTreeMap<SessionId, SubagentDescriptor>,
}

#[derive(Default)]
struct AdmissionState {
    closing: bool,
    count: usize,
}

struct Admission(Arc<HostInner>);

impl Drop for Admission {
    fn drop(&mut self) {
        let mut state = lock(&self.0.admission);
        state.count = state.count.saturating_sub(1);
        if state.closing && state.count == 0 {
            self.0.drained.notify_waiters();
        }
    }
}

impl HostRuntime {
    pub async fn boot(config: HostConfig) -> Result<Self, HostError> {
        let profile = config.compose_profile()?;
        let cwd = config
            .cwd
            .canonicalize()
            .map_err(|source| HostError::Canonicalize {
                path: config.cwd.clone(),
                source,
            })?;
        if !cwd.is_dir() {
            return Err(HostError::InvalidConfiguration(
                "cwd is not a directory".into(),
            ));
        }
        tokio::fs::create_dir_all(&config.data_dir)
            .await
            .map_err(|source| HostError::CreateDataDir {
                path: config.data_dir.clone(),
                source,
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&config.data_dir, std::fs::Permissions::from_mode(0o700))
                .await
                .map_err(|source| HostError::CreateDataDir {
                    path: config.data_dir.clone(),
                    source,
                })?;
        }
        let data_dir =
            config
                .data_dir
                .canonicalize()
                .map_err(|source| HostError::Canonicalize {
                    path: config.data_dir.clone(),
                    source,
                })?;
        let (notices, _) = broadcast::channel(config.notification_capacity);
        if !data_dir.is_dir() {
            return Err(HostError::InvalidConfiguration(
                "data_dir is not a directory".into(),
            ));
        }
        let mut preset_roots = config.agent_preset_roots.clone();
        let implicit_user_preset_root = data_dir.join(".agent-presets");
        let authoring_root = if config.include_user_preset_root {
            preset_roots.push(AgentPresetRoot {
                path: implicit_user_preset_root.clone(),
                trust: AgentPresetTrust::User,
            });
            Some(implicit_user_preset_root)
        } else {
            preset_roots
                .iter()
                .find(|root| root.trust == AgentPresetTrust::User)
                .map(|root| root.path.clone())
        };
        if let Some(user_preset_root) = &authoring_root {
            tokio::fs::create_dir_all(user_preset_root)
                .await
                .map_err(|source| HostError::CreateDataDir {
                    path: user_preset_root.clone(),
                    source,
                })?;
        }
        let agent_presets = Arc::new(AgentPresetService::with_roots(preset_roots, authoring_root));

        let settings_path = host_file_path(
            &data_dir,
            config.settings_path.as_deref(),
            "settings.yaml",
            "settings_path",
        )?;
        let credentials_path = host_file_path(
            &data_dir,
            config.credentials_path.as_deref(),
            "credentials.yaml",
            "credentials_path",
        )?;

        let root = ContextHandle::root();
        let cancellation = root.scope().cancellation();
        let attachments = Arc::new(AttachmentStore::new(
            data_dir.join("attachments"),
            AttachmentLimits::default(),
        )?);
        let attachments_service =
            root.provide(attachments_service_key(), Arc::clone(&attachments))?;
        let settings = Arc::new(Settings::new(Arc::new(YamlSettingsProvider::new(
            settings_path,
        ))));
        let settings_service = root.provide(settings_service_key(), Arc::clone(&settings))?;
        let credentials = Arc::new(Credentials::new(Arc::new(YamlCredentialFile::new(
            credentials_path,
        ))));
        let credentials_service =
            root.provide(credentials_service_key(), Arc::clone(&credentials))?;
        let settings_base = profile
            .get(LLM_PI_AI_NAMESPACE)
            .cloned()
            .unwrap_or_else(|| json!({}));
        let llm_settings_applies = if config.adapter_factory.is_none()
            && config.recorded_replay.is_none()
            && (config.provider != "recorded"
                || config.model != "recorded"
                || settings_base
                    .get("providers")
                    .and_then(Value::as_object)
                    .is_some_and(|providers| !providers.is_empty()))
        {
            SettingsApplies::Live
        } else {
            SettingsApplies::Restart
        };
        settings
            .register(openai_settings_registration(
                settings_base,
                llm_settings_applies,
            ))
            .await
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let deepseek_base = profile
            .get(LLM_DEEPSEEK_NAMESPACE)
            .cloned()
            .unwrap_or_else(|| json!({}));
        settings
            .register(deepseek_settings_registration(
                deepseek_base,
                llm_settings_applies,
            ))
            .await
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let default_base = profile
            .get(AGENT_DEFAULT_MODEL_NAMESPACE)
            .cloned()
            .unwrap_or_else(|| json!({}));
        settings
            .register(default_model_registration(&config, default_base))
            .await
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        for (namespace, field, choices, default) in [
            ("locale", "preference", &["zh", "en"][..], None),
            (
                "ui-theme",
                "preference",
                &["light", "dark", "system"][..],
                Some("system"),
            ),
            (
                "ui-conversation",
                "busyEnter",
                &["queue", "steer"][..],
                Some("queue"),
            ),
        ] {
            let base = profile.get(namespace).cloned().unwrap_or_else(|| json!({}));
            settings
                .register(choice_settings_registration(
                    namespace, field, choices, default, base,
                ))
                .await
                .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        }
        for (namespace, fields, defaults) in [
            (
                "shell",
                &["timeoutMs", "maxOutputBytes"][..],
                json!({"timeoutMs": 60000, "maxOutputBytes": 65536}),
            ),
            (
                "agent-loop",
                &["maxParallelToolCalls"][..],
                json!({"maxParallelToolCalls": 4}),
            ),
        ] {
            let base = profile.get(namespace).cloned().unwrap_or_else(|| json!({}));
            settings
                .register(positive_integer_settings_registration(
                    namespace, fields, defaults, base,
                ))
                .await
                .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        }
        let web_search_base = profile
            .get("web-search-deepseek")
            .cloned()
            .unwrap_or_else(|| json!({}));
        settings
            .register(web_search_settings_registration(web_search_base))
            .await
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let permission_base = profile
            .get(PERMISSION_SETTINGS_NAMESPACE)
            .cloned()
            .unwrap_or_else(|| json!({}));
        settings
            .register(permission_settings_registration(permission_base))
            .await
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let onboarding_base = profile
            .get("ui-onboarding")
            .cloned()
            .unwrap_or_else(|| json!({}));
        settings
            .register(SettingsRegistration::new(
                "ui-onboarding",
                json!({"type": "object", "properties": {"welcomeNoticeVersion": {"type": "string"}}}),
                json!({}),
                onboarding_base,
            ))
            .await
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let preset_base = profile
            .get("agent-presets")
            .cloned()
            .unwrap_or_else(|| json!({}));
        settings
            .register(SettingsRegistration::new(
                "agent-presets",
                json!({"type": "object", "properties": {"default": {"type": "string"}}, "additionalProperties": false}),
                json!({"default": "standard"}),
                preset_base,
            ))
            .await
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let route_snapshot = settings
            .get(LLM_PI_AI_NAMESPACE)
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let mut initial_routes = parse_routes(&route_snapshot.value, route_snapshot.revision)
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let deepseek_snapshot = settings
            .get(LLM_DEEPSEEK_NAMESPACE)
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let deepseek_route =
            parse_deepseek_route(&deepseek_snapshot.value, deepseek_snapshot.revision)
                .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        initial_routes.retry_policies.insert(
            DEEPSEEK_PROVIDER.into(),
            LlmRetryPolicy::resolve(None).expect("default retry policy is valid"),
        );
        initial_routes
            .routes
            .insert(DEEPSEEK_PROVIDER.into(), deepseek_route);
        let route_map = Arc::new(initial_routes.routes);
        let retry_policies = Arc::new(initial_routes.retry_policies);
        let route_resolver = Arc::new(DynamicRouteResolver {
            routes: Arc::new(Mutex::new(Arc::clone(&route_map))),
            credentials: Arc::clone(&credentials),
        });
        let dynamic_routes = config.adapter_factory.is_none()
            && config.recorded_replay.is_none()
            && (config.provider != "recorded" || config.model != "recorded");
        let persistence: Arc<dyn SessionPersistence> =
            Arc::new(JsonlSessionPersistence::new(&data_dir));
        let session_inspections = persistence.list(cancellation.clone()).await?;
        let workspace_registry = WorkspaceRegistry::open(&data_dir, &cwd, session_inspections)?;
        let default_workspace_id = workspace_registry
            .list()
            .into_iter()
            .find(|workspace| Path::new(&workspace.path) == cwd.as_path())
            .map(|workspace| workspace.workspace_id);
        let resources = Arc::new(SessionResourceResolver::new(workspace_registry.clone()));
        let sessions = SessionStore::new(Arc::clone(&persistence));
        let session_service = root.provide(session_service_key(), sessions.clone())?;

        let llm = LlmRuntime::new();
        let adapter: Arc<dyn LlmAdapter> = if dynamic_routes {
            Arc::new(CompatibleApiAdapter::with_resolver_and_store(
                (*route_resolver).clone(),
                Arc::clone(&attachments),
            ))
        } else {
            adapter_for(&config)?
        };
        let mut registrations = BTreeMap::new();
        if dynamic_routes {
            for provider in route_map.keys() {
                let retry_policy = retry_policies
                    .get(provider)
                    .cloned()
                    .expect("parsed route has a retry policy");
                registrations.insert(
                    provider.clone(),
                    llm.register_with_retry_policy(
                        provider.clone(),
                        Arc::clone(&adapter),
                        Some(retry_policy),
                    )?,
                );
            }
        } else {
            registrations.insert(
                config.provider.clone(),
                llm.register(config.provider.clone(), Arc::clone(&adapter))?,
            );
        }
        let llm_service = llm.clone().publish(&root)?;
        let prompt = SystemPrompt::new();
        let system_prompt = if config.code_runtime.is_some() {
            Some(match config.system_prompt.as_deref() {
                Some(prompt) => format!("{prompt}\n\n{CODE_MODE_PROMPT}"),
                None => CODE_MODE_PROMPT.into(),
            })
        } else {
            config.system_prompt.clone()
        };
        let prompt_registration = system_prompt
            .as_ref()
            .map(|text| prompt.register(PromptSection::new("host", 0, text.clone())))
            .transpose()?;
        let prompt_service = prompt.clone().publish(&root)?;
        let web = WebRuntime::from_env()?;
        let web_fetch = web.register_fetch(
            "http",
            Arc::new(HttpFetchProvider::new(HttpFetchConfig::default())?),
        )?;
        let web_search = web.register_search(
            DEEPSEEK_SEARCH_PROVIDER,
            Arc::new(DeepSeekSearchProvider::new(
                Arc::clone(&credentials),
                sessions.clone(),
            )?),
        )?;
        let web_service = web.clone().publish(&root)?;
        let tools = ToolRuntime::new();
        let approvals = HostApprovalRegistry::new();
        tools.set_approval(Some(Arc::new(approvals.clone())));
        let questions = HostQuestionRegistry::new();
        let question_tool = register_ask_user_question_tool(&tools, questions.clone())?;
        let sandbox = Sandbox::local();
        let sandbox_service = sandbox.clone().publish(&root)?;
        let jobs = LocalJobRegistry::new();
        let (dynamic_cordis, dynamic_cordis_tools) = if config.dynamic_cordis {
            let registry = DynamicCordisRegistry::new(notices.clone())
                .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
            let registrations = registry
                .register_tools(&tools)
                .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
            (Some(registry), Some(registrations))
        } else {
            (None, None)
        };
        let jobs_service = jobs.clone().publish(&root)?;
        let job_owners: BashJobOwners = Arc::new(Mutex::new(BTreeMap::new()));
        let schedule_owners: ScheduleOwners = Arc::new(Mutex::new(BTreeMap::new()));
        let builtin_tools = BuiltinTools::for_host(
            &tools,
            BuiltinToolsConfig {
                enable_bash: config.enable_trusted_bash,
                cwd: cwd.clone(),
                resolver: Some(Arc::clone(&resources)),
                ..BuiltinToolsConfig::default()
            },
            HostToolServices::new(
                sessions.clone(),
                sandbox,
                Arc::new(approvals.clone()),
                Arc::clone(&job_owners),
                Arc::clone(&attachments),
                web,
            ),
        )?;
        let job_tools = JobTools::install_for_owners(&tools, Arc::clone(&job_owners))?;
        let schedule_tools =
            ScheduleTools::install_for_owners(&tools, Arc::clone(&schedule_owners))?;
        let skills = SkillRuntime::new();
        let skills_service = skills.clone().publish(&root)?;
        let skill_scopes = skill_session_scopes();
        let skill_tools = SkillTools::register_for_scopes(
            &tools,
            skills.clone(),
            Arc::clone(&skill_scopes),
            Arc::new(AllowSkillInvocation),
        )?;
        let approval_restrictions = config
            .approval_required_tools
            .iter()
            .cloned()
            .fold(ToolRestrictions::new(), ToolRestrictions::ask);
        let dispatch_tools = if config.approval_required_tools.is_empty() {
            tools.clone()
        } else {
            tools.scoped(approval_restrictions.clone())?
        };
        let code_tool = config
            .code_runtime
            .as_ref()
            .map(|runtime| register_code_tool(&tools, dispatch_tools.clone(), runtime.clone()))
            .transpose()?;
        let mut agent_tools = if code_tool.is_some() {
            tools.scoped(ToolRestrictions::allow_only(["run_code"]))?
        } else {
            dispatch_tools.clone()
        };
        if !config.approval_required_tools.is_empty() && code_tool.is_none() {
            agent_tools = agent_tools.scoped(approval_restrictions)?;
        }
        let goal_tools = GoalToolRouter::default();
        let goal_registrations = goal_tools
            .register_tools(&tools)
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let planning_tools = PlanningToolRouter::default();
        let planning_registrations = planning_tools
            .register_tools(&tools, questions.clone())
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let projections = ProjectionRegistry::new();
        let compaction = CompactionService::new(
            llm.clone(),
            CompactionConfig {
                provider: config.provider.clone(),
                model: config.model.clone(),
                ..CompactionConfig::default()
            },
        )
        .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let compaction_service = compaction.publish(&root)?;
        let tools_service = tools.publish(&root)?;
        let registry = AgentRegistry::new(sessions.clone());
        let subagents =
            SubagentService::new(registry.clone(), sessions.clone(), Arc::clone(&persistence));
        let subagent_tools = SubagentTools::install(&tools, subagents.clone())?;
        let subagent_delegation_tools =
            SubagentDelegationTools::install(&tools, subagents.clone(), Arc::clone(&job_owners))?;
        let workflow_engine =
            NativeWorkflowEngine::from_recording(config.recorded_replay.as_deref())
                .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let workflow = WorkflowRuntime::new(
            sessions.clone(),
            subagents.clone(),
            Arc::new(workflow_engine),
            64,
        )
        .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let workflow_service = workflow.clone().publish(&root)?;
        let workflow_tools = register_workflow_tool(&tools, workflow.clone(), registry.clone())?;
        let factory = AgentLoopFactory::new(llm.clone(), prompt.clone(), agent_tools)
            .with_dispatch_tools(tools.clone())
            .with_approval_required_tools(config.approval_required_tools.clone())
            .with_presets(Arc::clone(&agent_presets))
            .with_compaction(compaction)
            .with_skills(skills.clone(), Arc::clone(&skill_scopes))
            .with_context_window_resolver({
                let routes = Arc::clone(&route_resolver.routes);
                let recorded = config.recorded_replay_context_window;
                Arc::new(move |provider: &str, model: &str| {
                    recorded.or_else(|| {
                        lock(&routes)
                            .get(provider)
                            .and_then(|route| route.models.iter().find(|item| item.id == model))
                            .and_then(|item| item.context_window)
                    })
                })
            });
        let factory = if code_tool.is_some() {
            factory.with_code_mode()
        } else {
            factory.with_standard_catalog()
        };
        let factory = registry.register_factory(Arc::new(factory))?;
        let subagent_provider = subagents
            .register(
                "native",
                Arc::new(NativeSubagentProvider::new(
                    registry.clone(),
                    Vec::<String>::new(),
                )),
            )
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
        let subagents_service = subagents.clone().publish(&root)?;
        let agents_service = registry.clone().publish(&root)?;
        let subprocesses = SubprocessRuntime::new();
        let subprocess_service = subprocesses.publish(&root)?;
        let telemetry_service = config
            .telemetry
            .as_ref()
            .map(|value| value.clone().publish(&root))
            .transpose()?;
        let code_service = config
            .code_runtime
            .as_ref()
            .map(|value| value.clone().publish(&root))
            .transpose()?;

        let needs_legacy = config.entries.as_ref().is_some_and(|entries| {
            entries
                .active_entries()
                .iter()
                .any(|entry| entry.options.runtime == tessivum_core::RuntimeKind::LegacyNode)
        });
        let legacy = if needs_legacy {
            match (&config.legacy_profile, &config.legacy_host) {
                (Some(profile), _) => Some(profile.clone()),
                (None, Some(host)) => Some(
                    LegacyProfile::new(
                        host.command.clone(),
                        host.client.clone(),
                        BridgeServices::new(
                            tools.clone(),
                            prompt.clone(),
                            llm.clone(),
                            sessions.clone(),
                            registry.clone(),
                        )
                        .with_settings(Arc::clone(&settings))
                        .with_credentials(Arc::clone(&credentials))
                        .with_pnpm_boundary(Arc::new(
                            PnpmProfileBoundary::new(plugin_profile_root(&config.data_dir))
                                .map_err(|error| {
                                    HostError::InvalidConfiguration(error.to_string())
                                })?,
                        )),
                    )
                    .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?,
                ),
                (None, None) => None,
            }
        } else {
            None
        };
        let loader = if let Some(entries) = config.entries.clone() {
            let resolver: Arc<dyn PackageResolver> = match &config.package_resolver {
                Some(value) => Arc::clone(value),
                None => Arc::new(
                    ProductPackageResolver::new()
                        .confine_to(&cwd)
                        .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?,
                ),
            };
            if let Some(profile) = &legacy {
                profile
                    .start()
                    .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
            }
            let policies = WasmPolicyRegistry::new();
            let capabilities = Arc::new(CapabilityRegistry::new());
            let wasm_bridge = DomainBridge::with_policy_registry(
                BridgeServices::new(
                    tools,
                    prompt.clone(),
                    llm.clone(),
                    sessions.clone(),
                    registry.clone(),
                )
                .with_settings(Arc::clone(&settings))
                .with_credentials(Arc::clone(&credentials)),
                policies.clone(),
            )
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
            capabilities
                .register(Capability::ServiceCall, move |request| {
                    CapabilityHandler::call(&wasm_bridge, request)
                })
                .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
            capabilities.grant(Capability::ServiceCall);
            let wasm = Arc::new(WasmProductRuntime::new(
                capabilities,
                policies,
                config.wasm_limits.clone(),
            ));

            let mut loader = match product_loader(legacy.as_ref(), resolver, wasm) {
                Ok(loader) => loader.with_context(root.clone()),
                Err(error) => {
                    if let Some(profile) = &legacy {
                        let _ = profile.shutdown().await;
                    }
                    let _ = root.scope().dispose().await;
                    return Err(HostError::InvalidConfiguration(error.to_string()));
                }
            };
            if let Err(error) = loader.load(entries).await {
                if let Some(profile) = &legacy {
                    let _ = profile.shutdown().await;
                }
                let _ = root.scope().dispose().await;
                return Err(HostError::InvalidConfiguration(format!(
                    "Core Loader activation failed: {error}"
                )));
            }
            Some(loader)
        } else {
            None
        };
        let message_feedback =
            MessageFeedbackStore::open(data_dir.join("message-feedback.json")).await?;
        let job_delivery = JobCompletionDelivery::new(
            sessions.clone(),
            registry.clone(),
            Arc::clone(&job_owners),
            cancellation.clone(),
        );
        let completion_delivery = job_delivery.clone();
        let job_done_observer = jobs.on_done(move |job| {
            completion_delivery.observe(job.clone());
        });

        let jobs_for_observer = jobs.clone();
        let job_notices = notices.clone();
        let job_observer = jobs.on_changed(move |job| {
            let _ = job_notices.send(HostNotification::SessionJobs(HostSessionJobsNotification {
                session_id: job.owner.clone(),
                jobs: jobs_for_observer.list_session(&job.owner),
            }));
        });
        let path_opener: Arc<dyn HostPathOpener> = config
            .path_opener
            .clone()
            .unwrap_or_else(|| Arc::new(SystemPathOpener));
        let directory_picker: Arc<dyn HostDirectoryPicker> = config
            .directory_picker
            .clone()
            .unwrap_or_else(|| Arc::new(SystemDirectoryPicker));
        let inner = Arc::new(HostInner {
            identity: HostIdentity {
                cwd,
                data_dir,
                profile: config.profile.clone(),
            },
            profile,
            config: config.clone(),
            settings,
            credentials,
            path_opener,
            directory_picker,
            llm,
            job_delivery,
            attachments: Arc::clone(&attachments),
            route_adapter: Arc::clone(&adapter),
            dynamic_routes,
            route_resolver,
            route_state: Mutex::new(RouteState {
                routes: route_map,
                retry_policies,
                registrations,
            }),
            route_gate: AsyncMutex::new(()),
            cancellation,
            sessions,
            persistence,
            workspace_registry,
            default_workspace_id,
            projections,
            agent_presets,
            message_feedback,
            resources,
            registry,
            subagents,
            approvals,
            jobs,
            questions,
            job_owners,
            schedule_owners,
            skills,
            skill_scopes,
            skill_providers: Mutex::new(BTreeMap::new()),
            telemetry: config.telemetry.clone(),
            code_runtime: config.code_runtime.clone(),
            subprocesses,
            legacy,
            loader: AsyncMutex::new(loader),
            dynamic_cordis,
            goal_tools,
            planning_tools,
            services: Services {
                root,
                _sessions: session_service,
                _llm: llm_service,
                _prompt: prompt_service,
                _compaction: compaction_service,
                _web: web_service,
                _web_fetch: web_fetch,
                _web_search: web_search,
                _tools: tools_service,
                _agents: agents_service,
                _subagents: subagents_service,
                _subagent_provider: subagent_provider,
                _subagent_tools: subagent_tools,
                _subagent_delegation_tools: subagent_delegation_tools,
                _workflow: workflow_service,
                _workflow_tools: workflow_tools,
                _jobs: jobs_service,
                _skills: skills_service,
                _job_done_observer: job_done_observer,
                _job_tools: job_tools,
                _skill_tools: skill_tools,
                _schedule_tools: schedule_tools,
                _job_observer: job_observer,
                _subprocesses: subprocess_service,
                _settings: settings_service,
                _credentials: credentials_service,
                _attachments: attachments_service,
                _telemetry: telemetry_service,
                _code: code_service,
                _code_tool: code_tool,
                _prompt_registration: prompt_registration,
                _question_tool: question_tool,
                _sandbox: sandbox_service,
                _builtin_tools: builtin_tools,
                _dynamic_cordis_tools: dynamic_cordis_tools,
                _goal_tools: goal_registrations,
                _planning_tools: planning_registrations,
                _factory: factory,
            },
            owned_agents: Mutex::new(BTreeMap::new()),
            state: Mutex::new(State::default()),
            setup: AsyncMutex::new(()),
            commands: AsyncMutex::new(()),
            admission: Mutex::new(AdmissionState::default()),
            drained: Notify::new(),
            shutdown: AsyncMutex::new(()),
            notices,
            relay_stop: Notify::new(),
            relays_closed: AtomicBool::new(false),
            relays: Mutex::new(Vec::new()),
        });
        HostHandle::start_service_relays(&inner);
        let handle = HostHandle { inner };
        handle.start_approval_relay();
        handle.start_question_relay();
        Ok(Self { handle })
    }

    pub fn register_projection(
        &self,
        definition: ProjectionDefinition,
    ) -> Result<(), TessivumError> {
        self.handle.register_projection(definition)
    }

    pub fn handle(&self) -> HostHandle {
        self.handle.clone()
    }
    pub fn identity(&self) -> &HostIdentity {
        self.handle.identity()
    }
    pub fn profile(&self) -> &Value {
        self.handle.profile()
    }
    pub async fn shutdown(&self) -> Result<(), HostError> {
        self.handle.shutdown_inner().await
    }
}

impl HostHandle {
    pub(crate) fn dynamic_cordis(&self) -> Option<&DynamicCordisRegistry> {
        self.inner.dynamic_cordis.as_ref()
    }

    pub fn register_projection(
        &self,
        definition: ProjectionDefinition,
    ) -> Result<(), TessivumError> {
        self.inner
            .projections
            .register(definition)
            .map_err(|error| {
                TessivumError::new(error.code(), error.to_string(), "projection", Value::Null)
            })
    }
    async fn append_dynamic_cordis_fact(
        &self,
        session_id: SessionId,
        method: &str,
        args: Value,
        result: Value,
    ) -> Result<(), TessivumError> {
        let _admission = self.admit().map_err(HostError::wire)?;
        validate_session(&session_id).map_err(HostError::wire)?;
        let session = self
            .inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| HostError::from(SessionError::NotFound(session_id.clone())))
            .map_err(HostError::wire)?;
        let args = args.as_object().map_or_else(Map::new, |source| {
            [
                "agentId",
                "pluginId",
                "packageId",
                "pluginRunId",
                "requestId",
                "mode",
            ]
            .into_iter()
            .filter_map(|key| source.get(key).cloned().map(|value| (key.into(), value)))
            .collect()
        });
        self.append_command_event(
            &session,
            "cordis/dynamic",
            json!({"method": method, "args": args, "result": result}),
        )
        .await
        .map(|_| ())
        .map_err(HostError::wire)
    }

    async fn pick_directory_inner(&self) -> Result<Option<String>, TessivumError> {
        let _admission = self.admit().map_err(HostError::wire)?;
        let selected = self.inner.directory_picker.pick_directory().await?;
        let Some(path) = selected else {
            return Ok(None);
        };
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            TessivumError::new(
                "DIRECTORY_INVALID_PATH",
                "selected directory is unavailable",
                "host",
                json!({"path": path, "error": error.to_string()}),
            )
        })?;
        if !canonical.is_dir() {
            return Err(TessivumError::new(
                "DIRECTORY_INVALID_PATH",
                "selected path is not a directory",
                "host",
                json!({"path": canonical}),
            ));
        }
        Ok(Some(canonical.to_string_lossy().into_owned()))
    }

    async fn open_path_inner(&self, path: String) -> Result<(), TessivumError> {
        let _admission = self.admit().map_err(HostError::wire)?;
        if path.trim().is_empty() {
            return Err(TessivumError::new(
                "PATH_INVALID",
                "path must not be blank",
                "host",
                Value::Null,
            ));
        }
        let requested = PathBuf::from(path);
        let requested = if requested.is_absolute() {
            requested
        } else {
            self.inner.identity.cwd.join(requested)
        };
        let canonical = std::fs::canonicalize(&requested).map_err(|error| {
            TessivumError::new(
                "PATH_INVALID",
                "path is unavailable",
                "host",
                json!({"error": error.to_string()}),
            )
        })?;
        if !self.inner.path_opener.can_open_path() {
            return Err(TessivumError::new(
                "PATH_OPENER_UNAVAILABLE",
                "native path opener is unavailable",
                "host",
                Value::Null,
            ));
        }
        let workspace_contains = self
            .inner
            .workspace_registry
            .list()
            .into_iter()
            .filter_map(|workspace| std::fs::canonicalize(workspace.path).ok())
            .any(|root| canonical.starts_with(root));
        if !canonical.starts_with(&self.inner.identity.cwd)
            && !canonical.starts_with(&self.inner.identity.data_dir)
            && !workspace_contains
        {
            return Err(TessivumError::new(
                "PATH_UNSAFE",
                "path resolves outside a host-owned workspace",
                "host",
                json!({"path": canonical}),
            ));
        }
        self.inner.path_opener.open_path(canonical).await
    }

    async fn open_settings_document_inner(&self) -> Result<(), TessivumError> {
        let _admission = self.admit().map_err(HostError::wire)?;
        if !self.inner.path_opener.can_open_path() {
            return Err(TessivumError::new(
                "SETTINGS_DOCUMENT_UNAVAILABLE",
                "native path opener is unavailable",
                "settings",
                Value::Null,
            ));
        }
        let path = self
            .inner
            .settings
            .prepare_document()
            .await
            .map_err(|error| error.as_tessivum_error())?
            .ok_or_else(|| {
                TessivumError::new(
                    "SETTINGS_DOCUMENT_UNAVAILABLE",
                    "settings provider has no local document to open",
                    "settings",
                    json!({}),
                )
            })?;
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            TessivumError::new(
                "SETTINGS_DOCUMENT_UNAVAILABLE",
                "settings document is unavailable",
                "settings",
                json!({"error": error.to_string()}),
            )
        })?;
        self.inner.path_opener.open_text_file(canonical).await
    }

    pub async fn agent_preset_list(&self) -> Result<Vec<AgentPresetSummary>, HostError> {
        let mut presets = self
            .inner
            .agent_presets
            .list()
            .await
            .map_err(preset_error)?;
        let configured = self
            .inner
            .settings
            .get("agent-presets")
            .ok()
            .and_then(|snapshot| snapshot.value.get("default")?.as_str().map(str::to_owned));
        let default = configured
            .filter(|id| presets.iter().any(|preset| preset.id == *id))
            .or_else(|| presets.first().map(|preset| preset.id.clone()));
        for preset in &mut presets {
            preset.is_default = default.as_deref() == Some(&preset.id);
        }
        Ok(presets)
    }
    pub async fn agent_preset_read(&self, id: String) -> Result<AgentPresetDocument, HostError> {
        self.inner
            .agent_presets
            .read(&id)
            .await
            .map_err(preset_error)
    }
    pub async fn agent_preset_copy(
        &self,
        from: String,
        target: String,
        name: Option<String>,
    ) -> Result<String, HostError> {
        let _admission = self.admit()?;
        self.inner
            .agent_presets
            .copy(&from, &target, name)
            .await
            .map_err(preset_error)
    }
    pub async fn agent_preset_remove(&self, id: String) -> Result<(), HostError> {
        let _admission = self.admit()?;
        self.inner
            .agent_presets
            .remove(&id)
            .await
            .map_err(preset_error)
    }
    pub async fn agent_preset_path(&self, id: String) -> Result<(String, String), HostError> {
        let (trust, path) = self
            .inner
            .agent_presets
            .path(&id)
            .await
            .map_err(preset_error)?;
        Ok((
            match trust {
                crate::agent_preset::AgentPresetTrust::System => "system",
                crate::agent_preset::AgentPresetTrust::User => "user",
            }
            .into(),
            path.to_string_lossy().into_owned(),
        ))
    }
    pub async fn agent_preset_open_document(
        &self,
        id: String,
    ) -> Result<HostAgentPresetDocument, HostError> {
        let _admission = self.admit()?;
        let (trust, path) = self
            .inner
            .agent_presets
            .path(&id)
            .await
            .map_err(preset_error)?;
        if trust != AgentPresetTrust::User {
            return Err(HostError::Runtime(TessivumError::new(
                "agent-preset-read-only",
                "system presets are read-only",
                "host",
                json!({"agentPreset": id, "reason": "system presets are read-only"}),
            )));
        }
        let path = path.to_string_lossy().into_owned();
        if let Some(open) = &self.inner.config.path_opener {
            open.open_path(PathBuf::from(&path))
                .await
                .map_err(HostError::Runtime)?;
            return Ok(HostAgentPresetDocument {
                opened: true,
                path: None,
            });
        }
        Ok(HostAgentPresetDocument {
            opened: false,
            path: Some(path),
        })
    }

    pub async fn agent_preset_select(
        &self,
        session_id: SessionId,
        preset: String,
    ) -> Result<String, HostError> {
        let _admission = self.admit()?;
        crate::agent_preset::validate_id_public(&preset).map_err(|reason| {
            HostError::Runtime(TessivumError::new(
                "agent-preset-invalid",
                "agent preset was rejected",
                "host",
                json!({"agentPreset": preset, "reason": reason}),
            ))
        })?;
        let _ = self
            .inner
            .agent_presets
            .read(&preset)
            .await
            .map_err(preset_error)?;
        let _setup = self.inner.setup.lock().await;
        let session = match self.inner.sessions.get(&session_id) {
            Some(session) => session,
            None => {
                self.inner
                    .sessions
                    .restore(
                        &session_id,
                        crate::session::RestoreMode::Live,
                        self.inner.cancellation.clone(),
                    )
                    .await?
            }
        };
        if let Some(agent) = self.inner.registry.get(&session_id) {
            if agent.status() == AgentStatus::Running {
                return Err(HostError::Runtime(TessivumError::new(
                    "agent-preset-locked",
                    "session is running",
                    "host",
                    json!({"sessionId": session_id, "agentPreset": preset}),
                )));
            }
        }
        let events = session.events();
        if has_model_visible_work(&events) {
            return Err(HostError::Runtime(TessivumError::new(
                "agent-preset-locked",
                "agent preset can only be selected for a blank session",
                "host",
                json!({"sessionId": session_id, "agentPreset": preset}),
            )));
        }
        self.inner
            .agent_presets
            .prepare_selection(&preset)
            .await
            .map_err(preset_error)?;
        if let Some(agent) = self.inner.registry.get(&session_id) {
            self.inner.approvals.cancel_session(&session_id);
            let _ = self
                .inner
                .registry
                .cancel(&session_id, AgentCancelCause::Disposed, false);
            self.release_owned_agent(&session_id).await;
            agent.dispose().await?;
        }
        let event = SessionEvent {
            event_type: "agent-preset/selected".into(),
            seq: session.next_seq()?,
            time: now(),
            data: crate::agent_preset::selected_event_data(&preset),
            ignorable: Some(true),
            source_event_seqs: None,
            surface_op: None,
        };
        session
            .append(event, self.inner.cancellation.clone())
            .await?;
        self.ensure_relay(session);
        Ok(preset)
    }
    pub fn identity(&self) -> &HostIdentity {
        &self.inner.identity
    }
    pub fn profile(&self) -> &Value {
        &self.inner.profile
    }

    /// Returns the active Legacy Node profile's dynamic route registry, if any.
    pub fn web_route_registry(&self) -> Option<DomainBridge> {
        self.inner
            .legacy
            .as_ref()
            .map(LegacyProfile::web_route_registry)
    }
    pub fn in_flight(&self) -> usize {
        lock(&self.inner.admission).count
    }
    pub fn is_shutting_down(&self) -> bool {
        lock(&self.inner.admission).closing
    }
    pub fn provider_directory(&self) -> Vec<HostProviderDirectoryEntry> {
        let state = lock(&self.inner.route_state);
        let active = |provider: &str| {
            state
                .registrations
                .get(provider)
                .is_some_and(LlmProviderRegistration::is_active)
        };
        let entry = |route: ResponsesRoute,
                     namespace: &str,
                     settings_path: Vec<String>,
                     declared: bool| HostProviderDirectoryEntry {
            credential_configured: credential_configured(
                &self.inner.credentials,
                &route.credential_ref,
            ),
            active: active(&route.id),
            route,
            namespace: namespace.into(),
            settings_path,
            declared,
        };
        let mut entries =
            Vec::with_capacity(BUILTIN_PI_AI_PROVIDERS.len() + state.routes.len() + 1);
        if active(DEEPSEEK_PROVIDER) {
            entries.push(entry(
                state
                    .routes
                    .get(DEEPSEEK_PROVIDER)
                    .cloned()
                    .unwrap_or_else(default_deepseek_route),
                LLM_DEEPSEEK_NAMESPACE,
                Vec::new(),
                false,
            ));
        }
        for provider in BUILTIN_PI_AI_PROVIDERS {
            entries.push(entry(
                state
                    .routes
                    .get(provider.id)
                    .cloned()
                    .unwrap_or_else(|| builtin_pi_ai_route(*provider)),
                LLM_PI_AI_NAMESPACE,
                vec!["providers".into(), provider.id.into()],
                false,
            ));
        }
        entries.extend(
            state
                .routes
                .iter()
                .filter(|(id, _)| {
                    id.as_str() != DEEPSEEK_PROVIDER && builtin_pi_ai_provider(id).is_none()
                })
                .map(|(id, route)| {
                    entry(
                        route.clone(),
                        LLM_PI_AI_NAMESPACE,
                        vec!["providers".into(), id.clone()],
                        true,
                    )
                }),
        );
        entries
    }

    pub fn model_groups(&self, provider: &str) -> Vec<HostModelGroup> {
        let state = lock(&self.inner.route_state);
        state
            .routes
            .get(provider)
            .cloned()
            .map(|route| model_group_for_route(&self.inner.credentials, route))
            .into_iter()
            .collect()
    }

    pub async fn provider_models(
        &self,
        provider: String,
        config: Value,
    ) -> Result<HostProviderModels, HostError> {
        let _admission = self.admit()?;
        let rows = self.inner.llm.models(provider.clone(), config).await?;
        Ok(HostProviderModels {
            provider: provider.clone(),
            models: normalize_provider_models(&provider, rows)?,
            updated_at: now(),
        })
    }
    pub async fn set_provider_enabled(
        &self,
        provider: String,
        enabled: bool,
    ) -> Result<HostProviderEnabled, HostError> {
        let _admission = self.admit()?;
        let active = !provider.is_empty() && {
            let state = lock(&self.inner.route_state);
            state.routes.contains_key(&provider)
                && state
                    .registrations
                    .get(&provider)
                    .is_some_and(LlmProviderRegistration::is_active)
        };
        if !active {
            return Err(HostError::Runtime(model_error(
                "LLM_PROVIDER_NOT_FOUND",
                "provider route is not registered",
                &provider,
                None,
            )));
        }

        let descriptor = self.inner.settings.describe(LLM_PI_AI_NAMESPACE)?;
        let mut document = descriptor.user;
        {
            let document_object = document
                .as_object_mut()
                .ok_or(SettingsError::InvalidDocument)?;
            let providers = match document_object.entry("providers") {
                serde_json::map::Entry::Vacant(entry) => entry.insert(Value::Object(Map::new())),
                serde_json::map::Entry::Occupied(entry) => entry.into_mut(),
            }
            .as_object_mut()
            .ok_or(SettingsError::InvalidDocument)?;
            let provider_settings = match providers.entry(provider.clone()) {
                serde_json::map::Entry::Vacant(entry) => entry.insert(Value::Object(Map::new())),
                serde_json::map::Entry::Occupied(entry) => entry.into_mut(),
            }
            .as_object_mut()
            .ok_or(SettingsError::InvalidDocument)?;
            match provider_settings.get("enabled") {
                Some(Value::Bool(current)) if *current == enabled => {
                    return Ok(HostProviderEnabled { provider, enabled });
                }
                Some(Value::Bool(_)) | None => {}
                Some(_) => return Err(HostError::from(SettingsError::InvalidDocument)),
            }
            provider_settings.insert("enabled".into(), Value::Bool(enabled));
        }
        self.inner
            .settings
            .replace(LLM_PI_AI_NAMESPACE, document, Some(descriptor.revision))
            .await?;
        let _ = self.inner.notices.send(HostNotification::AdaptersUpdated);
        Ok(HostProviderEnabled { provider, enabled })
    }

    pub async fn session_models(
        &self,
        session_id: SessionId,
    ) -> Result<HostSessionModels, HostError> {
        validate_session(&session_id)?;
        let events = if let Some(session) = self.inner.sessions.get(&session_id) {
            session.events()
        } else {
            self.inner
                .persistence
                .read_from(&session_id, 0, self.inner.cancellation.clone())
                .await?
        };
        let current = latest_model_selection(&events).or_else(|| {
            Some(if self.inner.dynamic_routes {
                self.initial_selection()
            } else {
                self.config_selection()
            })
        });
        let groups = {
            let state = lock(&self.inner.route_state);
            state
                .routes
                .values()
                .cloned()
                .map(|route| model_group_for_route(&self.inner.credentials, route))
                .collect::<Vec<_>>()
        };
        let mut failures = Vec::new();
        let routable = current
            .as_ref()
            .is_some_and(|selection| self.selection_is_routable(selection, &mut failures));
        Ok(HostSessionModels {
            current,
            routable,
            groups,
            failures,
        })
    }

    pub async fn select_model(
        &self,
        session_id: SessionId,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Result<SessionModelSelection, HostError> {
        let _admission = self.admit()?;
        validate_session(&session_id)?;
        let selection = SessionModelSelection {
            provider,
            model,
            reasoning_effort,
        };
        selection.validate()?;
        if !self.inner.dynamic_routes && selection != self.config_selection() {
            return Err(HostError::Runtime(TessivumError::new(
                "MODEL_SELECTION_RESTART_REQUIRED",
                "this adapter applies model changes only after restart",
                "host",
                json!({"restartRequired": true}),
            )));
        }
        let selection = {
            let state = lock(&self.inner.route_state);
            selection_with_default_effort(state.routes.as_ref(), selection)
        };
        self.validate_selection(&selection)?;
        let _setup = self.inner.setup.lock().await;
        let session = match self.inner.sessions.get(&session_id) {
            Some(session) => session,
            None => {
                self.inner
                    .sessions
                    .restore(
                        &session_id,
                        crate::session::RestoreMode::Live,
                        self.inner.cancellation.clone(),
                    )
                    .await?
            }
        };
        if let Some(agent) = self.inner.registry.get(&session_id) {
            if agent.status() == AgentStatus::Running {
                return Err(HostError::invalid(
                    "SESSION_BUSY",
                    "cannot select a model while the session is running",
                ));
            }
            self.inner.approvals.cancel_session(&session_id);
            let _ = self
                .inner
                .registry
                .cancel(&session_id, AgentCancelCause::Disposed, false);
            self.release_owned_agent(&session_id).await;
            let _ = agent.dispose().await;
        }
        let event = SessionEvent {
            event_type: "session/model-selected".into(),
            seq: session.next_seq()?,
            time: now(),
            data: serde_json::to_value(&selection).unwrap_or(Value::Null),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
        };
        session
            .append(event, self.inner.cancellation.clone())
            .await?;
        self.inner
            .settings
            .replace(
                AGENT_DEFAULT_MODEL_NAMESPACE,
                serde_json::to_value(&selection).expect("model selection serializes"),
                None,
            )
            .await?;
        self.ensure_relay(session);
        Ok(selection)
    }

    fn default_selection(&self) -> Option<SessionModelSelection> {
        self.inner
            .settings
            .get(AGENT_DEFAULT_MODEL_NAMESPACE)
            .ok()
            .and_then(|snapshot| serde_json::from_value(snapshot.value).ok())
    }

    fn config_selection(&self) -> SessionModelSelection {
        SessionModelSelection {
            provider: self.inner.config.provider.clone(),
            model: self.inner.config.model.clone(),
            reasoning_effort: None,
        }
    }

    fn initial_selection(&self) -> SessionModelSelection {
        let state = lock(&self.inner.route_state);
        if let Some(selection) = self.default_selection() {
            let declared = state
                .routes
                .get(&selection.provider)
                .is_some_and(|route| route.models.iter().any(|model| model.id == selection.model));
            let explicit = self
                .inner
                .settings
                .user(AGENT_DEFAULT_MODEL_NAMESPACE)
                .ok()
                .and_then(|value| serde_json::from_value::<SessionModelSelection>(value).ok())
                .is_some_and(|value| value == selection);
            if !self.inner.dynamic_routes || declared || explicit {
                return selection_with_default_effort(state.routes.as_ref(), selection);
            }
        }
        let selection = state
            .routes
            .get(&self.inner.config.provider)
            .and_then(|route| {
                route.models.first().map(|model| SessionModelSelection {
                    provider: route.id.clone(),
                    model: model.id.clone(),
                    reasoning_effort: None,
                })
            })
            .or_else(|| {
                state.routes.values().find_map(|route| {
                    route.models.first().map(|model| SessionModelSelection {
                        provider: route.id.clone(),
                        model: model.id.clone(),
                        reasoning_effort: None,
                    })
                })
            })
            .unwrap_or_else(|| self.config_selection());
        selection_with_default_effort(state.routes.as_ref(), selection)
    }

    async fn append_model_selection(
        &self,
        session: &Arc<crate::session::Session>,
        selection: &SessionModelSelection,
    ) -> Result<(), HostError> {
        let event = SessionEvent {
            event_type: "session/model-selected".into(),
            seq: session.next_seq()?,
            time: now(),
            data: serde_json::to_value(selection).unwrap_or(Value::Null),
            ignorable: None,
            source_event_seqs: None,
            surface_op: None,
        };
        session
            .append(event, self.inner.cancellation.clone())
            .await?;
        self.ensure_relay(Arc::clone(session));
        Ok(())
    }
    async fn pin_initial_permission(&self, session: &Session) -> Result<(), HostError> {
        let events = session.events();
        let mut knobs = permission_knobs(&events);
        if knobs.preset().is_none()
            && knobs.sandbox().is_none()
            && knobs.approval().is_none()
            && !events
                .iter()
                .any(|event| event.event_type == "session/end-seed")
        {
            let settings = self
                .inner
                .settings
                .get(PERMISSION_SETTINGS_NAMESPACE)
                .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
            let name = settings
                .value
                .get("defaultPreset")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    HostError::invalid(
                        "INVALID_PERMISSION_PRESET",
                        "permission.defaultPreset must be a supported preset",
                    )
                })?;
            let spec = permission_preset(name).ok_or_else(|| {
                HostError::invalid(
                    "INVALID_PERMISSION_PRESET",
                    "permission.defaultPreset must be a supported preset",
                )
            })?;
            self.append_command_event(session, "permission/preset", json!({"preset": name}))
                .await?;
            self.append_command_event(session, "sandbox/mode", json!({"mode": spec.sandbox}))
                .await?;
            self.append_command_event(session, "approval/policy", json!({"policy": spec.approval}))
                .await?;
            return Ok(());
        }

        if knobs.preset().is_none() {
            let current = current_permission(&knobs);
            if current != CUSTOM_PERMISSION_PRESET {
                self.append_command_event(session, "permission/preset", json!({"preset": current}))
                    .await?;
                knobs.select_preset(current);
            }
        }
        if knobs.sandbox().is_none() {
            self.append_command_event(
                session,
                "sandbox/mode",
                json!({"mode": crate::sandbox::SandboxMode::WorkspaceWrite}),
            )
            .await?;
        }
        if knobs.approval().is_none() {
            self.append_command_event(
                session,
                "approval/policy",
                json!({"policy": crate::approval::ApprovalPolicy::Ask}),
            )
            .await?;
        }
        Ok(())
    }

    async fn selection_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionModelSelection, HostError> {
        if !self.inner.dynamic_routes {
            return Ok(self.config_selection());
        }
        let session = if let Some(session) = self.inner.sessions.get(session_id) {
            session
        } else {
            let events = self
                .inner
                .persistence
                .read_from(session_id, 0, self.inner.cancellation.clone())
                .await?;
            if let Some(selection) = latest_model_selection(&events) {
                let selection = {
                    let state = lock(&self.inner.route_state);
                    selection_with_default_effort(state.routes.as_ref(), selection)
                };
                self.validate_selection(&selection)?;
                return Ok(selection);
            }
            if events.is_empty()
                && self
                    .inner
                    .persistence
                    .inspect(session_id, self.inner.cancellation.clone())
                    .await?
                    .is_none()
            {
                let selection = self.initial_selection();
                self.validate_selection(&selection)?;
                return Ok(selection);
            }
            self.inner
                .sessions
                .restore(
                    session_id,
                    crate::session::RestoreMode::Live,
                    self.inner.cancellation.clone(),
                )
                .await?
        };
        if let Some(selection) = latest_model_selection(&session.events()) {
            let selection = {
                let state = lock(&self.inner.route_state);
                selection_with_default_effort(state.routes.as_ref(), selection)
            };
            self.validate_selection(&selection)?;
            return Ok(selection);
        }
        let selection = self.initial_selection();
        self.validate_selection(&selection)?;
        self.append_model_selection(&session, &selection).await?;
        Ok(selection)
    }

    fn selection_is_routable(
        &self,
        selection: &SessionModelSelection,
        failures: &mut Vec<HostRouteFailure>,
    ) -> bool {
        let state = lock(&self.inner.route_state);
        let Some(route) = state.routes.get(&selection.provider) else {
            if selection.provider == self.inner.config.provider
                && selection.model == self.inner.config.model
                && (!self.inner.dynamic_routes || state.routes.is_empty())
            {
                return true;
            }
            failures.push(route_failure(
                "LLM_PROVIDER_NOT_FOUND",
                "provider route is not registered",
                selection,
                None,
            ));
            return false;
        };
        let Some(model) = route
            .models
            .iter()
            .find(|model| model.id == selection.model)
        else {
            failures.push(route_failure(
                "LLM_MODEL_NOT_FOUND",
                "model is not declared by provider route",
                selection,
                None,
            ));
            return false;
        };
        if selection.reasoning_effort.as_ref().is_some_and(|effort| {
            !model
                .reasoning_efforts
                .iter()
                .any(|candidate| candidate.id == *effort)
        }) {
            failures.push(route_failure(
                "INVALID_REASONING_EFFORT",
                "reasoning effort is not declared by the selected model",
                selection,
                Some(model.id.clone()),
            ));
            return false;
        }
        if !route.credential_ref.is_empty()
            && !credential_configured(&self.inner.credentials, &route.credential_ref)
        {
            failures.push(route_failure(
                "MISSING_CREDENTIAL",
                "provider credential is not configured",
                selection,
                Some(model.id.clone()),
            ));
            return false;
        }
        true
    }

    fn validate_selection(&self, selection: &SessionModelSelection) -> Result<(), HostError> {
        let mut failures = Vec::new();
        if self.selection_is_routable(selection, &mut failures) {
            Ok(())
        } else {
            let failure = failures.into_iter().next().unwrap_or_else(|| {
                route_failure(
                    "MODEL_NOT_ROUTABLE",
                    "model is not routable",
                    selection,
                    None,
                )
            });
            Err(HostError::invalid(failure.code, failure.message))
        }
    }
    pub fn attachment_limits(&self) -> AttachmentLimits {
        self.inner.attachments.limits().clone()
    }

    async fn read_attachment_inner(
        &self,
        session: SessionId,
        attachment_id: AttachmentId,
    ) -> Result<AttachmentData, HostError> {
        validate_session(&session)?;
        let attachment_id = AttachmentId::try_from(attachment_id.as_str())?;
        let events = if let Some(session) = self.inner.sessions.get(&session) {
            session.events()
        } else {
            self.inner
                .persistence
                .read_from(&session, 0, self.inner.cancellation.clone())
                .await?
        };
        let reference = events
            .iter()
            .find_map(|event| find_attachment_ref(&event.data, &attachment_id))
            .ok_or_else(|| {
                HostError::invalid(
                    "ATTACHMENT_NOT_REFERENCED",
                    "attachment is not referenced by the session",
                )
            })?;
        self.inner
            .attachments
            .read_ref_bounded(&reference, self.inner.attachments.limits().max_image_bytes)
            .await
            .map_err(HostError::from)
    }

    async fn upload_attachment_inner(
        &self,
        data: Vec<u8>,
        name: Option<String>,
    ) -> Result<AttachmentRef, HostError> {
        let _admission = self.admit()?;
        Ok(self
            .inner
            .attachments
            .save(AttachmentInput::new(data, name))
            .await?)
    }

    async fn normalize_prompt_inner(
        &self,
        mut params: SessionPromptParams,
    ) -> Result<SessionPromptParams, HostError> {
        validate_session(&params.session_id)?;
        let mut plans = Vec::new();
        let mut inputs = Vec::new();
        collect_image_plans(
            &params.content_blocks,
            &self.inner.attachments,
            &mut plans,
            &mut inputs,
        )?;
        if plans.is_empty() {
            return Ok(params);
        }

        let limits = self.inner.attachments.limits();
        if plans.len() > limits.max_images_per_message {
            return Err(AttachmentError::BatchCountLimit.into());
        }
        let mut total_bytes = 0u64;
        for plan in &plans {
            let reference = match plan {
                ImagePlan::Reference(reference) | ImagePlan::Inline(reference) => reference,
            };
            if !limits.media_types.contains(&reference.media_type) {
                return Err(AttachmentError::UnsupportedMediaType.into());
            }
            let pixels = u64::from(reference.width)
                .checked_mul(u64::from(reference.height))
                .ok_or(AttachmentError::PixelLimit)?;
            if reference.bytes > limits.max_image_bytes {
                return Err(AttachmentError::ByteLimit.into());
            }
            if pixels == 0 || pixels > limits.max_image_pixels {
                return Err(AttachmentError::PixelLimit.into());
            }
            total_bytes = total_bytes
                .checked_add(reference.bytes)
                .ok_or(AttachmentError::BatchByteLimit)?;
            if total_bytes > limits.max_message_image_bytes {
                return Err(AttachmentError::BatchByteLimit.into());
            }
        }

        if self.inner.dynamic_routes {
            let events = if let Some(session) = self.inner.sessions.get(&params.session_id) {
                session.events()
            } else {
                self.inner
                    .persistence
                    .read_from(&params.session_id, 0, self.inner.cancellation.clone())
                    .await?
            };
            let selection =
                latest_model_selection(&events).or_else(|| Some(self.initial_selection()));
            let supports_image = selection
                .as_ref()
                .and_then(|selection| {
                    let state = lock(&self.inner.route_state);
                    state.routes.get(&selection.provider).and_then(|route| {
                        route
                            .models
                            .iter()
                            .find(|model| model.id == selection.model)
                            .cloned()
                    })
                })
                .is_some_and(|model| {
                    let input = if model.input.is_empty() {
                        &[RESPONSES_TEXT_MODALITY.to_owned()][..]
                    } else {
                        &model.input
                    };
                    input
                        .iter()
                        .any(|modality| modality == RESPONSES_IMAGE_MODALITY)
                });
            if !supports_image {
                return Err(HostError::invalid(
                    "UNSUPPORTED_MODALITY",
                    "the selected model does not support image input",
                ));
            }
        }

        for plan in &plans {
            if let ImagePlan::Reference(reference) = plan {
                self.inner.attachments.read_ref(reference).await?;
            }
        }
        let inline_refs = if inputs.is_empty() {
            Vec::new()
        } else {
            self.inner.attachments.save_batch(inputs).await?
        };
        let mut plan_index = 0;
        let mut inline_index = 0;
        replace_image_plans(
            &mut params.content_blocks,
            &plans,
            &mut plan_index,
            &inline_refs,
            &mut inline_index,
        )?;
        Ok(params)
    }

    async fn initialize_inner(
        &self,
        mut params: InitializeParams,
    ) -> Result<InitializeResult, HostError> {
        let _admission = self.admit()?;
        params.validate()?;
        if params.provider.trim().is_empty() || params.model.trim().is_empty() {
            return Err(HostError::invalid(
                "INVALID_INITIALIZE_PARAMS",
                "provider and model must not be blank",
            ));
        }
        let cwd =
            Path::new(&params.cwd)
                .canonicalize()
                .map_err(|source| HostError::Canonicalize {
                    path: PathBuf::from(&params.cwd),
                    source,
                })?;
        if cwd != self.inner.identity.cwd
            || params.provider != self.inner.config.provider
            || params.model != self.inner.config.model
            || params.max_tokens != self.inner.config.max_tokens
        {
            return Err(HostError::InitializationConflict);
        }
        params.cwd = cwd.to_string_lossy().into_owned();
        let mut state = lock(&self.inner.state);
        if state
            .initialized
            .as_ref()
            .is_some_and(|current| current != &params)
        {
            return Err(HostError::InitializationConflict);
        }
        state.initialized = Some(params);
        Ok(InitializeResult {
            server_info: SdkServerInfo {
                name: "deepseek-harness-sdk-runtime".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        })
    }

    async fn search_sessions_inner(
        &self,
        query: String,
    ) -> Result<HostSessionSearchResult, HostError> {
        let _admission = self.admit()?;
        let visible = self
            .inner
            .persistence
            .list(self.inner.cancellation.clone())
            .await?
            .into_iter()
            .filter(|session| session.header.cwd.is_some())
            .map(|session| session.header.id)
            .collect::<BTreeSet<_>>();
        let query_engine = SessionQuery::new(
            self.inner.sessions.clone(),
            Arc::clone(&self.inner.persistence),
        );
        let mut matches = query_engine
            .search_visible(&query, &visible, self.inner.cancellation.clone())
            .await
            .map_err(|error| HostError::invalid(error.code(), error.to_string()))?;
        let has_more = matches.len() > SESSION_SEARCH_RESULT_LIMIT;
        matches.truncate(SESSION_SEARCH_RESULT_LIMIT);
        Ok(HostSessionSearchResult {
            items: matches
                .into_iter()
                .map(|hit| HostSessionSearchHit {
                    session_id: hit.session_id,
                    snippet: hit.snippet,
                })
                .collect(),
            has_more,
        })
    }

    async fn rename_session_inner(
        &self,
        session_id: SessionId,
        title: String,
    ) -> Result<HostSessionRenameResult, HostError> {
        let _admission = self.admit()?;
        validate_session(&session_id)?;
        let title = normalize_session_title(&title)?;
        let _setup = self.inner.setup.lock().await;
        let session = match self.inner.sessions.get(&session_id) {
            Some(session) => session,
            None => {
                self.inner
                    .sessions
                    .restore(
                        &session_id,
                        RestoreMode::Metadata,
                        self.inner.cancellation.clone(),
                    )
                    .await?
            }
        };
        let event_title = title.clone();
        let seq = session
            .append_next(
                move |seq| SessionEvent {
                    event_type: "session/title".into(),
                    seq,
                    time: now(),
                    data: json!({"title": event_title, "messageSeqs": [], "source": {"kind": "user"}}),
                    ignorable: None,
                    source_event_seqs: None,
                    surface_op: None,
                },
                self.inner.cancellation.clone(),
            )
            .await?;
        self.ensure_relay(session);
        Ok(HostSessionRenameResult { title, seq })
    }

    async fn fork_session_inner(
        &self,
        session_id: SessionId,
        at_seq: Option<u64>,
    ) -> Result<SessionId, HostError> {
        let _admission = self.admit()?;
        validate_session(&session_id)?;
        let _setup = self.inner.setup.lock().await;
        let query = SessionQuery::new(
            self.inner.sessions.clone(),
            Arc::clone(&self.inner.persistence),
        );
        let source = query
            .read(&session_id, self.inner.cancellation.clone())
            .await
            .map_err(|error| HostError::invalid(error.code(), error.to_string()))?;
        let last_seq = source.events.last().map(|event| event.seq);
        let boundary = at_seq
            .and_then(|requested| {
                source
                    .events
                    .iter()
                    .position(|event| event.event_type == "turn/end" && event.seq >= requested)
            })
            .or_else(|| {
                (at_seq.is_none() || at_seq > last_seq)
                    .then(|| {
                        source
                            .events
                            .iter()
                            .rposition(|event| event.event_type == "turn/end")
                    })
                    .flatten()
            })
            .ok_or_else(|| {
                HostError::invalid(
                    "FORK_UNAVAILABLE",
                    if at_seq.is_some() && at_seq <= last_seq {
                        "session has not completed the requested turn"
                    } else {
                        "session has no completed turn to fork from"
                    },
                )
            })?;
        let mut end = boundary + 1;
        while end < source.events.len() && source.events[end].event_type != "turn/start" {
            end += 1;
        }
        let seed = fork_seed(&source.events[..end])?;
        let workspace_id = self.workspace_for_fork(&query, &source.header).await?;
        let child_id = SessionId::random();
        let child = self
            .inner
            .sessions
            .create_seeded(
                SessionHeader {
                    version: SESSION_FORMAT_VERSION,
                    id: child_id.clone(),
                    created_at: now(),
                    cwd: source.header.cwd.clone(),
                    parent_session: Some(source.header.id.clone()),
                    seed_length: Some(seed.len() as u64),
                    origin: None,
                    delegation_depth: source.header.delegation_depth,
                    agent_preset: source.header.agent_preset.clone(),
                },
                seed,
                self.inner.cancellation.clone(),
            )
            .await?;
        self.pin_initial_permission(&child).await?;
        self.inner.workspace_registry.recognize_session(&child_id)?;
        if let Some(workspace_id) = workspace_id {
            self.inner
                .workspace_registry
                .attach_session(&workspace_id, &child_id, None)
                .map_err(|source| HostError::WorkspaceAttach {
                    session_id: child_id.clone(),
                    workspace_id,
                    source,
                })?;
        }
        self.ensure_relay(child);
        Ok(child_id)
    }

    async fn workspace_for_fork(
        &self,
        query: &SessionQuery,
        header: &SessionHeader,
    ) -> Result<Option<WorkspaceId>, HostError> {
        let mut seen = BTreeSet::new();
        let mut current = header.clone();
        loop {
            if !seen.insert(current.id.clone()) {
                return Err(HostError::invalid(
                    "FORK_UNAVAILABLE",
                    "session lineage contains a cycle",
                ));
            }
            if let Some(workspace) = self
                .inner
                .workspace_registry
                .workspace_for_session(&current.id)
            {
                return Ok(Some(workspace.workspace_id));
            }
            if let Some(cwd) = current.cwd.as_deref() {
                if let Some(workspace) = self
                    .inner
                    .workspace_registry
                    .snapshot()
                    .items
                    .into_iter()
                    .find(|workspace| workspace.path == cwd)
                {
                    return Ok(Some(workspace.workspace_id));
                }
            }
            let Some(parent) = current.parent_session.clone() else {
                return Ok(None);
            };
            current = query
                .read(&parent, self.inner.cancellation.clone())
                .await
                .map_err(|error| HostError::invalid(error.code(), error.to_string()))?
                .header;
        }
    }

    async fn create_session_inner(
        &self,
        session_id: SessionId,
    ) -> Result<HostSessionInfo, HostError> {
        let _admission = self.admit()?;
        let _setup = self.inner.setup.lock().await;
        self.create_session_in_unadmitted(session_id, self.default_workspace_id()?)
            .await
    }

    async fn create_session_in_inner(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<HostSessionInfo, HostError> {
        let _admission = self.admit()?;
        let _setup = self.inner.setup.lock().await;
        self.create_session_in_unadmitted(session_id, workspace_id)
            .await
    }

    async fn create_session_in_unadmitted(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<HostSessionInfo, HostError> {
        validate_session(&session_id)?;
        let lease = self.inner.workspace_registry.resolve(&workspace_id)?;
        let root = lease.validate_current()?;
        let existing = match self.inner.sessions.get(&session_id) {
            Some(session) => Some((session.header(), session.events().len() as u64)),
            None => self
                .inner
                .persistence
                .inspect(&session_id, self.inner.cancellation.clone())
                .await?
                .map(|session| (session.header, session.event_count)),
        };
        if let Some((header, event_count)) = existing {
            self.require_session_root(&header, &root)?;
            if self
                .inner
                .workspace_registry
                .workspace_for_session(&session_id)
                .is_some_and(|workspace| workspace.workspace_id != workspace_id)
            {
                return Err(HostError::invalid(
                    "SESSION_CONFLICT",
                    "session is already attached to another workspace",
                ));
            }
            self.inner
                .workspace_registry
                .recognize_session(&session_id)?;
            self.inner
                .workspace_registry
                .attach_session(&workspace_id, &session_id, None)
                .map_err(|source| HostError::WorkspaceAttach {
                    session_id: session_id.clone(),
                    workspace_id: workspace_id.clone(),
                    source,
                })?;
            let events = self
                .inner
                .persistence
                .read_from(&session_id, 0, self.inner.cancellation.clone())
                .await?;
            return Ok(HostSessionInfo {
                session_id: session_id.clone(),
                workspace_id: Some(workspace_id),
                created_at: header.created_at,
                updated_at: header.created_at,
                running: self.session_running(&session_id),
                cwd: header.cwd,
                parent_session: header.parent_session,
                origin: header.origin,
                agent_preset: header.agent_preset,
                event_count,
                blank: !has_model_visible_work(&events),
            });
        }

        let created_at = now();
        let cwd = root.to_string_lossy().into_owned();
        let selection = if self.inner.dynamic_routes {
            self.initial_selection()
        } else {
            self.config_selection()
        };
        let agent_preset = self
            .agent_preset_list()
            .await?
            .into_iter()
            .find(|preset| preset.is_default)
            .map(|preset| preset.id);
        let session = self
            .inner
            .sessions
            .create(
                SessionHeader {
                    version: SESSION_FORMAT_VERSION,
                    id: session_id.clone(),
                    created_at,
                    cwd: Some(cwd.clone()),
                    parent_session: None,
                    seed_length: None,
                    origin: None,
                    delegation_depth: Some(0),
                    agent_preset: agent_preset.clone(),
                },
                self.inner.cancellation.clone(),
            )
            .await?;
        self.append_model_selection(&session, &selection).await?;
        self.pin_initial_permission(&session).await?;

        self.inner
            .workspace_registry
            .recognize_session(&session_id)?;
        self.inner
            .workspace_registry
            .attach_session(&workspace_id, &session_id, None)
            .map_err(|source| HostError::WorkspaceAttach {
                session_id: session_id.clone(),
                workspace_id: workspace_id.clone(),
                source,
            })?;
        Ok(HostSessionInfo {
            session_id,
            workspace_id: Some(workspace_id),
            created_at,
            updated_at: created_at,
            running: false,
            cwd: Some(cwd),
            parent_session: None,
            origin: None,
            agent_preset,
            event_count: session.events().len() as u64,
            blank: !has_model_visible_work(&session.events()),
        })
    }

    async fn delete_workspace_inner(&self, workspace_id: WorkspaceId) -> Result<bool, HostError> {
        let _admission = self.admit()?;
        let _setup = self.inner.setup.lock().await;
        let Some(workspace) = self
            .inner
            .workspace_registry
            .list()
            .into_iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
        else {
            return Ok(false);
        };
        let sessions = workspace.session_ids;
        for session_id in sessions {
            self.inner.approvals.cancel_session(&session_id);
            self.inner.questions.cancel_session(&session_id);
            self.release_owned_agent(&session_id).await;
            if let Some(agent) = self.inner.registry.get(&session_id) {
                agent.cancel(AgentCancelCause::Disposed, false);
                agent.dispose().await?;
            }
            lock(&self.inner.state).statuses.remove(&session_id);
        }
        self.inner.workspace_registry.delete(workspace_id, None)?;
        Ok(true)
    }

    fn default_workspace_id(&self) -> Result<WorkspaceId, HostError> {
        let workspace_id = self.inner.default_workspace_id.clone().ok_or_else(|| {
            HostError::invalid(
                "WORKSPACE_NOT_FOUND",
                "host cwd workspace is not registered",
            )
        })?;
        self.inner.workspace_registry.resolve(&workspace_id)?;
        Ok(workspace_id)
    }

    fn require_session_root(&self, header: &SessionHeader, root: &Path) -> Result<(), HostError> {
        let valid = header
            .cwd
            .as_deref()
            .and_then(|cwd| Path::new(cwd).canonicalize().ok())
            .is_some_and(|cwd| cwd == root);
        if valid {
            Ok(())
        } else {
            Err(HostError::invalid(
                "SESSION_CONFLICT",
                "session cwd does not match the requested workspace",
            ))
        }
    }

    async fn prompt_inner(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, HostError> {
        self.send_inner(params, false).await
    }

    async fn steer_inner(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, HostError> {
        self.send_inner(params, true).await
    }

    async fn send_inner(
        &self,
        params: SessionPromptParams,
        steer: bool,
    ) -> Result<SessionPromptResult, HostError> {
        let _admission = self.admit()?;
        let params = self.normalize_prompt_inner(params).await?;
        validate_prompt(&params)?;
        let (agent, session, mut commits, message_id, was_running) = {
            let _setup = self.inner.setup.lock().await;
            let agent = self.ensure_agent_under_setup(&params.session_id).await?;
            let session = agent.session();
            let commits = session.subscribe();
            let message_id = MessageId::random();
            let was_running = agent.status() == AgentStatus::Running;
            let message = Message {
                id: message_id.clone(),
                role: MessageRole::User,
                content: params.content_blocks,
                source: MessageSource::User {
                    client_time_zone: params.client_time_zone,
                },
            };
            let target = if steer {
                InboxTarget::Steer
            } else {
                InboxTarget::Followup
            };
            session
                .append_next(
                    |seq| inbox_enqueued_event(seq, target, &message),
                    self.inner.cancellation.clone(),
                )
                .await?;
            self.inner.job_delivery.reset(&params.session_id);
            self.ensure_relay(Arc::clone(&session));
            if steer {
                agent.steer(message).await?;
            } else {
                agent.followup(message).await?;
            }
            self.transition(params.session_id.clone(), SessionStatus::Running);
            let idle_agent = self.inner.registry.get(&params.session_id).ok_or_else(|| {
                HostError::InvalidConfiguration("accepted agent disappeared".into())
            })?;
            self.watch_idle(params.session_id, idle_agent);
            (agent, session, commits, message_id, was_running)
        };
        if was_running {
            return Ok(SessionPromptResult { message_id });
        }
        if !session.events().iter().any(|event| {
            event.event_type == "user/message"
                && event.data.get("id").and_then(Value::as_str) == Some(message_id.as_str())
        }) {
            loop {
                tokio::select! { received = commits.recv() => match received { Ok(event) if event.event_type == "user/message" && event.data.get("id").and_then(Value::as_str) == Some(message_id.as_str()) => break, Ok(_) => continue, Err(broadcast::error::RecvError::Lagged(_)) => { if session.events().iter().any(|event| event.event_type == "user/message" && event.data.get("id").and_then(Value::as_str) == Some(message_id.as_str())) { break; } }, Err(broadcast::error::RecvError::Closed) => return Err(HostError::invalid("PROMPT_NOT_DURABLE", "agent session closed before prompt admission")), }, result = agent.when_idle() => { if session.events().iter().any(|event| event.event_type == "user/message" && event.data.get("id").and_then(Value::as_str) == Some(message_id.as_str())) { break; } return Err(result.err().unwrap_or(AgentError::Disposed).into()); } }
            }
        }
        append_fallback_session_title(&session, &message_id, self.inner.cancellation.clone())
            .await?;
        Ok(SessionPromptResult { message_id })
    }

    async fn update_queue_inner(
        &self,
        params: SessionUpdateQueueParams,
    ) -> Result<SessionUpdateQueueResult, HostError> {
        let _admission = self.admit()?;
        validate_session(&params.session_id)?;
        if params.item_id.as_str().is_empty() {
            return Err(queue_error(
                "queue-item-not-found",
                "queued item is no longer pending",
                &params.item_id,
            ));
        }
        if matches!(&params.action, SessionQueueAction::Edit { content } if content.iter().any(|block| !matches!(block, ContentBlock::Text { .. })))
        {
            return Err(HostError::Runtime(TessivumError::new(
                "attachment-error",
                "queue edits accept text content only",
                "queue",
                json!({"reason": "QUEUE_EDIT_NON_TEXT"}),
            )));
        }
        let _setup = self.inner.setup.lock().await;
        let agent = self.inner.registry.get(&params.session_id).ok_or_else(|| {
            queue_error(
                "queue-item-not-found",
                "queued item is no longer pending",
                &params.item_id,
            )
        })?;
        let session = agent.session();
        if session.id() != params.session_id || session.header().origin.is_some() {
            return Err(HostError::Runtime(TessivumError::new(
                "subagent-ownership",
                "queue updates require the session's ordinary agent",
                "queue",
                json!({"sessionId": params.session_id}),
            )));
        }
        let action_name = match &params.action {
            SessionQueueAction::Edit { .. } => "edit",
            SessionQueueAction::Remove => "remove",
            SessionQueueAction::Steer => "steer",
        };
        let update = match params.action {
            SessionQueueAction::Edit { content } => InboxUpdate::Edit { content },
            SessionQueueAction::Remove => InboxUpdate::Remove,
            SessionQueueAction::Steer => InboxUpdate::Steer,
        };
        let reservation = match agent.reserve_inbox_update(&params.item_id, update).await? {
            InboxReservationResult::Reserved(reservation) => reservation,
            InboxReservationResult::NotPending => {
                return Err(queue_error(
                    "queue-item-not-found",
                    "queued item is no longer pending",
                    &params.item_id,
                ));
            }
            InboxReservationResult::SteerUnavailable => {
                return Err(queue_error(
                    "steer-unavailable",
                    "current turn no longer accepts steering",
                    &params.item_id,
                ));
            }
        };
        let source_target = reservation.source_target();
        let start = reservation.start();
        let destination_start = reservation.destination_start();
        let message = reservation.message().clone();
        let data = if action_name == "steer" {
            json!({
                "target": "next-step",
                "start": destination_start.expect("steer reservation has a destination"),
                "removedCount": 0,
                "inserted": [message.clone()],
                "itemId": params.item_id,
                "action": "steer",
                "message": message,
            })
        } else {
            let inserted = if action_name == "edit" {
                vec![message]
            } else {
                Vec::new()
            };
            json!({
                "target": queue_target_name(source_target),
                "start": start,
                "removedCount": 1,
                "inserted": inserted,
            })
        };
        session
            .append_next(
                move |seq| SessionEvent {
                    event_type: "agent/inbox/spliced".into(),
                    seq,
                    time: now(),
                    data,
                    ignorable: Some(true),
                    source_event_seqs: None,
                    surface_op: None,
                },
                self.inner.cancellation.clone(),
            )
            .await?;
        self.ensure_relay(Arc::clone(&session));
        let _ = agent.commit_inbox_update(reservation).await?;
        self.publish_queue(params.session_id, agent.inbox());
        Ok(SessionUpdateQueueResult { accepted: true })
    }

    async fn release_owned_agent(&self, session: &SessionId) {
        lock(&self.inner.job_owners).remove(session);
        lock(&self.inner.schedule_owners).remove(session);
        lock(&self.inner.skill_scopes).remove(session);
        self.inner.goal_tools.remove(session);
        self.inner.planning_tools.remove(session);
        let owned = { lock(&self.inner.owned_agents).remove(session) };
        if let Some(owned) = owned {
            owned.jobs.dispose().await;
            owned.schedule.dispose();
        }
    }

    async fn cancel_inner(
        &self,
        session: SessionId,
        cause: AgentCancelCause,
    ) -> Result<bool, HostError> {
        self.inner.approvals.cancel_session(&session);
        self.inner.questions.cancel_session(&session);
        let agent = self.inner.registry.get(&session);
        let cancelled = match self.inner.registry.cancel(&session, cause, true) {
            Ok(value) => value,
            Err(AgentError::NotFound(_)) => false,
            Err(error) => return Err(error.into()),
        };
        if cancelled {
            if let Some(agent) = &agent {
                self.publish_queue(session.clone(), agent.inbox());
            }
            lock(&self.inner.state).queue_relayed.remove(&session);
            self.release_owned_agent(&session).await;
            if let Some(agent) = agent {
                agent.dispose().await?;
            }
            self.transition(session, SessionStatus::Idle);
        }
        Ok(cancelled)
    }

    async fn read_raw_session_inner(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionRawArtifact>, HostError> {
        validate_session(&session)?;
        if !self.inner.persistence.supports_raw_artifacts() {
            return Err(HostError::invalid(
                "SESSION_RAW_ARTIFACTS_UNSUPPORTED",
                "the persistence backend does not expose per-session raw artifacts",
            ));
        }
        if let Some(live) = self.inner.sessions.get(&session) {
            live.flush(self.inner.cancellation.clone()).await?;
        }
        Ok(self
            .inner
            .persistence
            .read_raw(&session, self.inner.cancellation.clone())
            .await?)
    }

    async fn events_inner(
        &self,
        session: SessionId,
        from_seq: u64,
    ) -> Result<Vec<SessionEvent>, HostError> {
        validate_session(&session)?;
        if let Some(live) = self.inner.sessions.get(&session) {
            return Ok(live
                .events()
                .into_iter()
                .filter(|event| event.seq >= from_seq)
                .collect());
        }
        Ok(self
            .inner
            .persistence
            .read_from(&session, from_seq, self.inner.cancellation.clone())
            .await?)
    }

    async fn status_inner(&self, session: SessionId) -> Result<Option<SessionStatus>, HostError> {
        validate_session(&session)?;
        if let Some(status) = lock(&self.inner.state).statuses.get(&session).copied() {
            return Ok(Some(status));
        }
        if let Some(agent) = self.inner.registry.get(&session) {
            return Ok(Some(match agent.status() {
                AgentStatus::Idle => SessionStatus::Idle,
                AgentStatus::Running => SessionStatus::Running,
            }));
        }
        Ok(self
            .inner
            .persistence
            .inspect(&session, self.inner.cancellation.clone())
            .await?
            .map(|_| SessionStatus::Idle))
    }
    fn session_running(&self, session: &SessionId) -> bool {
        lock(&self.inner.state).statuses.get(session) == Some(&SessionStatus::Running)
            || self
                .inner
                .registry
                .get(session)
                .is_some_and(|agent| agent.status() == AgentStatus::Running)
    }

    async fn subagent_list_inner(
        &self,
        parent_session_id: SessionId,
    ) -> Result<SubagentCatalog, HostError> {
        validate_session(&parent_session_id)?;
        let entries = self
            .inner
            .subagents
            .list(parent_session_id.clone(), self.inner.cancellation.clone())
            .await
            .map_err(host_subagent_error)?;
        let parent_available = self
            .inner
            .registry
            .get(&parent_session_id)
            .is_some_and(|agent| !agent.is_disposed());
        Ok(SubagentCatalog {
            entries,
            parent_available,
        })
    }

    async fn subagent_history_inner(
        &self,
        params: SubagentHistoryRequest,
    ) -> Result<SubagentHistoryResult, HostError> {
        self.inner
            .subagents
            .history(params, self.inner.cancellation.clone())
            .await
            .map_err(host_subagent_error)
    }

    async fn subagent_prompt_inner(
        &self,
        params: SubagentPromptRequest,
    ) -> Result<SubagentPromptResult, HostError> {
        let _admission = self.admit()?;
        let child_session_id = params.child_session_id.clone();
        if let Some(agent) = self.inner.registry.get(&child_session_id) {
            self.ensure_relay(agent.session());
            self.ensure_queue_relay(
                child_session_id.clone(),
                agent.inbox(),
                agent.cancellation(),
            );
        }
        let result = self
            .inner
            .subagents
            .prompt(params, self.inner.cancellation.clone())
            .await
            .map_err(host_subagent_error)?;
        if let Some(agent) = self.inner.registry.get(&child_session_id) {
            self.ensure_relay(agent.session());
            self.ensure_queue_relay(
                child_session_id.clone(),
                agent.inbox(),
                agent.cancellation(),
            );
            self.publish_queue(child_session_id.clone(), agent.inbox());
            self.transition(child_session_id.clone(), SessionStatus::Running);
            self.watch_idle(child_session_id, agent);
        }
        Ok(result)
    }

    async fn subagent_interrupt_inner(
        &self,
        params: SubagentInterruptRequest,
    ) -> Result<SubagentInterruptResult, HostError> {
        let _admission = self.admit()?;
        self.inner
            .subagents
            .interrupt(params, self.inner.cancellation.clone())
            .await
            .map_err(host_subagent_error)
    }
    async fn subagent_delete_inner(
        &self,
        params: SubagentDeleteRequest,
    ) -> Result<SubagentDeleteResult, HostError> {
        let _admission = self.admit()?;
        let _setup = self.inner.setup.lock().await;
        let child_session_id = params.child_session_id.clone();
        let result = self
            .inner
            .subagents
            .delete(params, self.inner.cancellation.clone())
            .await
            .map_err(host_subagent_error)?;
        lock(&self.inner.state).statuses.remove(&child_session_id);
        lock(&self.inner.state)
            .queue_relayed
            .remove(&child_session_id);
        Ok(result)
    }

    async fn shutdown_inner(&self) -> Result<(), HostError> {
        let _shutdown = self.inner.shutdown.lock().await;
        let already_closing = {
            let mut state = lock(&self.inner.admission);
            let already = state.closing;
            if !already {
                state.closing = true;
                if state.count == 0 {
                    self.inner.drained.notify_waiters();
                }
            }
            already
        };
        if already_closing {
            self.wait_drained().await;
            return Ok(());
        }
        self.inner.settings.shutdown().await;
        self.inner.credentials.shutdown().await;
        self.inner.workspace_registry.close();
        self.inner.approvals.cancel_all();
        self.inner.questions.cancel_all();
        self.inner.registry.cancel_all(
            AgentCancelCause::Hook {
                reason: "host shutdown".into(),
            },
            false,
        );
        self.wait_drained().await;
        let mut failures = Vec::new();
        if let Err(error) = self.inner.registry.dispose_all().await {
            failures.push(format!("agents: {error}"));
        }
        let owned_agents = std::mem::take(&mut *lock(&self.inner.owned_agents));
        lock(&self.inner.job_owners).clear();
        lock(&self.inner.schedule_owners).clear();
        lock(&self.inner.skill_scopes).clear();
        for owned in owned_agents.into_values() {
            owned.jobs.dispose().await;
            owned.schedule.dispose();
        }
        self.inner.workspace_registry.shutdown();
        if let Some(code) = &self.inner.code_runtime {
            if let Err(error) = code.dispose().await {
                failures.push(format!("code: {error}"));
            }
        }
        self.inner.subprocesses.shutdown().await;
        for session in self.inner.sessions.list() {
            if let Err(error) = session.flush(self.inner.cancellation.clone()).await {
                failures.push(format!("session {}: {error}", session.id()));
            }
        }
        if let Err(error) = self.sweep_orphaned_attachments().await {
            failures.push(format!("attachments: {error}"));
        }
        self.stop_relays().await;
        self.inner.projections.shutdown().await;
        if let Some(telemetry) = &self.inner.telemetry {
            telemetry
                .shutdown(
                    self.inner
                        .sessions
                        .list()
                        .into_iter()
                        .map(|session| session.id()),
                )
                .await;
        }
        let loader = { self.inner.loader.lock().await.take() };
        if let Some(mut loader) = loader {
            let unloaded = tokio::task::spawn_blocking(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime.block_on(async move {
                    loader
                        .replace(EntryTree::default())
                        .await
                        .map_err(|error| error.to_string())
                })
            })
            .await;
            match unloaded {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("loader: {error}")),
                Err(error) => failures.push(format!("loader task: {error}")),
            }
        }
        if let Some(legacy) = &self.inner.legacy {
            if let Err(error) = legacy.shutdown().await {
                failures.push(format!("legacy: {error}"));
            }
        }
        self.inner.cancellation.cancel();
        if let Err(error) = self.inner.services.root.scope().dispose().await {
            failures.push(format!("root: {error}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(HostError::Shutdown(failures.join("; ")))
        }
    }

    fn admit(&self) -> Result<Admission, HostError> {
        let mut state = lock(&self.inner.admission);
        if state.closing {
            return Err(HostError::ShuttingDown);
        }
        state.count = state
            .count
            .checked_add(1)
            .ok_or_else(|| HostError::InvalidConfiguration("admission count overflow".into()))?;
        Ok(Admission(Arc::clone(&self.inner)))
    }

    async fn wait_drained(&self) {
        loop {
            let notified = self.inner.drained.notified();
            if lock(&self.inner.admission).count == 0 {
                return;
            }
            notified.await;
        }
    }

    async fn goal_service_inner(&self, session_id: SessionId) -> Result<GoalService, HostError> {
        let _admission = self.admit()?;
        validate_session(&session_id)?;
        let _setup = self.inner.setup.lock().await;
        if self.inner.sessions.get(&session_id).is_none()
            && self
                .inner
                .persistence
                .inspect(&session_id, self.inner.cancellation.clone())
                .await?
                .is_none()
        {
            return Err(SessionError::NotFound(session_id).into());
        }
        self.ensure_agent_under_setup(&session_id).await?;
        lock(&self.inner.owned_agents)
            .get(&session_id)
            .map(|owned| owned.goals.clone())
            .ok_or_else(|| {
                HostError::invalid(
                    "GOAL_SERVICE_ABSENT",
                    "goal service is absent for the session's live agent",
                )
            })
    }

    async fn planning_service_inner(
        &self,
        session_id: SessionId,
    ) -> Result<PlanningService, HostError> {
        let _admission = self.admit()?;
        validate_session(&session_id)?;
        let _setup = self.inner.setup.lock().await;
        self.ensure_agent_under_setup(&session_id).await?;
        lock(&self.inner.owned_agents)
            .get(&session_id)
            .map(|owned| owned.planning.clone())
            .ok_or_else(|| {
                HostError::invalid(
                    "PLANNING_SERVICE_ABSENT",
                    "planning service is absent for the session's live agent",
                )
            })
    }
    async fn command_list_inner(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<HostCommandDescriptor>, HostError> {
        let _admission = self.admit()?;
        validate_session(&session_id)?;
        let _setup = self.inner.setup.lock().await;
        self.ensure_agent_under_setup(&session_id).await?;
        let session = self
            .inner
            .sessions
            .get(&session_id)
            .ok_or_else(|| SessionError::NotFound(session_id.clone()))?;
        let preset = selected_agent_preset(&session).unwrap_or_else(|| "standard".into());
        let mut commands = vec![
            feedback_command_descriptor(),
            goal_command_descriptor(),
            permission_command_descriptor(),
            simple_command_descriptor("export", "Download this Session log as a ZIP archive"),
        ];
        if self
            .inner
            .agent_presets
            .contains_plugin(&preset, "@deepseek-ai/dsh-command-compact")
            .await
            .unwrap_or(false)
        {
            commands.push(simple_command_descriptor(
                "compact",
                "Compact older conversation history",
            ));
        }
        if self
            .inner
            .agent_presets
            .contains_plugin(&preset, "@deepseek-ai/dsh-plan-mode")
            .await
            .unwrap_or(false)
        {
            commands.push(plan_command_descriptor());
        }
        commands.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(commands)
    }

    async fn command_execute_inner(
        &self,
        session_id: SessionId,
        line: String,
    ) -> Result<Option<HostCommandExecution>, HostError> {
        let _admission = self.admit()?;
        validate_session(&session_id)?;
        let Some((name, raw_input)) = parse_command(&line) else {
            return Ok(None);
        };
        if !matches!(name, "export" | "feedback" | "goal" | "permission" | "plan") {
            return Ok(None);
        }
        let _commands = self.inner.commands.lock().await;
        let session = {
            let _setup = self.inner.setup.lock().await;
            self.ensure_agent_under_setup(&session_id).await?.session()
        };
        let approvals = if name == "permission" {
            Some(self.inner.approvals.lookup(&session_id).ok_or_else(|| {
                HostError::invalid(
                    "APPROVAL_SERVICE_ABSENT",
                    "approval service is absent for the session's live agent",
                )
            })?)
        } else {
            None
        };
        let command_id = format!("cmd-{}", Uuid::new_v4());
        let mut command_run = json!({
            "commandId": command_id,
            "name": name,
            "source": {"kind": "user"},
        });
        if name != "feedback" {
            command_run["args"] = json!(raw_input);
        }
        self.append_command_event(&session, "command/run", command_run)
            .await?;

        let result = match name {
            "feedback" => self.run_feedback_command(&session, raw_input).await,
            "export" => Ok(if raw_input.trim().is_empty() {
                HostCommandResult::Success {
                    text: Some("Session log download requested.".into()),
                    source_event_seq: None,
                }
            } else {
                HostCommandResult::Error {
                    text: "The Web /export command does not accept a path.".into(),
                }
            }),
            "goal" => self.run_goal_command(&session_id, raw_input).await,
            "plan" => self.run_plan_command(&session_id, raw_input).await,
            "permission" => {
                self.run_permission_command(
                    &session,
                    &approvals.expect("permission command requires approval service"),
                    raw_input,
                )
                .await
            }
            _ => unreachable!(),
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                let _ = self
                    .append_command_event(
                        &session,
                        "command/done",
                        json!({"commandId": command_id, "kind": "error", "text": error.to_string()}),
                    )
                    .await;
                return Err(error);
            }
        };
        let goal_input = raw_input.trim();
        let plan_prompt = (name == "plan"
            && matches!(&result, HostCommandResult::Success { .. })
            && !raw_input.trim().is_empty()
            && raw_input.trim() != "off")
            .then(|| raw_input.trim().to_owned());
        let drive_goal = name == "goal"
            && matches!(&result, HostCommandResult::Success { .. })
            && !matches!(goal_input, "" | "pause" | "clear" | "edit")
            && !goal_input.starts_with("edit ");
        let mut done = serde_json::to_value(&result).expect("command result is serializable");
        done.as_object_mut()
            .expect("command result serializes as an object")
            .insert("commandId".into(), Value::String(command_id.clone()));
        self.append_command_event(&session, "command/done", done)
            .await?;
        if let Some(text) = plan_prompt {
            self.prompt_inner(SessionPromptParams {
                session_id: session_id.clone(),
                content_blocks: vec![ContentBlock::Text { text }],
                client_time_zone: None,
            })
            .await?;
        }
        if drive_goal {
            let goals = self.goal_service_inner(session_id.clone()).await?;
            tokio::spawn(async move {
                let _ = goals.drive().await;
            });
        }
        Ok(Some(HostCommandExecution { command_id, result }))
    }
    async fn run_feedback_command(
        &self,
        session: &Session,
        raw_input: &str,
    ) -> Result<HostCommandResult, HostError> {
        let text = raw_input.trim();
        if text.is_empty() {
            return Ok(HostCommandResult::Error {
                text: "Feedback text is required. Usage: /feedback <text>".into(),
            });
        }
        let source_event_seq = self
            .append_command_event(session, "feedback/record", json!({"text": text}))
            .await?;
        let user_id = self.inner.message_feedback.anonymous_user_id().await?;
        let disclosure = match self.inner.telemetry.as_ref().map(TelemetryCoordinator::sharing) {
            None => "Session sharing is not configured.",
            Some(crate::telemetry::TelemetrySharing::Full) => "Session sharing is enabled.",
            Some(crate::telemetry::TelemetrySharing::FeedbackOnly) => {
                "Session sharing is feedback-gated; recording feedback releases the session prefix for sharing."
            }
            Some(crate::telemetry::TelemetrySharing::Disabled) => "Session sharing is disabled.",
        };
        Ok(HostCommandResult::Success {
            text: Some(format!(
                "Feedback recorded for session {}\nAnonymous user: {user_id}. {disclosure}",
                session.id()
            )),
            source_event_seq: Some(source_event_seq),
        })
    }

    async fn message_feedback_header(
        &self,
        session_id: &SessionId,
        flush: bool,
    ) -> Result<Option<SessionHeader>, HostError> {
        if let Some(session) = self.inner.sessions.get(session_id) {
            if flush {
                session.flush(self.inner.cancellation.clone()).await?;
            }
            return Ok(Some(session.header()));
        }
        Ok(self
            .inner
            .persistence
            .inspect(session_id, self.inner.cancellation.clone())
            .await?
            .map(|inspection| inspection.header))
    }

    async fn message_feedback_list_inner(&self, session_id: SessionId) -> Result<Value, HostError> {
        let Some(header) = self.message_feedback_header(&session_id, false).await? else {
            return Ok(
                json!({"ok": false, "error": {"code": "session-not-found", "sessionId": session_id}}),
            );
        };
        let items = self.inner.message_feedback.list(&session_id, &header).await;
        Ok(json!({"ok": true, "value": {"items": items}}))
    }

    async fn message_feedback_put_inner(
        &self,
        session_id: SessionId,
        message_id: String,
        rating: String,
        note: Option<String>,
        if_version: Option<String>,
    ) -> Result<Value, HostError> {
        if !matches!(rating.as_str(), "positive" | "negative") {
            return Err(HostError::invalid(
                "INVALID_MESSAGE_FEEDBACK_RATING",
                "feedback rating must be positive or negative",
            ));
        }
        if note.as_deref().is_some_and(|value| value.trim().is_empty()) {
            return Ok(json!({"ok": false, "error": {"code": "note-blank"}}));
        }
        if let Some(note) = &note {
            if note.len() > MAX_MESSAGE_FEEDBACK_NOTE_BYTES {
                return Ok(json!({"ok": false, "error": {
                    "code": "note-too-large",
                    "maxBytes": MAX_MESSAGE_FEEDBACK_NOTE_BYTES,
                    "actualBytes": note.len(),
                }}));
            }
        }
        let Some(header) = self.message_feedback_header(&session_id, true).await? else {
            return Ok(
                json!({"ok": false, "error": {"code": "session-not-found", "sessionId": session_id}}),
            );
        };
        let events = self
            .inner
            .persistence
            .read_from(&session_id, 0, self.inner.cancellation.clone())
            .await?;
        let target_exists = events.iter().any(|event| {
            event.event_type == "assistant/message"
                && event.surface_op.as_ref() == Some(&SurfaceOp::Append)
                && event
                    .data
                    .get("message")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<Message>(value).ok())
                    .is_some_and(|message| {
                        message.role == MessageRole::Assistant
                            && !message.content.is_empty()
                            && message.id.as_str() == message_id
                    })
        });
        if !target_exists {
            return Ok(json!({"ok": false, "error": {
                "code": "target-not-found",
                "sessionId": session_id,
                "messageId": message_id,
            }}));
        }
        match self
            .inner
            .message_feedback
            .put(session_id, &header, message_id, rating, note, if_version)
            .await?
        {
            MessageFeedbackPut::Item(item) => Ok(json!({"ok": true, "value": item})),
            MessageFeedbackPut::Conflict(current) => {
                Ok(json!({"ok": false, "error": {"code": "version-conflict", "current": current}}))
            }
        }
    }

    async fn message_feedback_delete_inner(
        &self,
        session_id: SessionId,
        message_id: String,
        if_version: String,
    ) -> Result<Value, HostError> {
        let Some(header) = self.message_feedback_header(&session_id, false).await? else {
            return Ok(
                json!({"ok": false, "error": {"code": "session-not-found", "sessionId": session_id}}),
            );
        };
        match self
            .inner
            .message_feedback
            .delete(session_id, &header, &message_id, &if_version)
            .await?
        {
            MessageFeedbackDelete::Absent => Ok(json!({"ok": true, "value": {"absent": true}})),
            MessageFeedbackDelete::Conflict(current) => {
                Ok(json!({"ok": false, "error": {"code": "version-conflict", "current": current}}))
            }
        }
    }

    async fn run_goal_command(
        &self,
        session_id: &SessionId,
        raw_input: &str,
    ) -> Result<HostCommandResult, HostError> {
        let goals = self.goal_service_inner(session_id.clone()).await?;
        let input = raw_input.trim();
        let current = goals.current().await;
        let success = |text| HostCommandResult::Success {
            text: Some(text),
            source_event_seq: None,
        };
        let missing = || HostCommandResult::Error {
            text: "No goal is set. Usage: /goal <objective>".into(),
        };
        match input {
            "" => Ok(success(current.map_or_else(
                || "No goal is currently set.\nUsage: /goal [<objective>|clear|edit <objective>|pause|resume]".into(),
                |goal| format!("Current goal: {}", goal.title),
            ))),
            "pause" => {
                let Some(goal) = current else {
                    return Ok(missing());
                };
                goals
                    .pause(goal.reference, self.inner.cancellation.clone())
                    .await?;
                Ok(success("Goal paused.".into()))
            }
            "resume" => {
                let Some(goal) = current else {
                    return Ok(missing());
                };
                let goal = goals
                    .resume(goal.reference, self.inner.cancellation.clone())
                    .await?;
                Ok(success(format!("Goal resumed: {}", goal.title)))
            }
            "clear" => {
                let Some(goal) = current else {
                    return Ok(missing());
                };
                goals
                    .clear(goal.reference, self.inner.cancellation.clone())
                    .await?;
                Ok(success("Goal cleared.".into()))
            }
            "edit" => Ok(HostCommandResult::Error {
                text: "Usage: /goal edit <objective>".into(),
            }),
            _ if input.starts_with("edit ") => {
                let Some(goal) = current else {
                    return Ok(missing());
                };
                let objective = input[5..].trim();
                if objective.is_empty() {
                    return Ok(HostCommandResult::Error {
                        text: "Usage: /goal edit <objective>".into(),
                    });
                }
                let goal = goals
                    .edit(
                        goal.reference,
                        Some(objective.to_owned()),
                        None,
                        self.inner.cancellation.clone(),
                    )
                    .await?;
                Ok(success(format!("Goal updated: {}", goal.title)))
            }
            objective => {
                if current.is_some() {
                    return Ok(HostCommandResult::Error {
                        text: "A goal is already set. Use /goal edit, /goal pause, /goal resume, or /goal clear.".into(),
                    });
                }
                let goal = goals
                    .create(
                        objective.to_owned(),
                        Some(256),
                        self.inner.cancellation.clone(),
                    )
                    .await?;
                Ok(success(format!(
                    "Goal created\nStatus: active\nObjective: {}\nRounds: 0/{}\nActivation: armed\n\nCommands: /goal edit <objective>, /goal pause, /goal clear",
                    goal.title, goal.max_goal_rounds,
                )))
            }
        }
    }

    async fn run_plan_command(
        &self,
        session_id: &SessionId,
        raw_input: &str,
    ) -> Result<HostCommandResult, HostError> {
        let planning = self.planning_service_inner(session_id.clone()).await?;
        let target = raw_input.trim() != "off";
        if planning.mode().await
            == (if target {
                PlanMode::Plan
            } else {
                PlanMode::Normal
            })
        {
            return Ok(HostCommandResult::Success {
                text: Some(if target {
                    "Plan mode is already active.".into()
                } else {
                    "Plan mode is already inactive.".into()
                }),
                source_event_seq: None,
            });
        }
        planning
            .set_mode(
                if target {
                    PlanMode::Plan
                } else {
                    PlanMode::Normal
                },
                self.inner.cancellation.clone(),
            )
            .await?;
        Ok(HostCommandResult::Success {
            text: Some(if target {
                "Plan mode on. Use /plan off to leave.".into()
            } else {
                "Plan mode off.".into()
            }),
            source_event_seq: None,
        })
    }

    async fn run_permission_command(
        &self,
        session: &Session,
        approvals: &ApprovalService,
        raw_input: &str,
    ) -> Result<HostCommandResult, HostError> {
        let requested = raw_input.trim();
        let mut knobs = permission_knobs(&session.events());
        let available = permission_preset_names().collect::<Vec<_>>().join(", ");
        if requested.is_empty() {
            return Ok(HostCommandResult::Success {
                text: Some(format!(
                    "current preset {} (available: {available})",
                    current_permission(&knobs)
                )),
                source_event_seq: None,
            });
        }
        let Some(spec) = permission_preset(requested) else {
            return Ok(HostCommandResult::Error {
                text: format!("unknown preset \"{requested}\" (available: {available})"),
            });
        };
        if current_permission(&knobs) != requested {
            self.append_command_event(session, "permission/preset", json!({"preset": requested}))
                .await?;
            knobs.select_preset(requested);
        }
        if knobs.sandbox() != Some(spec.sandbox) {
            self.append_command_event(session, "sandbox/mode", json!({"mode": spec.sandbox}))
                .await?;
        }
        if knobs.approval() != Some(spec.approval) {
            approvals
                .set_policy(spec.approval, self.inner.cancellation.clone())
                .await?;
        }
        Ok(HostCommandResult::Success {
            text: Some(format!("preset {requested}")),
            source_event_seq: None,
        })
    }

    async fn append_command_event(
        &self,
        session: &Session,
        event_type: &'static str,
        data: Value,
    ) -> Result<u64, HostError> {
        Ok(session
            .append_next(
                move |seq| SessionEvent {
                    event_type: event_type.into(),
                    seq,
                    time: now(),
                    data,
                    ignorable: None,
                    source_event_seqs: None,
                    surface_op: None,
                },
                self.inner.cancellation.clone(),
            )
            .await?)
    }

    async fn ensure_agent_under_setup(
        &self,
        session_id: &SessionId,
    ) -> Result<AgentHandle, HostError> {
        let header = match self.inner.sessions.get(session_id) {
            Some(session) => session.header(),
            None => match self
                .inner
                .persistence
                .inspect(session_id, self.inner.cancellation.clone())
                .await?
            {
                Some(session) => session.header,
                None => {
                    self.create_session_in_unadmitted(
                        session_id.clone(),
                        self.default_workspace_id()?,
                    )
                    .await?;
                    self.inner
                        .sessions
                        .get(session_id)
                        .ok_or_else(|| {
                            HostError::InvalidConfiguration("new session was not published".into())
                        })?
                        .header()
                }
            },
        };
        if self
            .inner
            .workspace_registry
            .workspace_for_session(session_id)
            .is_none()
        {
            return Err(HostError::SessionUngrouped {
                session_id: session_id.clone(),
            });
        }
        let lease = self.inner.resources.resolve(session_id)?;
        let root = lease.validate_current()?;
        self.require_session_root(&header, &root)?;
        if let Some(agent) = self.inner.registry.get(session_id) {
            return Ok(agent);
        }
        let selected_preset = self
            .events_inner(session_id.clone(), 0)
            .await?
            .into_iter()
            .rev()
            .find(|event| event.event_type == "agent-preset/selected")
            .and_then(|event| {
                event
                    .data
                    .get("agentPreset")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            });
        let agent_preset = selected_preset
            .clone()
            .or_else(|| header.agent_preset.clone());
        if let Some(preset) = &agent_preset {
            let roster_present = !self
                .inner
                .agent_presets
                .list()
                .await
                .map_err(preset_error)?
                .is_empty();
            if selected_preset.is_some() || roster_present {
                self.inner
                    .agent_presets
                    .prepare_selection(preset)
                    .await
                    .map_err(preset_error)?;
            }
        }
        let selection = self.selection_for_session(session_id).await?;
        let max_tokens = self.inner.config.max_tokens.or_else(|| {
            let state = lock(&self.inner.route_state);
            state
                .routes
                .get(&selection.provider)
                .and_then(|route| {
                    route
                        .models
                        .iter()
                        .find(|model| model.id == selection.model)
                })
                .and_then(|model| model.max_tokens)
        });
        if lock(&self.inner.owned_agents).len() >= self.inner.config.max_live_sessions {
            return Err(HostError::SessionCapacity);
        }
        self.enable_session_skills(session_id, &root, agent_preset.as_deref())
            .await?;
        let owned = match self
            .inner
            .registry
            .create_or_resume(
                SessionHeader {
                    version: SESSION_FORMAT_VERSION,
                    id: session_id.clone(),
                    created_at: header.created_at,
                    cwd: Some(root.to_string_lossy().into_owned()),
                    parent_session: None,
                    seed_length: None,
                    origin: None,
                    delegation_depth: Some(0),
                    agent_preset: agent_preset.clone(),
                },
                AgentOptions {
                    provider: selection.provider,
                    model: selection.model,
                    reasoning_effort: selection.reasoning_effort,
                    max_tokens,
                },
                self.inner.cancellation.clone(),
            )
            .await
        {
            Ok(owned) => owned,
            Err(error) => {
                lock(&self.inner.skill_scopes).remove(session_id);
                return Err(error.into());
            }
        };
        let observer = match self.inner.registry.get(session_id) {
            Some(agent) => agent,
            None => {
                let _ = owned.dispose().await;
                lock(&self.inner.skill_scopes).remove(session_id);
                return Err(HostError::InvalidConfiguration(
                    "created agent was not published".into(),
                ));
            }
        };
        let session = observer.session();
        if let Err(error) = self.pin_initial_permission(&session).await {
            let _ = owned.dispose().await;
            return Err(error);
        }
        let authority = owned.authority();
        let inbox = observer.inbox();
        let queue_cancellation = observer.cancellation();
        let approvals = match ApprovalService::new(observer) {
            Ok(approvals) => approvals,
            Err(error) => {
                let _ = owned.dispose().await;
                lock(&self.inner.skill_scopes).remove(session_id);
                return Err(error.into());
            }
        };
        let approval = match self.inner.approvals.install(&authority, approvals) {
            Ok(approval) => approval,
            Err(error) => {
                let _ = owned.dispose().await;
                lock(&self.inner.skill_scopes).remove(session_id);
                return Err(error.into());
            }
        };
        let question = match self.inner.questions.install(&authority, owned.session()) {
            Ok(question) => question,
            Err(error) => {
                let _ = owned.dispose().await;
                return Err(HostError::InvalidConfiguration(error.to_string()));
            }
        };
        let goals = match self.inner.registry.get(session_id) {
            Some(agent) => match GoalService::new(agent) {
                Ok(goals) => goals,
                Err(error) => {
                    let _ = owned.dispose().await;
                    lock(&self.inner.skill_scopes).remove(session_id);
                    return Err(error.into());
                }
            },
            None => {
                let _ = owned.dispose().await;
                lock(&self.inner.skill_scopes).remove(session_id);
                return Err(HostError::InvalidConfiguration(
                    "created agent was not published".into(),
                ));
            }
        };
        let planning = match self.inner.registry.get(session_id) {
            Some(agent) => match PlanningService::new(agent) {
                Ok(planning) => planning,
                Err(error) => {
                    let _ = owned.dispose().await;
                    return Err(HostError::invalid(
                        "PLANNING_SERVICE_ABSENT",
                        error.to_string(),
                    ));
                }
            },
            None => {
                let _ = owned.dispose().await;
                return Err(HostError::InvalidConfiguration(
                    "created agent was not published".into(),
                ));
            }
        };
        let jobs = match self
            .inner
            .jobs
            .attach_owner(&authority, &self.inner.services.root)
        {
            Ok(jobs) => jobs,
            Err(error) => {
                let _ = owned.dispose().await;
                lock(&self.inner.skill_scopes).remove(session_id);
                return Err(HostError::InvalidConfiguration(error.to_string()));
            }
        };
        lock(&self.inner.job_owners).insert(session_id.clone(), jobs.clone());
        let session = owned.session();
        let schedule =
            ScheduleOwner::new(authority, Arc::clone(&session), self.inner.registry.clone());
        lock(&self.inner.schedule_owners).insert(session_id.clone(), schedule.clone());
        let goal_driver = goals.clone();
        lock(&self.inner.owned_agents).insert(
            session_id.clone(),
            OwnedAgent {
                _approval: approval,
                _question: question,
                _agent: owned,
                goals,
                planning: planning.clone(),
                jobs,
                schedule: schedule.clone(),
            },
        );
        self.inner.planning_tools.insert(planning);
        self.inner.goal_tools.insert(goal_driver.clone());
        self.ensure_relay(session);
        self.ensure_queue_relay(session_id.clone(), inbox, queue_cancellation);
        schedule.start();
        self.transition(session_id.clone(), SessionStatus::Idle);
        goal_driver.drive().await?;
        self.inner.registry.get(session_id).ok_or_else(|| {
            HostError::InvalidConfiguration("created agent was not published".into())
        })
    }

    async fn enable_session_skills(
        &self,
        session_id: &SessionId,
        root: &Path,
        selected_preset: Option<&str>,
    ) -> Result<(), HostError> {
        if let Some(preset) = selected_preset {
            for plugin in [
                "@deepseek-ai/dsh-skill-filesystem",
                "@deepseek-ai/dsh-tool-skill",
            ] {
                match self
                    .inner
                    .agent_presets
                    .contains_plugin(preset, plugin)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => return Ok(()),
                    Err(error) => return Err(preset_error(error)),
                }
            }
        }
        let skill_root = root.join(".agents").join("skills");
        if skill_root.is_dir() {
            let mut providers = lock(&self.inner.skill_providers);
            if !providers.contains_key(root) {
                let provider =
                    FilesystemSkillProvider::from_root(skill_root).map_err(HostError::Runtime)?;
                let registration = self
                    .inner
                    .skills
                    .register(
                        format!("filesystem:{}", root.display()),
                        Arc::new(provider),
                        root,
                        0,
                    )
                    .map_err(HostError::Runtime)?;
                providers.insert(root.to_path_buf(), registration);
            }
        }
        lock(&self.inner.skill_scopes).insert(session_id.clone(), root.to_path_buf());
        Ok(())
    }

    fn start_service_relays(inner: &Arc<HostInner>) {
        let mut settings_events = inner.settings.subscribe();
        let settings_inner = Arc::clone(inner);
        let settings_relay = tokio::spawn(async move {
            loop {
                if settings_inner.relays_closed.load(Ordering::Acquire) {
                    while let Ok(event) = settings_events.try_recv() {
                        relay_settings_notification(&settings_inner, event);
                    }
                    break;
                }
                tokio::select! {
                    event = settings_events.recv() => match event {
                        Ok(event) => relay_settings_notification(&settings_inner, event),
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = settings_inner.relay_stop.notified() => {
                        while let Ok(event) = settings_events.try_recv() {
                            relay_settings_notification(&settings_inner, event);
                        }
                        break;
                    },
                }
            }
        });
        let mut credential_events = inner.credentials.subscribe();
        let credentials_inner = Arc::clone(inner);
        let credentials_relay = tokio::spawn(async move {
            loop {
                if credentials_inner.relays_closed.load(Ordering::Acquire) {
                    while let Ok(event) = credential_events.try_recv() {
                        let _ = credentials_inner
                            .notices
                            .send(HostNotification::CredentialsChanged(event));
                    }
                    break;
                }
                tokio::select! {
                    event = credential_events.recv() => match event {
                        Ok(event) => { let _ = credentials_inner.notices.send(HostNotification::CredentialsChanged(event)); }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = credentials_inner.relay_stop.notified() => {
                        while let Ok(event) = credential_events.try_recv() {
                            let _ = credentials_inner.notices.send(HostNotification::CredentialsChanged(event));
                        }
                        break;
                    },
                }
            }
        });
        let mut lifecycle_events = inner.notices.subscribe();
        let lifecycle_inner = Arc::clone(inner);
        let lifecycle_relay = tokio::spawn(async move {
            loop {
                if lifecycle_inner.relays_closed.load(Ordering::Acquire) {
                    break;
                }
                tokio::select! {
                    notification = lifecycle_events.recv() => match notification {
                        Ok(HostNotification::SubagentStarted(notification)) => {
                            if let Some(agent) = lifecycle_inner.registry.get(&notification.child_session_id) {
                                let host = HostHandle { inner: Arc::clone(&lifecycle_inner) };
                                host.ensure_relay(agent.session());
                                host.ensure_queue_relay(
                                    notification.child_session_id.clone(),
                                    agent.inbox(),
                                    agent.cancellation(),
                                );
                                host.watch_idle(notification.child_session_id, agent);
                            }
                        }
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = lifecycle_inner.relay_stop.notified() => break,
                }
            }
        });
        lock(&inner.relays).extend([settings_relay, credentials_relay, lifecycle_relay]);
    }

    fn ensure_relay(&self, session: Arc<crate::session::Session>) {
        let session_id = session.id();
        if !lock(&self.inner.state).relayed.insert(session_id.clone()) {
            return;
        }
        self.inner
            .projections
            .attach(Arc::clone(&session))
            .expect("registered host projections must attach before session relay");
        let inner = Arc::clone(&self.inner);
        let starting_next_seq = session.next_seq().unwrap_or(u64::MAX);
        let mut receiver = session.subscribe();
        let task = tokio::spawn(async move {
            let mut next_seq = starting_next_seq;
            relay_missing_events(&inner, &session, &session_id, &mut next_seq);
            loop {
                if inner.relays_closed.load(Ordering::Acquire) {
                    break;
                }
                tokio::select! {
                    event = receiver.recv() => match event {
                        Ok(event) => {
                            if event.seq > next_seq {
                                relay_missing_events(&inner, &session, &session_id, &mut next_seq);
                            }
                            if event.seq >= next_seq {
                                next_seq = event.seq.saturating_add(1);
                                relay_event(&inner, &session_id, event);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            relay_missing_events(&inner, &session, &session_id, &mut next_seq);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = inner.relay_stop.notified() => break,
                }
            }
            relay_missing_events(&inner, &session, &session_id, &mut next_seq);
        });
        lock(&self.inner.relays).push(task);
    }
    fn ensure_queue_relay(
        &self,
        session_id: SessionId,
        inbox: crate::agent::Inbox,
        cancellation: tessivum_core::CancellationToken,
    ) {
        if !lock(&self.inner.state)
            .queue_relayed
            .insert(session_id.clone())
        {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let task = tokio::spawn(async move {
            let mut revision = inbox.change_revision();
            publish_queue(&inner, session_id.clone(), &inbox);
            loop {
                tokio::select! {
                    _ = inbox.wait_for_change(revision) => {
                        revision = inbox.change_revision();
                        publish_queue(&inner, session_id.clone(), &inbox);
                    }
                    _ = cancellation.cancelled() => break,
                    _ = inner.relay_stop.notified() => break,
                }
            }
        });
        lock(&self.inner.relays).push(task);
    }

    fn publish_queue(&self, session_id: SessionId, inbox: crate::agent::Inbox) {
        publish_queue(&self.inner, session_id, &inbox);
    }

    fn start_approval_relay(&self) {
        let inner = Arc::clone(&self.inner);
        let mut receiver = inner.approvals.subscribe();
        let task = tokio::spawn(async move {
            loop {
                if inner.relays_closed.load(Ordering::Acquire) {
                    break;
                }
                tokio::select! {
                    notice = receiver.recv() => match notice {
                        Ok(ApprovalNotification::Requested(notice)) => {
                            let _ = inner.notices.send(HostNotification::ApprovalRequested(notice));
                        }
                        Ok(ApprovalNotification::Resolved(notice)) => {
                            let _ = inner.notices.send(HostNotification::ApprovalResolved(notice));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = inner.relay_stop.notified() => break,
                }
            }
        });
        lock(&self.inner.relays).push(task);
    }

    fn start_question_relay(&self) {
        let inner = Arc::clone(&self.inner);
        let mut receiver = inner.questions.subscribe();
        let task = tokio::spawn(async move {
            loop {
                if inner.relays_closed.load(Ordering::Acquire) {
                    break;
                }
                tokio::select! {
                    notice = receiver.recv() => match notice {
                        Ok(QuestionNotification::Requested(notice)) => {
                            let _ = inner.notices.send(HostNotification::QuestionRequested(notice));
                        }
                        Ok(QuestionNotification::Resolved(notice)) => {
                            let _ = inner.notices.send(HostNotification::QuestionResolved(notice));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) | Err(broadcast::error::RecvError::Closed) => break,
                    },
                    _ = inner.relay_stop.notified() => break,
                }
            }
        });
        lock(&self.inner.relays).push(task);
    }

    fn transition(&self, session_id: SessionId, status: SessionStatus) {
        let changed = {
            let mut state = lock(&self.inner.state);
            if state.statuses.get(&session_id) == Some(&status) {
                false
            } else {
                state.statuses.insert(session_id.clone(), status);
                true
            }
        };
        if changed {
            let _ = self.inner.notices.send(HostNotification::SessionStatus(
                SessionStatusNotification { session_id, status },
            ));
        }
    }

    fn watch_idle(&self, session_id: SessionId, agent: AgentHandle) {
        let host = self.clone();
        tokio::spawn(async move {
            let _ = agent.when_idle().await;
            host.publish_queue(session_id.clone(), agent.inbox());
            if !host.is_shutting_down() {
                host.transition(session_id, SessionStatus::Idle);
            }
        });
    }

    async fn stop_relays(&self) {
        self.inner.relays_closed.store(true, Ordering::Release);
        self.inner.relay_stop.notify_waiters();
        let relays = std::mem::take(&mut *lock(&self.inner.relays));
        for relay in relays {
            let _ = relay.await;
        }
    }

    async fn sweep_orphaned_attachments(&self) -> Result<(), HostError> {
        let sessions = self
            .inner
            .persistence
            .list(self.inner.cancellation.clone())
            .await?;
        // ponytail: skip rather than risk deleting a referenced blob when durable history exceeds one bounded sweep.
        if sessions.len() > MAX_ORPHAN_SWEEP_SESSIONS {
            return Ok(());
        }
        let mut referenced = BTreeSet::new();
        for session in sessions {
            for event in self
                .inner
                .persistence
                .read_from(&session.header.id, 0, self.inner.cancellation.clone())
                .await?
            {
                collect_attachment_refs(&event.data, &mut referenced);
            }
        }
        let directory = self.inner.attachments.root().join("v1");
        let mut entries = match fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(
                    AttachmentError::Storage(format!("list attachment directory: {error}")).into(),
                )
            }
        };
        for _ in 0..MAX_ORPHAN_SWEEP_ENTRIES {
            let Some(entry) = entries.next_entry().await.map_err(|error| {
                AttachmentError::Storage(format!("read attachment directory: {error}"))
            })?
            else {
                break;
            };
            if !entry
                .file_type()
                .await
                .map_err(|error| {
                    AttachmentError::Storage(format!("inspect attachment file: {error}"))
                })?
                .is_file()
            {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(attachment_id) = AttachmentId::try_from(format!("sha256:{name}")) else {
                continue;
            };
            if !referenced.contains(&attachment_id) {
                fs::remove_file(entry.path()).await.map_err(|error| {
                    AttachmentError::Storage(format!("remove orphan attachment: {error}"))
                })?;
            }
        }
        Ok(())
    }
}

fn find_attachment_ref(value: &Value, attachment_id: &AttachmentId) -> Option<AttachmentRef> {
    if let Ok(reference) = AttachmentRef::from_value(value) {
        if reference.attachment_id.as_str() == attachment_id.as_str() {
            return Some(reference);
        }
    }
    match value {
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_attachment_ref(value, attachment_id)),
        Value::Object(values) => values
            .values()
            .find_map(|value| find_attachment_ref(value, attachment_id)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

fn collect_attachment_refs(value: &Value, references: &mut BTreeSet<AttachmentId>) {
    if let Ok(reference) = AttachmentRef::from_value(value) {
        references.insert(reference.attachment_id);
        return;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                collect_attachment_refs(value, references);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_attachment_refs(value, references);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn relay_settings_notification(inner: &HostInner, event: SettingsEvent) {
    let default_model_changed = event.namespace == AGENT_DEFAULT_MODEL_NAMESPACE;
    let _ = inner.notices.send(HostNotification::SettingsChanged(event));
    if default_model_changed {
        let _ = inner.notices.send(HostNotification::ModelsChanged);
    }
}

fn collect_image_plans(
    blocks: &[ContentBlock],
    store: &AttachmentStore,
    plans: &mut Vec<ImagePlan>,
    inputs: &mut Vec<AttachmentInput>,
) -> Result<(), AttachmentError> {
    for block in blocks {
        match block {
            ContentBlock::Image { attachment } => {
                if let Ok(reference) = AttachmentRef::from_value(attachment) {
                    plans.push(ImagePlan::Reference(reference));
                } else {
                    let input = decode_inline_image(attachment)?;
                    let metadata = store.validate(&input)?;
                    plans.push(ImagePlan::Inline(metadata));
                    inputs.push(input);
                }
            }
            ContentBlock::ToolResult { content, .. } => {
                collect_image_plans(content, store, plans, inputs)?;
            }
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolCall { .. } => {}
        }
    }
    Ok(())
}

fn replace_image_plans(
    blocks: &mut [ContentBlock],
    plans: &[ImagePlan],
    plan_index: &mut usize,
    inline_refs: &[AttachmentRef],
    inline_index: &mut usize,
) -> Result<(), HostError> {
    for block in blocks {
        match block {
            ContentBlock::Image { attachment } => {
                let plan = plans.get(*plan_index).ok_or_else(|| {
                    HostError::invalid(
                        "INVALID_IMAGE_BLOCK",
                        "image normalization plan is incomplete",
                    )
                })?;
                let reference = match plan {
                    ImagePlan::Reference(reference) => reference.clone(),
                    ImagePlan::Inline(_) => {
                        let reference = inline_refs.get(*inline_index).ok_or_else(|| {
                            HostError::invalid(
                                "INVALID_IMAGE_BLOCK",
                                "inline image upload is incomplete",
                            )
                        })?;
                        *inline_index += 1;
                        reference.clone()
                    }
                };
                *attachment = serde_json::to_value(reference).map_err(|error| {
                    HostError::InvalidConfiguration(format!(
                        "serialize attachment reference: {error}"
                    ))
                })?;
                *plan_index += 1;
            }
            ContentBlock::ToolResult { content, .. } => {
                replace_image_plans(content, plans, plan_index, inline_refs, inline_index)?;
            }
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolCall { .. } => {}
        }
    }
    Ok(())
}

#[async_trait]
impl HostApi for HostHandle {
    async fn plugin_inventory(&self) -> Result<Vec<Value>, TessivumError> {
        let loader = self.inner.loader.lock().await;
        Ok(loader.as_ref().map_or_else(Vec::new, |loader| {
            loader
                .tree()
                .entries()
                .into_iter()
                .map(|entry| {
                    json!({
                        "entryId": entry.options.id.as_str(),
                        "moduleName": entry.options.name.as_deref().unwrap_or(&entry.package),
                        "enabled": !entry.options.disabled,
                        "fiberPhase": (!entry.options.disabled).then_some("active"),
                    })
                })
                .collect()
        }))
    }

    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult, TessivumError> {
        self.initialize_inner(params).await.map_err(HostError::wire)
    }
    async fn prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.prompt_inner(params).await.map_err(HostError::wire)
    }
    async fn steer(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.steer_inner(params).await.map_err(HostError::wire)
    }
    async fn cancel(
        &self,
        session: SessionId,
        cause: AgentCancelCause,
    ) -> Result<bool, TessivumError> {
        self.cancel_inner(session, cause)
            .await
            .map_err(HostError::wire)
    }
    async fn update_queue(
        &self,
        params: SessionUpdateQueueParams,
    ) -> Result<SessionUpdateQueueResult, TessivumError> {
        self.update_queue_inner(params)
            .await
            .map_err(HostError::wire)
    }
    async fn session_projections(
        &self,
        session: SessionId,
    ) -> Result<Vec<HostSessionProjection>, TessivumError> {
        let snapshots = match self.inner.projections.snapshots(&session, None) {
            Ok(snapshots) => snapshots,
            Err(crate::projection::ProjectionError::NotAttached { .. }) => {
                let Some(live) = self.inner.sessions.get(&session) else {
                    return Ok(Vec::new());
                };
                self.inner.projections.attach(live).map_err(|error| {
                    TessivumError::new(error.code(), error.to_string(), "projection", Value::Null)
                })?;
                self.inner
                    .projections
                    .snapshots(&session, None)
                    .map_err(|error| {
                        TessivumError::new(
                            error.code(),
                            error.to_string(),
                            "projection",
                            Value::Null,
                        )
                    })?
            }
            Err(error) => {
                return Err(TessivumError::new(
                    error.code(),
                    error.to_string(),
                    "projection",
                    Value::Null,
                ));
            }
        };
        Ok(snapshots
            .into_iter()
            .map(|snapshot| HostSessionProjection {
                key: snapshot.key,
                value: snapshot.view,
                seq: snapshot.as_of_seq,
            })
            .collect())
    }
    async fn events(
        &self,
        session: SessionId,
        from_seq: u64,
    ) -> Result<Vec<SessionEvent>, TessivumError> {
        self.events_inner(session, from_seq)
            .await
            .map_err(HostError::wire)
    }
    async fn status(&self, session: SessionId) -> Result<Option<SessionStatus>, TessivumError> {
        self.status_inner(session).await.map_err(HostError::wire)
    }

    async fn read_raw_session(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionRawArtifact>, TessivumError> {
        self.read_raw_session_inner(session)
            .await
            .map_err(HostError::wire)
    }
    async fn subagent_list(
        &self,
        parent_session_id: SessionId,
    ) -> Result<SubagentCatalog, TessivumError> {
        self.subagent_list_inner(parent_session_id)
            .await
            .map_err(HostError::wire)
    }
    async fn subagent_history(
        &self,
        params: SubagentHistoryRequest,
    ) -> Result<SubagentHistoryResult, TessivumError> {
        self.subagent_history_inner(params)
            .await
            .map_err(HostError::wire)
    }
    async fn subagent_prompt(
        &self,
        params: SubagentPromptRequest,
    ) -> Result<SubagentPromptResult, TessivumError> {
        self.subagent_prompt_inner(params)
            .await
            .map_err(HostError::wire)
    }
    async fn subagent_interrupt(
        &self,
        params: SubagentInterruptRequest,
    ) -> Result<SubagentInterruptResult, TessivumError> {
        self.subagent_interrupt_inner(params)
            .await
            .map_err(HostError::wire)
    }
    async fn subagent_delete(
        &self,
        params: SubagentDeleteRequest,
    ) -> Result<SubagentDeleteResult, TessivumError> {
        self.subagent_delete_inner(params)
            .await
            .map_err(HostError::wire)
    }

    async fn goal_service(&self, session: SessionId) -> Result<GoalService, TessivumError> {
        self.goal_service_inner(session)
            .await
            .map_err(HostError::wire)
    }
    async fn command_list(
        &self,
        session: SessionId,
    ) -> Result<Vec<HostCommandDescriptor>, TessivumError> {
        self.command_list_inner(session)
            .await
            .map_err(HostError::wire)
    }
    async fn command_execute(
        &self,
        session: SessionId,
        line: String,
    ) -> Result<Option<HostCommandExecution>, TessivumError> {
        self.command_execute_inner(session, line)
            .await
            .map_err(HostError::wire)
    }
    async fn message_feedback_list(&self, session: SessionId) -> Result<Value, TessivumError> {
        self.message_feedback_list_inner(session)
            .await
            .map_err(HostError::wire)
    }
    async fn message_feedback_put(
        &self,
        session: SessionId,
        message_id: String,
        rating: String,
        note: Option<String>,
        if_version: Option<String>,
    ) -> Result<Value, TessivumError> {
        self.message_feedback_put_inner(session, message_id, rating, note, if_version)
            .await
            .map_err(HostError::wire)
    }
    async fn message_feedback_delete(
        &self,
        session: SessionId,
        message_id: String,
        if_version: String,
    ) -> Result<Value, TessivumError> {
        self.message_feedback_delete_inner(session, message_id, if_version)
            .await
            .map_err(HostError::wire)
    }
    async fn create_session(
        &self,
        session_id: SessionId,
    ) -> Result<HostSessionInfo, TessivumError> {
        self.create_session_inner(session_id)
            .await
            .map_err(HostError::wire)
    }
    async fn create_session_in(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<HostSessionInfo, TessivumError> {
        self.create_session_in_inner(session_id, workspace_id)
            .await
            .map_err(HostError::wire)
    }
    async fn delete_workspace(&self, workspace_id: WorkspaceId) -> Result<bool, TessivumError> {
        self.delete_workspace_inner(workspace_id)
            .await
            .map_err(HostError::wire)
    }
    async fn list_sessions(&self) -> Result<Vec<HostSessionInfo>, TessivumError> {
        let sessions = self
            .inner
            .persistence
            .list(self.inner.cancellation.clone())
            .await
            .map_err(HostError::from)
            .map_err(HostError::wire)?;
        let mut listed = Vec::with_capacity(sessions.len());
        for session in sessions {
            self.inner
                .workspace_registry
                .recognize_session(&session.header.id)
                .map_err(HostError::from)
                .map_err(HostError::wire)?;
            // ponytail: fold activity and preset events on list; add a persistence index if large histories make this measurable.
            let events = self
                .inner
                .persistence
                .read_from(&session.header.id, 0, self.inner.cancellation.clone())
                .await
                .map_err(HostError::from)
                .map_err(HostError::wire)?;
            let updated_at = events
                .iter()
                .rev()
                .find(|event| event.event_type == "user/message")
                .map_or(session.header.created_at, |event| event.time);
            let agent_preset = events
                .iter()
                .rev()
                .find(|event| event.event_type == "agent-preset/selected")
                .and_then(|event| {
                    event
                        .data
                        .get("agentPreset")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .or_else(|| session.header.agent_preset.clone());
            listed.push(HostSessionInfo {
                workspace_id: self
                    .inner
                    .workspace_registry
                    .workspace_for_session(&session.header.id)
                    .map(|workspace| workspace.workspace_id),
                session_id: session.header.id.clone(),
                created_at: session.header.created_at,
                updated_at,
                running: self.session_running(&session.header.id),
                cwd: session.header.cwd,
                parent_session: session.header.parent_session,
                origin: session.header.origin,
                agent_preset,
                event_count: session.event_count,
                blank: !has_model_visible_work(&events),
            });
        }
        Ok(listed)
    }
    async fn search_sessions(
        &self,
        query: String,
    ) -> Result<HostSessionSearchResult, TessivumError> {
        self.search_sessions_inner(query)
            .await
            .map_err(HostError::wire)
    }
    async fn rename_session(
        &self,
        session_id: SessionId,
        title: String,
    ) -> Result<HostSessionRenameResult, TessivumError> {
        self.rename_session_inner(session_id, title)
            .await
            .map_err(HostError::wire)
    }
    async fn fork_session(
        &self,
        session_id: SessionId,
        at_seq: Option<u64>,
    ) -> Result<SessionId, TessivumError> {
        self.fork_session_inner(session_id, at_seq)
            .await
            .map_err(HostError::wire)
    }
    fn provider_directory(&self) -> Vec<HostProviderDirectoryEntry> {
        HostHandle::provider_directory(self)
    }
    fn model_groups(&self, provider: &str) -> Vec<HostModelGroup> {
        HostHandle::model_groups(self, provider)
    }
    async fn provider_models(
        &self,
        provider: String,
        config: Value,
    ) -> Result<HostProviderModels, TessivumError> {
        HostHandle::provider_models(self, provider, config)
            .await
            .map_err(HostError::wire)
    }
    async fn session_models(&self, session: SessionId) -> Result<HostSessionModels, TessivumError> {
        HostHandle::session_models(self, session)
            .await
            .map_err(HostError::wire)
    }
    async fn select_model(
        &self,
        session: SessionId,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Result<SessionModelSelection, TessivumError> {
        HostHandle::select_model(self, session, provider, model, reasoning_effort)
            .await
            .map_err(HostError::wire)
    }
    async fn set_provider_enabled(
        &self,
        provider: String,
        enabled: bool,
    ) -> Result<HostProviderEnabled, TessivumError> {
        HostHandle::set_provider_enabled(self, provider, enabled)
            .await
            .map_err(HostError::wire)
    }
    fn attachment_limits(&self) -> AttachmentLimits {
        HostHandle::attachment_limits(self)
    }
    async fn upload_attachment(
        &self,
        data: Vec<u8>,
        name: Option<String>,
    ) -> Result<AttachmentRef, TessivumError> {
        self.upload_attachment_inner(data, name)
            .await
            .map_err(HostError::wire)
    }
    async fn read_attachment(
        &self,
        session: SessionId,
        attachment_id: AttachmentId,
    ) -> Result<AttachmentData, TessivumError> {
        self.read_attachment_inner(session, attachment_id)
            .await
            .map_err(HostError::wire)
    }

    async fn mutate_settings(
        &self,
        namespace: String,
        mutation: HostSettingsMutation,
    ) -> Result<SettingsSnapshot, SettingsError> {
        mutate_settings_inner(&self.inner, namespace, mutation).await
    }
    async fn normalize_prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptParams, TessivumError> {
        self.normalize_prompt_inner(params)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_list(&self) -> Result<Vec<AgentPresetSummary>, TessivumError> {
        HostHandle::agent_preset_list(self)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_read(&self, id: String) -> Result<AgentPresetDocument, TessivumError> {
        HostHandle::agent_preset_read(self, id)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_copy(
        &self,
        from: String,
        target: String,
        name: Option<String>,
    ) -> Result<String, TessivumError> {
        HostHandle::agent_preset_copy(self, from, target, name)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_remove(&self, id: String) -> Result<(), TessivumError> {
        HostHandle::agent_preset_remove(self, id)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_path(&self, id: String) -> Result<(String, String), TessivumError> {
        HostHandle::agent_preset_path(self, id)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_open_document(
        &self,
        id: String,
    ) -> Result<HostAgentPresetDocument, TessivumError> {
        HostHandle::agent_preset_open_document(self, id)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_select(
        &self,
        session: SessionId,
        preset: String,
    ) -> Result<String, TessivumError> {
        HostHandle::agent_preset_select(self, session, preset)
            .await
            .map_err(HostError::wire)
    }
    fn agent_preset_capabilities(&self) -> (bool, bool) {
        (
            self.inner.agent_presets.authorable(),
            self.inner.config.path_opener.is_some(),
        )
    }
    fn descriptor(&self) -> HostDescriptor {
        HostDescriptor {
            cwd: self.inner.identity.cwd.to_string_lossy().into_owned(),
            provider: self.inner.config.provider.clone(),
            model: self.inner.config.model.clone(),
            max_tokens: self.inner.config.max_tokens,
        }
    }
    fn approval_registry(&self) -> Option<HostApprovalRegistry> {
        Some(self.inner.approvals.clone())
    }
    fn question_registry(&self) -> Option<HostQuestionRegistry> {
        Some(self.inner.questions.clone())
    }
    fn workspace_registry(&self) -> Option<WorkspaceRegistry> {
        Some(self.inner.workspace_registry.clone())
    }
    fn default_workspace_id(&self) -> Option<WorkspaceId> {
        self.inner.default_workspace_id.clone()
    }
    fn can_open_path(&self) -> bool {
        self.inner.path_opener.can_open_path()
    }
    fn has_settings_document(&self) -> bool {
        self.inner.settings.document_path().is_some()
    }
    fn settings(&self) -> Option<Arc<Settings>> {
        Some(Arc::clone(&self.inner.settings))
    }
    fn dynamic_cordis_inventory(&self) -> Result<Value, TessivumError> {
        Ok(self
            .dynamic_cordis()
            .map(DynamicCordisRegistry::inventory)
            .unwrap_or_else(|| Value::Array(Vec::new())))
    }
    async fn dynamic_cordis_run_host_half(&self, args: Value) -> Result<Value, TessivumError> {
        let registry = self.dynamic_cordis().cloned().ok_or_else(|| {
            TessivumError::new(
                "CORDIS_UNAVAILABLE",
                "dynamic Cordis compatibility is disabled",
                "cordis",
                Value::Null,
            )
        })?;
        let session_id =
            SessionId::from(args.get("agentId").and_then(Value::as_str).ok_or_else(|| {
                TessivumError::new(
                    "INVALID_CORDIS_REQUEST",
                    "agentId is required",
                    "cordis",
                    Value::Null,
                )
            })?);
        let result = registry.run_host_half(&args).await?;
        self.append_dynamic_cordis_fact(session_id, "runHostHalf", args, result.clone())
            .await?;
        Ok(result)
    }
    async fn dynamic_cordis_call(&self, method: &str, args: Value) -> Result<Value, TessivumError> {
        let Some(registry) = self.dynamic_cordis().cloned() else {
            if method == "syncInspectManifest" {
                return Ok(Value::Null);
            }
            return Err(TessivumError::new(
                "CORDIS_UNAVAILABLE",
                "dynamic Cordis compatibility is disabled",
                "cordis",
                Value::Null,
            ));
        };
        let resolved_request_owner = if method == "resolveRequestRun" {
            args.get("requestId")
                .and_then(Value::as_str)
                .and_then(|request_id| registry.pending_request_owner(request_id))
        } else {
            None
        };
        let mut result = match method {
            "getClientCode" => registry.get_client_code(&args),
            "resolveRequestRun" => registry.resolve_request_run(&args).await,
            "undefineFromPanel" => registry.undefine_from_panel(&args),
            "settleUserRun" => registry.settle_user_run(&args).await,
            "stopFromPanel" => registry.stop_from_panel(&args),
            "syncInspectManifest" => registry.sync_inspect_manifest(&args),
            "resolveInspectQuery" => registry.resolve_inspect_query(&args),
            "reportRenderFailure" => registry.report_render_failure(&args),
            "reportClientGuardFailure" => registry.report_client_guard_failure(&args),
            "invoke" => registry.invoke(&args).await,
            _ => Err(TessivumError::new(
                "CORDIS_METHOD_NOT_FOUND",
                "dynamic Cordis method was not found",
                "cordis",
                json!({"method": method}),
            )),
        }?;
        let activation_context = result
            .as_object_mut()
            .and_then(|object| object.remove("_context"))
            .and_then(|value| value.as_str().map(str::to_owned));
        if let (Some(session_id), Some(text)) =
            (resolved_request_owner.as_ref(), activation_context)
        {
            if let Some(agent) = self.inner.registry.get(session_id) {
                agent
                    .steer(Message {
                        id: MessageId::random(),
                        role: MessageRole::User,
                        content: vec![ContentBlock::Text { text }],
                        source: MessageSource::Plugin {
                            plugin: "cordis-host-runner".into(),
                            compaction_id: None,
                            form: None,
                            sections: None,
                            summary: None,
                        },
                    })
                    .await
                    .map_err(|error| {
                        TessivumError::new(
                            "CORDIS_AGENT_DELIVERY_FAILED",
                            error.to_string(),
                            "cordis",
                            json!({"sessionId": session_id}),
                        )
                    })?;
            }
        }
        let event_session = if method == "resolveRequestRun" {
            resolved_request_owner
        } else if matches!(
            method,
            "undefineFromPanel"
                | "settleUserRun"
                | "stopFromPanel"
                | "resolveInspectQuery"
                | "reportRenderFailure"
                | "reportClientGuardFailure"
        ) {
            Some(SessionId::from(
                args.get("agentId").and_then(Value::as_str).ok_or_else(|| {
                    TessivumError::new(
                        "INVALID_CORDIS_REQUEST",
                        "agentId is required",
                        "cordis",
                        Value::Null,
                    )
                })?,
            ))
        } else {
            None
        };
        if let Some(session_id) = event_session {
            self.append_dynamic_cordis_fact(session_id, method, args, result.clone())
                .await?;
        }
        Ok(result)
    }
    fn credentials(&self) -> Option<Arc<Credentials>> {
        Some(Arc::clone(&self.inner.credentials))
    }
    fn subscribe(&self) -> broadcast::Receiver<HostNotification> {
        self.inner.notices.subscribe()
    }
    async fn pick_directory(&self) -> Result<Option<String>, TessivumError> {
        self.pick_directory_inner().await
    }
    async fn open_path(&self, path: String) -> Result<(), TessivumError> {
        self.open_path_inner(path).await
    }
    async fn open_settings_document(&self) -> Result<(), TessivumError> {
        self.open_settings_document_inner().await
    }
    async fn shutdown(&self) -> Result<(), TessivumError> {
        self.shutdown_inner().await.map_err(HostError::wire)
    }
}

#[async_trait]
impl HostApi for HostRuntime {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult, TessivumError> {
        self.handle.initialize(params).await
    }
    async fn prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.handle.prompt(params).await
    }
    async fn steer(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptResult, TessivumError> {
        self.handle.steer(params).await
    }
    async fn cancel(
        &self,
        session: SessionId,
        cause: AgentCancelCause,
    ) -> Result<bool, TessivumError> {
        self.handle.cancel(session, cause).await
    }
    async fn update_queue(
        &self,
        params: SessionUpdateQueueParams,
    ) -> Result<SessionUpdateQueueResult, TessivumError> {
        self.handle.update_queue(params).await
    }
    async fn events(
        &self,
        session: SessionId,
        from_seq: u64,
    ) -> Result<Vec<SessionEvent>, TessivumError> {
        self.handle.events(session, from_seq).await
    }
    async fn status(&self, session: SessionId) -> Result<Option<SessionStatus>, TessivumError> {
        self.handle.status(session).await
    }

    async fn read_raw_session(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionRawArtifact>, TessivumError> {
        self.handle.read_raw_session(session).await
    }
    async fn subagent_list(
        &self,
        parent_session_id: SessionId,
    ) -> Result<SubagentCatalog, TessivumError> {
        self.handle.subagent_list(parent_session_id).await
    }
    async fn subagent_history(
        &self,
        params: SubagentHistoryRequest,
    ) -> Result<SubagentHistoryResult, TessivumError> {
        self.handle.subagent_history(params).await
    }
    async fn subagent_prompt(
        &self,
        params: SubagentPromptRequest,
    ) -> Result<SubagentPromptResult, TessivumError> {
        self.handle.subagent_prompt(params).await
    }
    async fn subagent_interrupt(
        &self,
        params: SubagentInterruptRequest,
    ) -> Result<SubagentInterruptResult, TessivumError> {
        self.handle.subagent_interrupt(params).await
    }
    async fn subagent_delete(
        &self,
        params: SubagentDeleteRequest,
    ) -> Result<SubagentDeleteResult, TessivumError> {
        self.handle.subagent_delete(params).await
    }

    async fn goal_service(&self, session: SessionId) -> Result<GoalService, TessivumError> {
        self.handle.goal_service(session).await
    }
    async fn command_list(
        &self,
        session: SessionId,
    ) -> Result<Vec<HostCommandDescriptor>, TessivumError> {
        self.handle.command_list(session).await
    }
    async fn command_execute(
        &self,
        session: SessionId,
        line: String,
    ) -> Result<Option<HostCommandExecution>, TessivumError> {
        self.handle.command_execute(session, line).await
    }
    async fn create_session(
        &self,
        session_id: SessionId,
    ) -> Result<HostSessionInfo, TessivumError> {
        self.handle.create_session(session_id).await
    }
    async fn message_feedback_list(&self, session: SessionId) -> Result<Value, TessivumError> {
        self.handle.message_feedback_list(session).await
    }
    async fn message_feedback_put(
        &self,
        session: SessionId,
        message_id: String,
        rating: String,
        note: Option<String>,
        if_version: Option<String>,
    ) -> Result<Value, TessivumError> {
        self.handle
            .message_feedback_put(session, message_id, rating, note, if_version)
            .await
    }
    async fn message_feedback_delete(
        &self,
        session: SessionId,
        message_id: String,
        if_version: String,
    ) -> Result<Value, TessivumError> {
        self.handle
            .message_feedback_delete(session, message_id, if_version)
            .await
    }
    async fn create_session_in(
        &self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<HostSessionInfo, TessivumError> {
        self.handle
            .create_session_in(session_id, workspace_id)
            .await
    }
    async fn delete_workspace(&self, workspace_id: WorkspaceId) -> Result<bool, TessivumError> {
        self.handle.delete_workspace(workspace_id).await
    }
    async fn list_sessions(&self) -> Result<Vec<HostSessionInfo>, TessivumError> {
        self.handle.list_sessions().await
    }
    async fn search_sessions(
        &self,
        query: String,
    ) -> Result<HostSessionSearchResult, TessivumError> {
        self.handle.search_sessions(query).await
    }
    async fn rename_session(
        &self,
        session_id: SessionId,
        title: String,
    ) -> Result<HostSessionRenameResult, TessivumError> {
        self.handle.rename_session(session_id, title).await
    }
    async fn fork_session(
        &self,
        session_id: SessionId,
        at_seq: Option<u64>,
    ) -> Result<SessionId, TessivumError> {
        self.handle.fork_session(session_id, at_seq).await
    }
    fn provider_directory(&self) -> Vec<HostProviderDirectoryEntry> {
        self.handle.provider_directory()
    }
    fn model_groups(&self, provider: &str) -> Vec<HostModelGroup> {
        self.handle.model_groups(provider)
    }
    async fn provider_models(
        &self,
        provider: String,
        config: Value,
    ) -> Result<HostProviderModels, TessivumError> {
        self.handle
            .provider_models(provider, config)
            .await
            .map_err(HostError::wire)
    }
    async fn session_models(&self, session: SessionId) -> Result<HostSessionModels, TessivumError> {
        self.handle
            .session_models(session)
            .await
            .map_err(HostError::wire)
    }
    async fn select_model(
        &self,
        session: SessionId,
        provider: String,
        model: String,
        reasoning_effort: Option<String>,
    ) -> Result<SessionModelSelection, TessivumError> {
        self.handle
            .select_model(session, provider, model, reasoning_effort)
            .await
            .map_err(HostError::wire)
    }
    async fn set_provider_enabled(
        &self,
        provider: String,
        enabled: bool,
    ) -> Result<HostProviderEnabled, TessivumError> {
        self.handle
            .set_provider_enabled(provider, enabled)
            .await
            .map_err(HostError::wire)
    }
    fn attachment_limits(&self) -> AttachmentLimits {
        self.handle.attachment_limits()
    }
    async fn upload_attachment(
        &self,
        data: Vec<u8>,
        name: Option<String>,
    ) -> Result<AttachmentRef, TessivumError> {
        self.handle.upload_attachment(data, name).await
    }
    async fn read_attachment(
        &self,
        session: SessionId,
        attachment_id: AttachmentId,
    ) -> Result<AttachmentData, TessivumError> {
        self.handle.read_attachment(session, attachment_id).await
    }

    async fn normalize_prompt(
        &self,
        params: SessionPromptParams,
    ) -> Result<SessionPromptParams, TessivumError> {
        self.handle.normalize_prompt(params).await
    }
    async fn mutate_settings(
        &self,
        namespace: String,
        mutation: HostSettingsMutation,
    ) -> Result<SettingsSnapshot, SettingsError> {
        self.handle.mutate_settings(namespace, mutation).await
    }
    async fn agent_preset_list(&self) -> Result<Vec<AgentPresetSummary>, TessivumError> {
        self.handle
            .agent_preset_list()
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_read(&self, id: String) -> Result<AgentPresetDocument, TessivumError> {
        self.handle
            .agent_preset_read(id)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_copy(
        &self,
        from: String,
        target: String,
        name: Option<String>,
    ) -> Result<String, TessivumError> {
        self.handle
            .agent_preset_copy(from, target, name)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_remove(&self, id: String) -> Result<(), TessivumError> {
        self.handle
            .agent_preset_remove(id)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_path(&self, id: String) -> Result<(String, String), TessivumError> {
        self.handle
            .agent_preset_path(id)
            .await
            .map_err(HostError::wire)
    }
    async fn agent_preset_open_document(
        &self,
        id: String,
    ) -> Result<HostAgentPresetDocument, TessivumError> {
        self.handle
            .agent_preset_open_document(id)
            .await
            .map_err(HostError::wire)
    }

    async fn agent_preset_select(
        &self,
        session: SessionId,
        preset: String,
    ) -> Result<String, TessivumError> {
        self.handle
            .agent_preset_select(session, preset)
            .await
            .map_err(HostError::wire)
    }
    fn agent_preset_capabilities(&self) -> (bool, bool) {
        self.handle.agent_preset_capabilities()
    }
    fn descriptor(&self) -> HostDescriptor {
        self.handle.descriptor()
    }
    fn approval_registry(&self) -> Option<HostApprovalRegistry> {
        self.handle.approval_registry()
    }
    fn question_registry(&self) -> Option<HostQuestionRegistry> {
        self.handle.question_registry()
    }
    fn workspace_registry(&self) -> Option<WorkspaceRegistry> {
        self.handle.workspace_registry()
    }
    fn default_workspace_id(&self) -> Option<WorkspaceId> {
        HostApi::default_workspace_id(&self.handle)
    }
    fn can_open_path(&self) -> bool {
        self.handle.can_open_path()
    }
    fn has_settings_document(&self) -> bool {
        self.handle.has_settings_document()
    }
    fn settings(&self) -> Option<Arc<Settings>> {
        self.handle.settings()
    }
    fn dynamic_cordis_inventory(&self) -> Result<Value, TessivumError> {
        self.handle.dynamic_cordis_inventory()
    }
    async fn dynamic_cordis_run_host_half(&self, args: Value) -> Result<Value, TessivumError> {
        self.handle.dynamic_cordis_run_host_half(args).await
    }
    async fn dynamic_cordis_call(&self, method: &str, args: Value) -> Result<Value, TessivumError> {
        self.handle.dynamic_cordis_call(method, args).await
    }
    fn credentials(&self) -> Option<Arc<Credentials>> {
        self.handle.credentials()
    }
    fn subscribe(&self) -> broadcast::Receiver<HostNotification> {
        self.handle.subscribe()
    }
    async fn pick_directory(&self) -> Result<Option<String>, TessivumError> {
        self.handle.pick_directory().await
    }
    async fn open_path(&self, path: String) -> Result<(), TessivumError> {
        self.handle.open_path(path).await
    }
    async fn open_settings_document(&self) -> Result<(), TessivumError> {
        self.handle.open_settings_document().await
    }
    async fn shutdown(&self) -> Result<(), TessivumError> {
        self.handle.shutdown().await
    }
}

fn feedback_command_descriptor() -> HostCommandDescriptor {
    HostCommandDescriptor {
        name: "feedback".into(),
        description: "record feedback about this session".into(),
        input: Some(HostCommandInputDescriptor {
            hint: "<text>".into(),
        }),
    }
}
fn permission_command_descriptor() -> HostCommandDescriptor {
    HostCommandDescriptor {
        name: "permission".into(),
        description: "Switch the permission preset (sandbox mode + approval policy)".into(),
        input: Some(HostCommandInputDescriptor {
            hint: "<preset>".into(),
        }),
    }
}
fn simple_command_descriptor(name: &str, description: &str) -> HostCommandDescriptor {
    HostCommandDescriptor {
        name: name.into(),
        description: description.into(),
        input: None,
    }
}

fn plan_command_descriptor() -> HostCommandDescriptor {
    HostCommandDescriptor {
        name: "plan".into(),
        description: "Enter or leave plan mode".into(),
        input: Some(HostCommandInputDescriptor {
            hint: "[off|message]".into(),
        }),
    }
}

fn goal_command_descriptor() -> HostCommandDescriptor {
    HostCommandDescriptor {
        name: "goal".into(),
        description: "set or view the goal for a long-running task".into(),
        input: Some(HostCommandInputDescriptor {
            hint: "<objective>".into(),
        }),
    }
}

fn selected_agent_preset(session: &Session) -> Option<String> {
    session
        .events()
        .into_iter()
        .rev()
        .find(|event| event.event_type == "agent-preset/selected")
        .and_then(|event| {
            event
                .data
                .get("agentPreset")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .or_else(|| session.header().agent_preset)
}

fn parse_command(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('/')?;
    let name_end = rest.find(['\t', '\n', '\r', ' ']).unwrap_or(rest.len());
    let name = &rest[..name_end];
    let mut bytes = name.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        || !bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        return None;
    }
    Some((name, &rest[name_end..]))
}
/// SIGTERM is a graceful success; SIGINT remains shell cancellation (130).
pub async fn shutdown_signal() -> Result<i32, std::io::Error> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map(|()| 130),
            _ = term.recv() => Ok(0),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map(|()| 130)
    }
}

async fn mutate_settings_inner(
    inner: &Arc<HostInner>,
    namespace: String,
    mutation: HostSettingsMutation,
) -> Result<SettingsSnapshot, SettingsError> {
    let routed = inner.dynamic_routes
        && matches!(
            namespace.as_str(),
            LLM_PI_AI_NAMESPACE | LLM_DEEPSEEK_NAMESPACE
        );
    let _route_gate = if routed {
        Some(inner.route_gate.lock().await)
    } else {
        None
    };
    let previous = if routed {
        Some(inner.settings.user(&namespace)?)
    } else {
        None
    };
    let snapshot = match mutation {
        HostSettingsMutation::Update {
            patch,
            expected_revision,
        } => {
            inner
                .settings
                .update(&namespace, patch, expected_revision)
                .await?
        }
        HostSettingsMutation::Replace {
            user,
            expected_revision,
        } => {
            inner
                .settings
                .replace(&namespace, user, expected_revision)
                .await?
        }
        HostSettingsMutation::Mutate {
            ops,
            expected_revision,
        } => {
            inner
                .settings
                .mutate(&namespace, ops, expected_revision)
                .await?
        }
    };
    if routed {
        if let Err(error) = apply_route_settings_locked(inner).await {
            if let Err(rollback) = inner
                .settings
                .replace(
                    &namespace,
                    previous.expect("routed settings retain the previous user document"),
                    Some(snapshot.revision),
                )
                .await
            {
                return Err(SettingsError::Persistence(format!(
                    "live route apply failed: {error}; rollback failed: {rollback}"
                )));
            }
            return Err(SettingsError::Validation(error.wire()));
        }
        let _ = inner.notices.send(HostNotification::ModelsChanged);
    }
    Ok(snapshot)
}

async fn apply_route_settings_locked(inner: &Arc<HostInner>) -> Result<(), HostError> {
    let pi_snapshot = inner
        .settings
        .get(LLM_PI_AI_NAMESPACE)
        .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
    let mut candidate = parse_routes(&pi_snapshot.value, pi_snapshot.revision)
        .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
    let deepseek_snapshot = inner
        .settings
        .get(LLM_DEEPSEEK_NAMESPACE)
        .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
    candidate.routes.insert(
        DEEPSEEK_PROVIDER.into(),
        parse_deepseek_route(&deepseek_snapshot.value, deepseek_snapshot.revision)
            .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?,
    );
    candidate.retry_policies.insert(
        DEEPSEEK_PROVIDER.into(),
        LlmRetryPolicy::resolve(None).expect("default retry policy is valid"),
    );
    let candidate_routes = Arc::new(candidate.routes);
    let candidate_retry_policies = Arc::new(candidate.retry_policies);
    let (old_routes, old_retry_policies, mut old_registrations) = {
        let mut state = lock(&inner.route_state);
        (
            Arc::clone(&state.routes),
            Arc::clone(&state.retry_policies),
            std::mem::take(&mut state.registrations),
        )
    };
    for registration in old_registrations.values_mut() {
        registration.unregister();
    }
    let mut registrations = BTreeMap::new();
    for provider in candidate_routes.keys() {
        let retry_policy = candidate_retry_policies
            .get(provider)
            .cloned()
            .expect("parsed route has a retry policy");
        match inner.llm.register_with_retry_policy(
            provider.clone(),
            Arc::clone(&inner.route_adapter),
            Some(retry_policy),
        ) {
            Ok(registration) => {
                registrations.insert(provider.clone(), registration);
            }
            Err(error) => {
                for registration in registrations.values_mut() {
                    registration.unregister();
                }
                let restored = old_routes
                    .keys()
                    .map(|provider| {
                        inner
                            .llm
                            .register_with_retry_policy(
                                provider.clone(),
                                Arc::clone(&inner.route_adapter),
                                Some(
                                    old_retry_policies
                                        .get(provider)
                                        .cloned()
                                        .expect("parsed route has a retry policy"),
                                ),
                            )
                            .map(|registration| (provider.clone(), registration))
                    })
                    .collect::<Result<BTreeMap<_, _>, _>>();
                match restored {
                    Ok(registrations) => {
                        let mut state = lock(&inner.route_state);
                        state.routes = old_routes;
                        state.retry_policies = old_retry_policies;
                        state.registrations = registrations;
                        return Err(error.into());
                    }
                    Err(restore) => {
                        return Err(HostError::InvalidConfiguration(format!(
                            "route update failed: {error}; failed to restore previous registrations: {restore}"
                        )));
                    }
                }
            }
        }
    }
    *lock(&inner.route_resolver.routes) = Arc::clone(&candidate_routes);
    let mut state = lock(&inner.route_state);
    state.routes = candidate_routes;
    state.retry_policies = candidate_retry_policies;
    state.registrations = registrations;
    Ok(())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawOpenAiSettings {
    #[serde(default)]
    providers: BTreeMap<String, RawOpenAiRoute>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawOpenAiRoute {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default = "default_openai_responses_api")]
    api: String,
    #[serde(default, alias = "baseURL")]
    base_url: Option<String>,
    #[serde(default, alias = "credentialRef")]
    api_key_env: Option<String>,
    #[serde(default)]
    models: Vec<RawOpenAiModel>,
    #[serde(default)]
    retry_policy: Option<LlmRetryPolicyConfig>,
    #[serde(default)]
    #[serde(rename = "enabled")]
    _enabled: bool,
}

fn default_openai_responses_api() -> String {
    "openai-responses".into()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawOpenAiModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_modalities", alias = "inputModalities")]
    input: Vec<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    reasoning_efforts: Option<BTreeMap<String, Option<String>>>,
    #[serde(default, alias = "defaultReasoningEffort")]
    default_effort: Option<String>,
}

fn default_modalities() -> Vec<String> {
    vec![RESPONSES_TEXT_MODALITY.into()]
}
const REASONING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

fn parse_reasoning_efforts(
    declared: Option<BTreeMap<String, Option<String>>>,
) -> Result<Vec<ResponsesReasoningEffort>, TessivumError> {
    let Some(mut declared) = declared else {
        return Ok(Vec::new());
    };
    if declared.is_empty()
        || declared
            .keys()
            .any(|level| !REASONING_LEVELS.contains(&level.as_str()))
    {
        return Err(TessivumError::new(
            "INVALID_OPENAI_MODEL",
            "reasoningEfforts must declare supported thinking levels",
            "host",
            Value::Null,
        ));
    }
    let mut efforts = Vec::with_capacity(declared.len());
    for level in REASONING_LEVELS {
        let Some(wire) = declared.remove(level) else {
            continue;
        };
        if (level != "off" && wire.is_none())
            || wire.as_deref().is_some_and(|value| value.trim().is_empty())
        {
            return Err(TessivumError::new(
                "INVALID_OPENAI_MODEL",
                "reasoning effort wire values must be non-empty; only off may be null",
                "host",
                Value::Null,
            ));
        }
        efforts.push(ResponsesReasoningEffort {
            id: level.into(),
            wire,
        });
    }
    Ok(efforts)
}

struct ParsedRoutes {
    routes: BTreeMap<String, ResponsesRoute>,
    retry_policies: BTreeMap<String, LlmRetryPolicy>,
}

fn parse_models(raw_models: Vec<RawOpenAiModel>) -> Result<Vec<ResponsesModel>, TessivumError> {
    let mut models = Vec::with_capacity(raw_models.len());
    for raw_model in raw_models {
        if raw_model
            .context_window
            .is_some_and(|value| value > MAX_SAFE_INTEGER)
            || raw_model
                .max_tokens
                .is_some_and(|value| value > MAX_SAFE_INTEGER)
        {
            return Err(TessivumError::new(
                "INVALID_OPENAI_MODEL",
                "model limits must be safe positive integers",
                "host",
                Value::Null,
            ));
        }
        let mut input = raw_model.input;
        if input.is_empty() {
            input.push(RESPONSES_TEXT_MODALITY.into());
        }
        let mut seen = BTreeSet::new();
        if input.iter().any(|modality| !seen.insert(modality.clone())) {
            return Err(TessivumError::new(
                "INVALID_OPENAI_MODALITY",
                "model input modalities must be unique",
                "host",
                Value::Null,
            ));
        }
        let model = ResponsesModel {
            id: raw_model.id,
            name: raw_model.name,
            description: raw_model.description,
            input,
            context_window: raw_model.context_window,
            max_tokens: raw_model.max_tokens,
            reasoning_efforts: parse_reasoning_efforts(raw_model.reasoning_efforts)?,
            default_reasoning_effort: raw_model.default_effort,
        };
        model.validate()?;
        models.push(model);
    }
    Ok(models)
}

fn valid_provider_id(id: &str) -> bool {
    !id.is_empty()
        && id.bytes().enumerate().all(|(index, byte)| {
            (index == 0 && byte.is_ascii_lowercase())
                || (index > 0
                    && (byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
        })
}

fn parse_routes(value: &Value, revision: u64) -> Result<ParsedRoutes, TessivumError> {
    let raw: RawOpenAiSettings = serde_json::from_value(value.clone()).map_err(|error| {
        TessivumError::new(
            "INVALID_OPENAI_ROUTE_SETTINGS",
            format!("invalid provider settings: {error}"),
            "host",
            Value::Null,
        )
    })?;
    let mut routes = BTreeMap::new();
    let mut retry_policies = BTreeMap::new();
    let mut credential_owners = BTreeMap::<CredentialRef, ()>::new();
    for (id, raw_route) in raw.providers {
        if !valid_provider_id(&id) {
            return Err(TessivumError::new(
                "INVALID_OPENAI_ROUTE_SETTINGS",
                "provider routes require a valid id",
                "host",
                Value::Null,
            ));
        }
        if !matches!(
            raw_route.api.as_str(),
            "openai-responses" | "openai-completions" | "anthropic-messages"
        ) {
            return Err(TessivumError::new(
                "INVALID_OPENAI_ROUTE_SETTINGS",
                "provider routes must name a supported protocol",
                "host",
                Value::Null,
            ));
        }
        let catalog = builtin_pi_ai_provider(&id).map(builtin_pi_ai_route);
        let display_name = raw_route
            .display_name
            .or_else(|| catalog.as_ref().map(|route| route.display_name.clone()))
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                TessivumError::new(
                    "INVALID_OPENAI_ROUTE_SETTINGS",
                    "declared provider routes require a displayName",
                    "host",
                    json!({"provider": id}),
                )
            })?;
        let base_url = raw_route
            .base_url
            .or_else(|| catalog.as_ref().map(|route| route.base_url.clone()))
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| {
                TessivumError::new(
                    "INVALID_OPENAI_ROUTE_SETTINGS",
                    "declared provider routes require a baseURL",
                    "host",
                    json!({"provider": id}),
                )
            })?;
        let models = if raw_route.models.is_empty() {
            catalog.map(|route| route.models).ok_or_else(|| {
                TessivumError::new(
                    "INVALID_OPENAI_ROUTE_SETTINGS",
                    "declared provider routes require at least one model",
                    "host",
                    json!({"provider": id}),
                )
            })?
        } else {
            parse_models(raw_route.models)?
        };
        let credential_ref = raw_route.api_key_env.unwrap_or_default();
        if !credential_ref.is_empty() {
            let credential = CredentialRef::new(credential_ref.clone()).map_err(|_error| {
                TessivumError::new(
                    "INVALID_CREDENTIAL_REF",
                    "provider route contains an invalid credential reference",
                    "host",
                    json!({"provider": id}),
                )
            })?;
            if credential_owners.insert(credential, ()).is_some() {
                return Err(TessivumError::new(
                    "INVALID_OPENAI_ROUTE_SETTINGS",
                    "provider routes must not share credential references",
                    "host",
                    Value::Null,
                ));
            }
        }
        let retry_policy =
            LlmRetryPolicy::resolve(raw_route.retry_policy.as_ref()).map_err(|error| {
                TessivumError::new(error.code, error.message, "host", json!({"provider": id}))
            })?;
        let mut route =
            ResponsesRoute::new(id.clone(), display_name, base_url, credential_ref, models)
                .with_api(raw_route.api);
        route.generation = revision;
        route.validate()?;
        retry_policies.insert(id.clone(), retry_policy);
        routes.insert(id, route);
    }
    Ok(ParsedRoutes {
        routes,
        retry_policies,
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawDeepSeekSettings {
    api_key_env: String,
    #[serde(alias = "baseURL")]
    base_url: String,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    default_context_window: Option<u64>,
    models: Vec<RawOpenAiModel>,
    #[serde(default)]
    stream_idle_timeout_ms: Option<f64>,
    #[serde(default)]
    retry_policy: Option<LlmRetryPolicyConfig>,
}

fn parse_deepseek_route(value: &Value, revision: u64) -> Result<ResponsesRoute, TessivumError> {
    let raw: RawDeepSeekSettings = serde_json::from_value(value.clone()).map_err(|error| {
        TessivumError::new(
            "INVALID_DEEPSEEK_ROUTE_SETTINGS",
            format!("invalid DeepSeek settings: {error}"),
            "host",
            Value::Null,
        )
    })?;
    if raw
        .thinking
        .as_deref()
        .is_some_and(|value| !matches!(value, "enabled" | "disabled"))
        || raw
            .reasoning_effort
            .as_deref()
            .is_some_and(|value| !matches!(value, "off" | "high" | "max"))
        || raw
            .max_tokens
            .is_some_and(|value| value == 0 || value > MAX_SAFE_INTEGER)
        || raw
            .default_context_window
            .is_some_and(|value| value == 0 || value > MAX_SAFE_INTEGER)
        || raw
            .stream_idle_timeout_ms
            .is_some_and(|value| !value.is_finite() || value <= 0.0 || value > 2_147_483_647.0)
    {
        return Err(TessivumError::new(
            "INVALID_DEEPSEEK_ROUTE_SETTINGS",
            "DeepSeek settings contain an invalid request default",
            "host",
            Value::Null,
        ));
    }
    let credential = CredentialRef::new(raw.api_key_env.clone()).map_err(|error| {
        TessivumError::new(
            "INVALID_CREDENTIAL_REF",
            error.to_string(),
            "host",
            json!({"provider": DEEPSEEK_PROVIDER}),
        )
    })?;
    let default_context_window = raw.default_context_window.unwrap_or(1_000_000);
    let default_max_tokens = raw.max_tokens.unwrap_or(256_000);
    let mut models = parse_models(raw.models)?;
    if models.is_empty() {
        return Err(TessivumError::new(
            "INVALID_DEEPSEEK_ROUTE_SETTINGS",
            "DeepSeek requires at least one model",
            "host",
            Value::Null,
        ));
    }
    for model in &mut models {
        if model.context_window.is_none() {
            model.context_window = Some(default_context_window);
        }
        if model.max_tokens.is_none() {
            model.max_tokens = Some(default_max_tokens);
        }
    }
    LlmRetryPolicy::resolve(raw.retry_policy.as_ref()).map_err(|error| {
        TessivumError::new(
            error.code,
            error.message,
            "host",
            json!({"provider": DEEPSEEK_PROVIDER}),
        )
    })?;
    let mut route = ResponsesRoute::new(
        DEEPSEEK_PROVIDER,
        "DeepSeek",
        raw.base_url,
        credential.as_str(),
        models,
    )
    .with_api("openai-completions");
    route.generation = revision;
    route.validate()?;
    Ok(route)
}

fn openai_settings_registration(base: Value, applies: SettingsApplies) -> SettingsRegistration {
    SettingsRegistration::new(
        LLM_PI_AI_NAMESPACE,
        json!({
            "uid": 28,
            "refs": {
                "1": {"type": "string", "meta": {"required": true}},
                "2": {"type": "string", "meta": {}},
                "3": {"type": "union", "meta": {}, "list": [5, 7]},
                "5": {"type": "const", "meta": {"required": true}, "value": "text"},
                "7": {"type": "const", "meta": {"required": true}, "value": "image"},
                "8": {"type": "array", "meta": {"default": []}, "inner": 3},
                "11": {"type": "number", "meta": {"step": 1, "min": 1}},
                "14": {"type": "number", "meta": {"step": 1, "min": 1}},
                "30": {"type": "dict", "meta": {"default": {}}, "inner": 2, "sKey": 2},
                "31": {"type": "string", "meta": {}},
                "32": {"type": "string", "meta": {}},
                "33": {"type": "number", "meta": {"step": 1, "min": 0, "max": 9_007_199_254_740_991_u64}},
                "34": {"type": "array", "meta": {"default": ["EMPTY_RESPONSE", "RATE_LIMIT", "SERVER", "TIMEOUT", "TRANSPORT"]}, "inner": 2},
                "35": {"type": "number", "meta": {"min": 0.000001, "max": 2147483647}},
                "36": {"type": "number", "meta": {"min": 0, "max": 1}},
                "37": {
                    "type": "object",
                    "meta": {"default": {}},
                    "dict": {"initialDelayMs": 35, "maxDelayMs": 35, "jitterRatio": 36}
                },
                "38": {"type": "const", "meta": {"required": true}, "value": "normal"},
                "39": {"type": "const", "meta": {"required": true}, "value": "always"},
                "40": {
                    "type": "object",
                    "meta": {"default": {}},
                    "dict": {"mode": 38, "maxRetries": 33, "retryableCodes": 34, "backoff": 37}
                },
                "41": {
                    "type": "object",
                    "meta": {"default": {}},
                    "dict": {"mode": 39, "backoff": 37}
                },
                "42": {"type": "union", "meta": {}, "list": [40, 41]},
                "15": {
                    "type": "object",
                    "meta": {"default": {}},
                    "dict": {
                        "id": 1,
                        "name": 2,
                        "description": 31,
                        "input": 8,
                        "contextWindow": 11,
                        "maxTokens": 14,
                        "reasoningEfforts": 30,
                        "defaultEffort": 32
                    }
                },
                "17": {"type": "string", "meta": {"role": "credential-ref"}},
                "18": {"type": "string", "meta": {}},
                "19": {"type": "union", "meta": {}, "list": [43, 21, 44]},
                "21": {
                    "type": "const",
                    "meta": {"required": true},
                    "value": "openai-responses"
                },
                "43": {"type": "const", "meta": {"required": true}, "value": "openai-completions"},
                "44": {"type": "const", "meta": {"required": true}, "value": "anthropic-messages"},
                "22": {"type": "string", "meta": {}},
                "23": {"type": "array", "meta": {"default": []}, "inner": 15},
                "29": {"type": "boolean", "meta": {"default": false}},
                "24": {
                    "type": "object",
                    "meta": {"default": {}},
                    "dict": {
                        "apiKeyEnv": 17,
                        "displayName": 18,
                        "api": 19,
                        "baseURL": 22,
                        "models": 23,
                        "retryPolicy": 42,
                        "enabled": 29
                    }
                },
                "26": {"type": "string", "meta": {}},
                "27": {
                    "type": "dict",
                    "meta": {"default": {}},
                    "inner": 24,
                    "sKey": 26
                },
                "28": {
                    "type": "object",
                    "meta": {"default": {}},
                    "dict": {"providers": 27}
                }
            }
        }),
        json!({"providers": {}}),
        base,
    )
    .with_validator(Arc::new(|value| parse_routes(value, 0).map(|_| ())))
    .with_applies(applies)
}

fn deepseek_settings_registration(base: Value, applies: SettingsApplies) -> SettingsRegistration {
    let defaults = deepseek_default_settings();
    let mut composed_base = defaults.clone();
    if let (Value::Object(composed), Value::Object(patch)) = (&mut composed_base, &base) {
        merge_object(composed, patch);
    }
    SettingsRegistration::new(
        LLM_DEEPSEEK_NAMESPACE,
        json!({
            "uid": 20,
            "refs": {
                "1": {"type": "string", "meta": {"role": "credential-ref", "default": "DEEPSEEK_API_KEY"}},
                "2": {"type": "string", "meta": {}},
                "3": {"type": "const", "meta": {"required": true}, "value": "enabled"},
                "4": {"type": "const", "meta": {"required": true}, "value": "disabled"},
                "5": {"type": "union", "meta": {}, "list": [3, 4]},
                "6": {"type": "const", "meta": {"required": true}, "value": "off"},
                "7": {"type": "const", "meta": {"required": true}, "value": "high"},
                "8": {"type": "const", "meta": {"required": true}, "value": "max"},
                "9": {"type": "union", "meta": {}, "list": [6, 7, 8]},
                "10": {"type": "number", "meta": {"step": 1, "min": 1, "max": 9_007_199_254_740_991_u64}},
                "11": {"type": "string", "meta": {"required": true}},
                "12": {"type": "string", "meta": {}},
                "13": {
                    "type": "object",
                    "meta": {"default": {}},
                    "dict": {"id": 11, "name": 12, "description": 12, "contextWindow": 10, "maxTokens": 10}
                },
                "14": {"type": "array", "meta": {"default": []}, "inner": 13},
                "15": {"type": "number", "meta": {"min": 0.000001, "max": 2147483647}},
                "16": {"type": "object", "meta": {"default": {}}, "dict": {}},
                "20": {
                    "type": "object",
                    "meta": {"default": {}},
                    "dict": {
                        "apiKeyEnv": 1,
                        "baseURL": 2,
                        "thinking": 5,
                        "reasoningEffort": 9,
                        "maxTokens": 10,
                        "defaultContextWindow": 10,
                        "models": 14,
                        "streamIdleTimeoutMs": 15,
                        "retryPolicy": 16
                    }
                }
            }
        }),
        defaults,
        composed_base,
    )
    .with_validator(Arc::new(|value| parse_deepseek_route(value, 0).map(|_| ())))
    .with_applies(applies)
}

fn deepseek_default_settings() -> Value {
    json!({
        "apiKeyEnv": "DEEPSEEK_API_KEY",
        "baseURL": "https://api.deepseek.com",
        "thinking": "enabled",
        "reasoningEffort": "high",
        "maxTokens": 256000,
        "defaultContextWindow": 1000000,
        "models": [
            {"id": "deepseek-v4-flash", "name": "DeepSeek-V4-Flash", "contextWindow": 1000000},
            {"id": "deepseek-v4-pro", "name": "DeepSeek-V4-Pro", "contextWindow": 1000000}
        ],
        "streamIdleTimeoutMs": 300000
    })
}

fn positive_integer_settings_registration(
    namespace: &'static str,
    fields: &'static [&'static str],
    defaults: Value,
    base: Value,
) -> SettingsRegistration {
    let mut composed_base = defaults.clone();
    if let (Value::Object(composed), Value::Object(patch)) = (&mut composed_base, &base) {
        merge_object(composed, patch);
    }
    SettingsRegistration::new(
        namespace,
        json!({"type": "object", "properties": fields.iter().map(|field| ((*field).to_owned(), json!({"type": "number", "minimum": 1}))).collect::<Map<_, _>>() }),
        defaults,
        composed_base,
    )
    .with_validator(Arc::new(move |value| {
        let valid = value.as_object().is_some_and(|object| {
            object.keys().all(|field| fields.contains(&field.as_str()))
                && object.values().all(|value| value.as_u64().is_some_and(|value| value > 0))
        });
        if valid {
            Ok(())
        } else {
            Err(TessivumError::new(
                "INVALID_SETTINGS_VALUE",
                format!("{namespace} requires positive integer settings"),
                "settings",
                json!({"namespace": namespace}),
            ))
        }
    }))
}

fn web_search_settings_registration(base: Value) -> SettingsRegistration {
    SettingsRegistration::new(
        "web-search-deepseek",
        json!({
            "type": "object",
            "properties": {
                "apiKeyEnv": {"type": "string", "meta": {"role": "credential-ref"}},
                "baseURL": {"type": "string"},
                "maxUses": {"type": "number", "minimum": 1}
            }
        }),
        json!({"apiKeyEnv": "DEEPSEEK_API_KEY", "maxUses": 3}),
        base,
    )
    .with_validator(Arc::new(|value| {
        let valid = value.as_object().is_some_and(|object| {
            object
                .keys()
                .all(|field| matches!(field.as_str(), "apiKeyEnv" | "baseURL" | "maxUses"))
                && object
                    .get("apiKeyEnv")
                    .and_then(Value::as_str)
                    .is_some_and(|reference| CredentialRef::new(reference.to_owned()).is_ok())
                && object
                    .get("baseURL")
                    .is_none_or(|value| value.as_str().is_some_and(|url| !url.trim().is_empty()))
                && object
                    .get("maxUses")
                    .is_some_and(|value| value.as_u64().is_some_and(|count| count > 0))
        });
        if valid {
            Ok(())
        } else {
            Err(TessivumError::new(
                "INVALID_SETTINGS_VALUE",
                "web-search-deepseek settings are invalid",
                "settings",
                Value::Null,
            ))
        }
    }))
}

fn default_model_registration(config: &HostConfig, base: Value) -> SettingsRegistration {
    SettingsRegistration::new(
        AGENT_DEFAULT_MODEL_NAMESPACE,
        json!({"type":"object","required":["provider","model"],"additionalProperties":false}),
        json!({"provider":config.provider,"model":config.model}),
        base,
    )
    .with_validator(Arc::new(|value| {
        let selection: SessionModelSelection =
            serde_json::from_value(value.clone()).map_err(|error| {
                TessivumError::new(
                    "INVALID_MODEL_SELECTION",
                    error.to_string(),
                    "settings",
                    Value::Null,
                )
            })?;
        selection.validate()
    }))
    .with_applies(SettingsApplies::Restart)
}
fn choice_settings_registration(
    namespace: &'static str,
    field: &'static str,
    choices: &'static [&'static str],
    default: Option<&'static str>,
    base: Value,
) -> SettingsRegistration {
    let mut refs = serde_json::Map::new();
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
        json!({"type": "object", "dict": {(field): union}}),
    );
    SettingsRegistration::new(
        namespace,
        json!({"uid": object, "refs": refs}),
        default.map_or_else(|| json!({}), |value| json!({(field): value})),
        base,
    )
    .with_validator(Arc::new(move |value| {
        let valid = value.as_object().is_some_and(|object| {
            (default.is_none() && object.is_empty())
                || (object.len() == 1
                    && object
                        .get(field)
                        .and_then(Value::as_str)
                        .is_some_and(|candidate| choices.contains(&candidate)))
        });
        if valid {
            Ok(())
        } else {
            Err(TessivumError::new(
                "INVALID_SETTINGS_VALUE",
                format!("{namespace}.{field} must be one of {}", choices.join(", ")),
                "settings",
                json!({"namespace": namespace, "field": field}),
            ))
        }
    }))
}

fn latest_model_selection(events: &[SessionEvent]) -> Option<SessionModelSelection> {
    events.iter().rev().find_map(|event| {
        (event.event_type == "session/model-selected")
            .then(|| serde_json::from_value(event.data.clone()).ok())
            .flatten()
    })
}

fn selection_with_default_effort(
    routes: &BTreeMap<String, ResponsesRoute>,
    mut selection: SessionModelSelection,
) -> SessionModelSelection {
    if selection.reasoning_effort.is_none() {
        selection.reasoning_effort = routes
            .get(&selection.provider)
            .and_then(|route| {
                route
                    .models
                    .iter()
                    .find(|model| model.id == selection.model)
            })
            .and_then(|model| model.default_reasoning_effort.clone());
    }
    selection
}

fn has_model_visible_work(events: &[SessionEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event.event_type.as_str(),
            "turn/start"
                | "user/message"
                | "assistant/chunk"
                | "assistant/message"
                | "tool/call"
                | "tool/result"
        )
    })
}

fn model_group_for_route(credentials: &Arc<Credentials>, route: ResponsesRoute) -> HostModelGroup {
    let credential_configured = route.credential_ref.is_empty()
        || credential_configured(credentials, &route.credential_ref);
    let routable = credential_configured;
    HostModelGroup {
        provider: route.id.clone(),
        display_name: route.display_name.clone(),
        models: route
            .models
            .into_iter()
            .map(|model| HostModelInfo {
                provider: route.id.clone(),
                id: model.id,
                name: model.name,
                description: model.description,
                input_modalities: if model.input.is_empty() {
                    default_modalities()
                } else {
                    model.input
                },
                context_window: model.context_window,
                max_tokens: model.max_tokens,
                reasoning: (!model.reasoning_efforts.is_empty()).then(|| HostModelReasoning {
                    efforts: model
                        .reasoning_efforts
                        .into_iter()
                        .map(|effort| HostModelReasoningEffort {
                            name: effort.id[..1].to_ascii_uppercase() + &effort.id[1..],
                            id: effort.id,
                            description: None,
                        })
                        .collect(),
                    default_effort: model.default_reasoning_effort,
                }),
                routable,
            })
            .collect(),
        credential_configured,
        routable,
        failure: (!credential_configured).then(|| HostRouteFailure {
            provider: route.id,
            model: None,
            code: "MISSING_CREDENTIAL".into(),
            message: "provider credential is not configured".into(),
        }),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdapterModelRow {
    id: String,
    name: String,
    input: Vec<String>,
    context_window: u64,
    max_output: u64,
    reasoning: bool,
}

fn normalize_provider_models(
    provider: &str,
    value: Value,
) -> Result<Vec<HostProviderModel>, TessivumError> {
    let rows: Vec<AdapterModelRow> = serde_json::from_value(value).map_err(|error| {
        TessivumError::new(
            "INVALID_MODEL_DISCOVERY_RESULT",
            "provider returned malformed model rows",
            "host",
            json!({"provider": provider, "reason": error.to_string()}),
        )
    })?;
    let mut ids = BTreeSet::new();
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            if row.id.trim().is_empty() || row.name.trim().is_empty() {
                return Err(invalid_provider_model(
                    provider,
                    index,
                    "model id and name must be non-empty",
                ));
            }
            if !ids.insert(row.id.clone()) {
                return Err(invalid_provider_model(
                    provider,
                    index,
                    "model ids must be unique",
                ));
            }
            if row.context_window == 0
                || row.max_output == 0
                || row.context_window > MAX_SAFE_INTEGER
                || row.max_output > MAX_SAFE_INTEGER
            {
                return Err(invalid_provider_model(
                    provider,
                    index,
                    "contextWindow and maxOutput must be positive safe integers",
                ));
            }
            let mut modalities = BTreeSet::new();
            if row.input.iter().any(|modality| {
                !matches!(
                    modality.as_str(),
                    RESPONSES_TEXT_MODALITY | RESPONSES_IMAGE_MODALITY
                ) || !modalities.insert(modality.clone())
            }) {
                return Err(invalid_provider_model(
                    provider,
                    index,
                    "model input modalities must be unique text or image values",
                ));
            }
            Ok(HostProviderModel {
                id: row.id,
                name: row.name,
                context_window: row.context_window,
                max_output: row.max_output,
                reasoning: row.reasoning,
                input: row.input,
            })
        })
        .collect()
}

fn invalid_provider_model(provider: &str, index: usize, message: &str) -> TessivumError {
    TessivumError::new(
        "INVALID_MODEL_DISCOVERY_RESULT",
        message,
        "host",
        json!({"provider": provider, "index": index}),
    )
}

fn route_failure(
    code: &str,
    message: &str,
    selection: &SessionModelSelection,
    model: Option<String>,
) -> HostRouteFailure {
    HostRouteFailure {
        provider: selection.provider.clone(),
        model,
        code: code.into(),
        message: message.into(),
    }
}

fn model_error(code: &str, message: &str, provider: &str, model: Option<&str>) -> TessivumError {
    TessivumError::new(
        code,
        message,
        "host",
        json!({"provider": provider, "model": model}),
    )
}

fn resolve_credential_sync(
    credentials: Arc<Credentials>,
    reference: CredentialRef,
) -> Result<Option<String>, TessivumError> {
    let handle = tokio::runtime::Handle::try_current().ok();
    std::thread::spawn(move || {
        let result = if let Some(handle) = handle {
            handle.block_on(credentials.resolve(&reference))
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    crate::credentials::CredentialError::Persistence(error.to_string())
                })
                .and_then(|runtime| runtime.block_on(credentials.resolve(&reference)))
        };
        result.map_err(|error| {
            TessivumError::new(error.code(), error.to_string(), "credentials", Value::Null)
        })
    })
    .join()
    .map_err(|_| {
        TessivumError::new(
            "CREDENTIALS_RESOLVE_FAILED",
            "credential resolution thread failed",
            "credentials",
            Value::Null,
        )
    })?
}

fn adapter_for(config: &HostConfig) -> Result<Arc<dyn LlmAdapter>, HostError> {
    if config.recorded_replay_override.is_some() && config.adapter_factory.is_some() {
        return Err(HostError::InvalidConfiguration(
            "a replay override cannot be used with an adapter factory".into(),
        ));
    }
    if let Some(factory) = &config.adapter_factory {
        return Ok(factory.create(&config.provider, &config.model)?);
    }
    if let Some(replay) = &config.recorded_replay {
        return Ok(Arc::new(recorded_adapter::Adapter::new(
            replay.clone(),
            config.provider.clone(),
            config.model.clone(),
            config.recorded_replay_override.clone(),
            config.recorded_replay_pace_ms,
        )?));
    }
    if config.recorded_replay_override.is_some() {
        return Err(HostError::InvalidConfiguration(
            "a replay override requires a durable session replay".into(),
        ));
    }
    Ok(Arc::new(recorded_adapter::UnconfiguredAdapter))
}

mod recorded_adapter {
    use super::*;
    pub(super) struct Adapter {
        recording: String,
        provider: String,
        model: String,
        script: Option<Arc<RecordedLlmAdapter>>,
        routes: Mutex<BTreeMap<SessionId, Arc<RecordedLlmAdapter>>>,
    }
    impl Adapter {
        pub(super) fn new(
            recording: String,
            provider: String,
            model: String,
            override_document: Option<String>,
            pace_ms: u64,
        ) -> Result<Self, TessivumError> {
            let durable = recording
                .lines()
                .find(|line| !line.trim().is_empty())
                .and_then(|line| serde_json::from_str::<Value>(line).ok())
                .and_then(|line| line.get("type").and_then(Value::as_str).map(str::to_owned))
                .as_deref()
                == Some("session");
            let script = if durable {
                Some(Arc::new(
                    RecordedLlmAdapter::from_session_jsonls_with_override(
                        &recording,
                        &[],
                        override_document.as_deref(),
                    )?
                    .with_pace_ms(pace_ms),
                ))
            } else if override_document.is_some() {
                return Err(TessivumError::new(
                    "INVALID_LLM_REPLAY",
                    "a replay override requires a durable session JSONL recording",
                    "host",
                    Value::Null,
                ));
            } else {
                None
            };
            Ok(Self {
                recording,
                provider,
                model,
                script,
                routes: Mutex::new(BTreeMap::new()),
            })
        }
    }
    #[async_trait]
    impl LlmAdapter for Adapter {
        async fn generate(
            &self,
            request: crate::protocol::GenerateRequest,
            cancellation: tessivum_core::CancellationToken,
        ) -> Result<LlmStream, TessivumError> {
            let session = request.session_id.clone().ok_or_else(|| {
                TessivumError::new(
                    "INVALID_LLM_REQUEST",
                    "recorded host requests require a session id",
                    "host",
                    Value::Null,
                )
            })?;
            let adapter = if let Some(adapter) = &self.script {
                Arc::clone(adapter)
            } else {
                let mut routes = lock(&self.routes);
                match routes.get(&session) {
                    Some(adapter) => Arc::clone(adapter),
                    None => {
                        let adapter = Arc::new(RecordedLlmAdapter::from_jsonl_with_route(
                            &self.recording,
                            Some(session.clone()),
                            self.provider.clone(),
                            self.model.clone(),
                        )?);
                        routes.insert(session, Arc::clone(&adapter));
                        adapter
                    }
                }
            };
            adapter.generate(request, cancellation).await
        }
    }
    pub(super) struct UnconfiguredAdapter;
}
#[async_trait]
impl LlmAdapter for recorded_adapter::UnconfiguredAdapter {
    async fn generate(
        &self,
        _request: crate::protocol::GenerateRequest,
        _cancellation: tessivum_core::CancellationToken,
    ) -> Result<LlmStream, TessivumError> {
        Err(TessivumError::new(
            "LLM_ADAPTER_NOT_CONFIGURED",
            "host has no recorded replay or deployment adapter",
            "host",
            Value::Null,
        ))
    }
}

fn host_file_path(
    data_dir: &Path,
    override_path: Option<&Path>,
    default_name: &str,
    field: &str,
) -> Result<PathBuf, HostError> {
    let path = override_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| data_dir.join(default_name));
    if path.file_name().is_none() || path.is_dir() {
        return Err(HostError::InvalidConfiguration(format!(
            "{field} must name a file selected by the host"
        )));
    }
    Ok(path)
}

fn validate_config(config: &HostConfig) -> Result<(), HostError> {
    if config.profile.is_empty()
        || config.profile.len() > MAX_PROFILE_BYTES
        || !config
            .profile
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(HostError::InvalidConfiguration(
            "profile must be a bounded ASCII identifier".into(),
        ));
    }
    if config.provider.trim().is_empty()
        || config.model.trim().is_empty()
        || config.max_tokens == Some(0)
    {
        return Err(HostError::InvalidConfiguration(
            "provider/model must be nonblank and max_tokens positive".into(),
        ));
    }
    if !(1..=MAX_NOTIFICATIONS).contains(&config.notification_capacity)
        || !(1..=MAX_LIVE_SESSIONS).contains(&config.max_live_sessions)
    {
        return Err(HostError::InvalidConfiguration(
            "host capacities are outside their bounds".into(),
        ));
    }
    if config.cli_patches.len() > 64 {
        return Err(HostError::InvalidConfiguration(
            "too many CLI patch layers".into(),
        ));
    }
    if config
        .agent_preset_roots
        .iter()
        .any(|root| root.path.as_os_str().is_empty())
    {
        return Err(HostError::InvalidConfiguration(
            "agent preset root paths must not be empty".into(),
        ));
    }
    let needs_legacy = config.entries.as_ref().is_some_and(|entries| {
        entries
            .active_entries()
            .iter()
            .any(|entry| entry.options.runtime == tessivum_core::RuntimeKind::LegacyNode)
    });
    if needs_legacy && config.legacy_profile.is_none() && config.legacy_host.is_none() {
        return Err(HostError::InvalidConfiguration(
            "legacy-node entries require a Legacy profile".into(),
        ));
    }
    config
        .wasm_limits
        .validate()
        .map_err(|error| HostError::InvalidConfiguration(error.to_string()))?;
    for patch in std::iter::once(&config.bundle_patch)
        .chain(std::iter::once(&config.profile_patch))
        .chain(std::iter::once(&config.home_patch))
        .chain(config.cli_patches.iter())
        .chain(std::iter::once(&config.telemetry_patch))
    {
        if !patch.is_object() || json_size(patch) > MAX_FRAME_BYTES {
            return Err(HostError::InvalidConfiguration(
                "patches must be bounded JSON objects".into(),
            ));
        }
    }
    Ok(())
}

fn validate_prompt(params: &SessionPromptParams) -> Result<(), HostError> {
    validate_session(&params.session_id)?;
    params.validate()?;
    if params.content_blocks.is_empty()
        || params.content_blocks.len() > MAX_PROMPT_BLOCKS
        || json_size(&params.content_blocks) > MAX_FRAME_BYTES
    {
        return Err(HostError::invalid(
            "INVALID_SESSION_PROMPT",
            "contentBlocks must be a bounded non-empty array",
        ));
    }
    Ok(())
}

fn validate_session(session: &SessionId) -> Result<(), HostError> {
    if session.as_str().is_empty() || session.as_str().len() > MAX_PROFILE_BYTES {
        Err(HostError::invalid(
            "INVALID_SESSION_ID",
            "session id is invalid",
        ))
    } else {
        Ok(())
    }
}

async fn append_fallback_session_title(
    session: &Session,
    message_id: &MessageId,
    cancellation: tessivum_core::CancellationToken,
) -> Result<(), HostError> {
    let events = session.events();
    if events
        .iter()
        .any(|event| event.event_type == "session/title")
    {
        return Ok(());
    }
    let Some(message) = events.iter().find(|event| {
        event.event_type == "user/message"
            && event.data.get("id").and_then(Value::as_str) == Some(message_id.as_str())
            && event.data.pointer("/source/kind").and_then(Value::as_str) == Some("user")
    }) else {
        return Ok(());
    };
    let text = message
        .data
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let Ok(normalized) = normalize_session_title(&text) else {
        return Ok(());
    };
    let mut title = normalized
        .split_whitespace()
        .take(5)
        .collect::<Vec<_>>()
        .join(" ");
    while title.len() > 40 {
        title.pop();
    }
    let message_seq = message.seq;
    session
        .append_next(
            move |seq| SessionEvent {
                event_type: "session/title".into(),
                seq,
                time: now(),
                data: json!({
                    "title": title.trim_end(),
                    "messageSeqs": [message_seq],
                    "source": { "kind": "fallback" },
                }),
                ignorable: None,
                source_event_seqs: None,
                surface_op: None,
            },
            cancellation,
        )
        .await?;
    Ok(())
}

fn normalize_session_title(value: &str) -> Result<String, HostError> {
    let mut normalized = String::new();
    let mut whitespace = true;
    for character in value.chars() {
        if matches!(character, '\u{0000}'..='\u{0008}' | '\u{000B}'..='\u{000C}' | '\u{000E}'..='\u{001F}' | '\u{007F}'..='\u{009F}' | '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' | '\u{FEFF}')
        {
            continue;
        }
        if character.is_whitespace() {
            if !whitespace {
                normalized.push(' ');
            }
            whitespace = true;
        } else {
            normalized.push(character);
            whitespace = false;
        }
    }
    let normalized = normalized.trim_end();
    let mut title = String::new();
    for character in normalized.chars() {
        if title.len() + character.len_utf8() > 80 {
            break;
        }
        title.push(character);
    }
    if title.is_empty() {
        Err(HostError::invalid(
            "TITLE_INVALID",
            "session title must contain visible text",
        ))
    } else {
        Ok(title)
    }
}

fn fork_seed(events: &[SessionEvent]) -> Result<Vec<SessionEvent>, HostError> {
    let mut sequences = BTreeMap::new();
    for event in events {
        if event.event_type != "session/end-seed" {
            let sequence = u64::try_from(sequences.len()).map_err(|_| {
                HostError::invalid("FORK_UNAVAILABLE", "fork seed exceeds sequence capacity")
            })?;
            sequences.insert(event.seq, sequence);
        }
    }
    let mut seed = Vec::with_capacity(sequences.len());
    for event in events {
        let Some(sequence) = sequences.get(&event.seq) else {
            continue;
        };
        let mut event = event.clone();
        event.seq = *sequence;
        if let Some(sources) = &event.source_event_seqs {
            let mapped = sources
                .iter()
                .filter_map(|source| sequences.get(source).copied())
                .collect::<Vec<_>>();
            event.source_event_seqs = (!mapped.is_empty() || sources.is_empty()).then_some(mapped);
        }
        seed.push(event);
    }
    Ok(seed)
}

fn merge_object(base: &mut Map<String, Value>, patch: &Map<String, Value>) {
    for (key, value) in patch {
        match (base.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(patch)) => merge_object(existing, patch),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn relay_missing_events(
    inner: &HostInner,
    session: &crate::session::Session,
    session_id: &SessionId,
    next_seq: &mut u64,
) {
    let start = *next_seq;
    for event in session
        .events()
        .into_iter()
        .filter(|event| event.seq >= start)
    {
        *next_seq = event.seq.saturating_add(1);
        relay_event(inner, session_id, event);
    }
}

fn relay_event(inner: &HostInner, session_id: &SessionId, event: SessionEvent) {
    if let Some(telemetry) = &inner.telemetry {
        telemetry.capture_event(session_id, &event);
    }
    let _ = inner
        .notices
        .send(HostNotification::SessionEvent(SessionEventNotification {
            session_id: session_id.clone(),
            event: event.clone(),
        }));
    if inner.projections.apply(session_id, &event).is_ok() {
        if let Ok(snapshots) = inner.projections.snapshots(session_id, None) {
            for snapshot in snapshots
                .into_iter()
                .filter(|snapshot| snapshot.as_of_seq == Some(event.seq))
            {
                let _ = inner.notices.send(HostNotification::SessionProjection(
                    HostSessionProjectionNotification {
                        session_id: session_id.clone(),
                        key: snapshot.key,
                        value: snapshot.view,
                        seq: event.seq,
                    },
                ));
            }
        }
    }
    match event.event_type.as_str() {
        "subagent/contained-start" => {
            let Some(descriptor) = event
                .data
                .get("child")
                .cloned()
                .and_then(|data| serde_json::from_value::<SubagentDescriptor>(data).ok())
                .filter(|descriptor| descriptor.parent_session_id == *session_id)
            else {
                return;
            };
            let child_session_id = descriptor.child_session_id.clone();
            let status =
                inner
                    .registry
                    .get(&child_session_id)
                    .map_or(SessionStatus::Idle, |agent| match agent.status() {
                        AgentStatus::Idle => SessionStatus::Idle,
                        AgentStatus::Running => SessionStatus::Running,
                    });
            let mut state = lock(&inner.state);
            state.subagents.insert(child_session_id.clone(), descriptor);
            state.statuses.insert(child_session_id.clone(), status);
            let _ = inner.notices.send(HostNotification::SubagentStarted(
                SubagentStartedNotification {
                    parent_session_id: session_id.clone(),
                    child_session_id: child_session_id.clone(),
                },
            ));
            let _ =
                inner
                    .notices
                    .send(HostNotification::SessionStatus(SessionStatusNotification {
                        session_id: child_session_id,
                        status,
                    }));
        }
        "subagent/contained-end" => {
            let Some(child_session_id) = event
                .data
                .get("childSessionId")
                .and_then(Value::as_str)
                .map(SessionId::from)
            else {
                return;
            };
            let Some(descriptor) = lock(&inner.state).subagents.remove(&child_session_id) else {
                return;
            };
            let child_status = event
                .data
                .get("status")
                .cloned()
                .and_then(|status| serde_json::from_value::<SubagentRunStatus>(status).ok())
                .unwrap_or(SubagentRunStatus::Error);
            let (status, stop_reason) = match child_status {
                SubagentRunStatus::Completed => (SdkRunStatus::Ok, "completed"),
                SubagentRunStatus::Cancelled => (SdkRunStatus::Error, "cancelled"),
                SubagentRunStatus::Error => (SdkRunStatus::Error, "error"),
            };
            let last_assistant_message = event
                .data
                .get("lastAssistantMessage")
                .cloned()
                .and_then(|message| serde_json::from_value(message).ok());
            let _ = inner.notices.send(HostNotification::SubagentFinished(
                SubagentFinishedNotification {
                    provider: descriptor.provider,
                    agent_id: descriptor.agent_id,
                    parent_session_id: session_id.clone(),
                    child_session_id: child_session_id.clone(),
                    status,
                    stop_reason: stop_reason.into(),
                    last_assistant_message,
                },
            ));
            lock(&inner.state)
                .statuses
                .insert(child_session_id.clone(), SessionStatus::Idle);
            let _ =
                inner
                    .notices
                    .send(HostNotification::SessionStatus(SessionStatusNotification {
                        session_id: child_session_id,
                        status: SessionStatus::Idle,
                    }));
        }
        "approval/asked" => {
            if let Ok(asked) = serde_json::from_value::<ApprovalAsked>(event.data.clone()) {
                inner.approvals.observe_asked(&asked);
            }
        }
        "approval/decided" => {
            if let Ok(decision) = serde_json::from_value::<ApprovalDecision>(event.data.clone()) {
                inner.approvals.observe_decided(session_id, &decision);
            }
        }
        "turn/end" => {
            if let Some(turn) = event.data.get("turn").and_then(Value::as_u64) {
                inner.approvals.cancel_turn(session_id, turn);
                inner.questions.cancel_turn(session_id, turn);
            }
        }
        _ => {}
    }
}

fn json_size(value: &impl serde::Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |value| value.len())
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| value.as_millis().try_into().unwrap_or(u64::MAX))
}
fn preset_error(error: Value) -> HostError {
    let (code, message, details) = crate::agent_preset::error_parts(&error);
    HostError::Runtime(TessivumError::new(code, message, "agent-preset", details))
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) fn inbox_enqueued_event(
    seq: u64,
    target: InboxTarget,
    message: &Message,
) -> SessionEvent {
    SessionEvent {
        event_type: "agent/inbox/enqueued".into(),
        seq,
        time: now(),
        data: json!({
            "target": queue_target_name(target),
            "message": message,
        }),
        ignorable: Some(true),
        source_event_seqs: None,
        surface_op: None,
    }
}

fn queue_target_name(target: InboxTarget) -> &'static str {
    match target {
        InboxTarget::Followup => "next-turn",
        InboxTarget::Steer | InboxTarget::Inject => "next-step",
    }
}

fn queue_error(code: &str, message: &str, item_id: &MessageId) -> HostError {
    HostError::Runtime(TessivumError::new(
        code,
        message,
        "queue",
        json!({"itemId": item_id}),
    ))
}

fn publish_queue(inner: &HostInner, session_id: SessionId, inbox: &crate::agent::Inbox) {
    let items = inbox
        .pending()
        .into_iter()
        .map(|(target, message)| HostSessionQueueItem {
            id: message.id.clone(),
            placement: match target {
                InboxTarget::Followup => "queued",
                InboxTarget::Steer => "steering",
                InboxTarget::Inject => "context",
            }
            .into(),
            message,
        })
        .collect();
    let _ = inner.notices.send(HostNotification::SessionQueue(
        HostSessionQueueNotification { session_id, items },
    ));
}
fn host_subagent_error(error: SubagentError) -> HostError {
    HostError::Runtime(TessivumError::new(
        error.code(),
        error.to_string(),
        "subagent",
        Value::Null,
    ))
}

use std::{
    env, fs,
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    process::{self, ExitCode},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use clap::error::ErrorKind;
use serde::Deserialize;
use serde_json::Value;
use tessivum::{
    agent_preset::AgentPresetTrust,
    boot_theme::inject_boot_theme,
    cli::{parse_cli, CliCommand, ExitClass, HeadlessCommand, PluginAction, PluginCommand},
    code_runtime::{ProcessCodeRuntime, ProcessCodeRuntimeConfig},
    frontend::{FrontendHtmlTap, FrontendStatic, FrontendTapRegistration},
    headless::{run_headless, run_headless_with_adapter, HeadlessConfig},
    host::{shutdown_signal, HostApi, HostConfig, HostRuntime},
    llm::LlmAdapter,
    openai_responses::OpenAiResponsesAdapter,
    plugin_manager::{configure_host_plugins, mutate_plugins, PluginMutation},
    settings::Settings,
    SessionId,
};

const PROCESS_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
static EMBEDDED_WEB_INSTANCE: AtomicU64 = AtomicU64::new(0);
const WEB_FILE_REFERENCE_GUIDANCE: &str = concat!(
    "When you successfully create or modify files, mention the primary outputs in your final response. ",
    "To make those and any other changed-file references clickable in Web, format them as Markdown ",
    "inline code using the exact file-tool path, or a basename when unique among the files changed in that turn.",
);

fn web_system_prompt(address: SocketAddr) -> String {
    format!(
        "You are interacting with the user through the Tessivum Web GUI at http://{address}. \
         When the user refers to \"this page\", \"this GUI\", or \"this app\" without naming another target, \
         they mean this GUI. The browser provides no implicit DOM, route, or screenshot context.\n\n\
         {WEB_FILE_REFERENCE_GUIDANCE}"
    )
}

struct EmbeddedAsset {
    path: &'static str,
    bytes: &'static [u8],
}

mod embedded_web_assets {
    include!(concat!(env!("OUT_DIR"), "/embedded_web_assets.rs"));
}

struct EmbeddedWebAssets {
    root: PathBuf,
}

impl EmbeddedWebAssets {
    fn materialize(include_dist: bool, include_packages: bool) -> Result<Self, Diagnostic> {
        let mut root = None;
        for _ in 0..64 {
            let candidate = env::temp_dir().join(format!(
                "tessivum-web-{}-{}",
                process::id(),
                EMBEDDED_WEB_INSTANCE.fetch_add(1, Ordering::Relaxed),
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => {
                    root = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(Diagnostic::runtime(
                        "WEB_ASSET_MATERIALIZATION_FAILED",
                        error,
                    ))
                }
            }
        }
        let root = root.ok_or_else(|| {
            Diagnostic::runtime(
                "WEB_ASSET_MATERIALIZATION_FAILED",
                "could not allocate a private temporary web asset directory",
            )
        })?;
        let result = (|| -> std::io::Result<()> {
            for asset in embedded_web_assets::ASSETS {
                if !(include_dist && asset.path.starts_with("dist/")
                    || include_packages && asset.path.starts_with("client-packages/"))
                {
                    continue;
                }
                let path = root.join(asset.path);
                fs::create_dir_all(
                    path.parent()
                        .expect("embedded assets have parent directories"),
                )?;
                fs::write(path, asset.bytes)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&root);
            return Err(Diagnostic::runtime(
                "WEB_ASSET_MATERIALIZATION_FAILED",
                error,
            ));
        }
        Ok(Self { root })
    }

    fn dist_root(&self) -> PathBuf {
        self.root.join("dist")
    }

    fn package_root(&self) -> PathBuf {
        self.root.join("client-packages")
    }
}

impl Drop for EmbeddedWebAssets {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

async fn shutdown_with_escalation<T>(shutdown: impl Future<Output = T>, exit_code: i32) -> T {
    match tokio::time::timeout(PROCESS_SHUTDOWN_TIMEOUT, shutdown).await {
        Ok(result) => result,
        Err(_) => {
            eprintln!("Tessivum shutdown exceeded 5 seconds; forcing exit");
            process::exit(exit_code);
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(diagnostic) => {
            eprintln!("{}: {}", diagnostic.code, diagnostic.message);
            ExitCode::from(diagnostic.class.code() as u8)
        }
    }
}

async fn run() -> Result<(), Diagnostic> {
    let command = match parse_cli(env::args_os()) {
        Ok(cli) => cli.command,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            error
                .print()
                .map_err(|error| Diagnostic::runtime("CLI_DISPLAY_FAILED", error))?;
            return Ok(());
        }
        Err(error) => return Err(Diagnostic::usage(error.to_string())),
    };

    match command {
        CliCommand::Headless(command) => run_headless_command(command).await,
        CliCommand::Web(command) => run_web(command).await,
        CliCommand::Plugin(command) => run_plugin_command(command),
        CliCommand::Sdk => run_sdk().await,
        // Keep plugin inspection isolated in the existing dedicated binary.
        CliCommand::PluginReport => Err(Diagnostic::runtime(
            "PLUGIN_REPORT_BINARY",
            "run the existing plugin_report binary for package inspection",
        )),
    }
}

fn run_plugin_command(command: PluginCommand) -> Result<(), Diagnostic> {
    let mutation = match command.action {
        PluginAction::Add(specifier) => PluginMutation::Add(specifier),
        PluginAction::Remove(package) => PluginMutation::Remove(package),
    };
    mutate_plugins(command.data_dir, mutation)
        .map_err(|error| Diagnostic::runtime("PLUGIN_MANAGEMENT_FAILED", error))
}

async fn run_headless_command(command: HeadlessCommand) -> Result<(), Diagnostic> {
    if !command.patches.is_empty() {
        return Err(Diagnostic::usage(
            "--patch overlays are only supported by the web profile",
        ));
    }
    let (config, task) = config(command).await?;
    let adapter = config
        .replay_jsonl
        .is_empty()
        .then(|| live_adapter(&config.provider))
        .transpose()?;
    let execution = async move {
        match adapter {
            Some(adapter) => run_headless_with_adapter(config, task, adapter).await,
            None => run_headless(config, task).await,
        }
    };
    let result = tokio::select! {
        result = execution => result.map_err(|error| {
            if error.code() == "CANCELLED" {
                Diagnostic::cancelled(error.to_string())
            } else {
                Diagnostic::runtime(error.code(), error.to_string())
            }
        }),
        signal = tokio::signal::ctrl_c() => {
            match signal {
                Ok(()) => Err(Diagnostic::cancelled("interrupted")),
                Err(error) => Err(Diagnostic::runtime("SIGNAL_FAILED", error)),
            }
        },
    }?;
    println!("{}", result.final_text);
    Ok(())
}

async fn run_web(command: tessivum::cli::WebCommand) -> Result<(), Diagnostic> {
    let dynamic_cordis = feature_flag("TESSIVUM_CORDIS_TOOLS")?;
    let address = env::var("TESSIVUM_WEB_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse::<SocketAddr>()
        .map_err(|error| Diagnostic::usage(format!("invalid TESSIVUM_WEB_ADDR: {error}")))?;
    env::set_var("DSH_WEB_URL", format!("http://{address}"));
    let trusted_authorities = environment("TESSIVUM_WEB_TRUSTED_AUTHORITIES")?
        .map(|authorities| authorities.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    if let Some(dist) = env::var_os("TESSIVUM_WEB_DIST") {
        FrontendStatic::new(PathBuf::from(dist))
            .map_err(|error| Diagnostic::runtime("WEB_FRONTEND_FAILED", error))?;
    }
    let patches = load_cli_patches(&command.patches).await?;
    let runtime = boot_host(
        web_replay().await?,
        dynamic_cordis,
        true,
        Some(web_system_prompt(address)),
        patches,
    )
    .await?;
    let (frontend, _theme_tap, _embedded_assets) = match web_frontend(
        runtime
            .handle()
            .settings()
            .expect("a booted Host always publishes settings"),
    ) {
        Ok(frontend) => frontend,
        Err(error) => {
            let _ = shutdown_with_escalation(runtime.shutdown(), ExitClass::Runtime.code()).await;
            return Err(error);
        }
    };
    let host: Arc<dyn HostApi> = Arc::new(runtime.handle());
    let mut server = match tessivum::api::ApiServer::bind_with_trusted_authorities(
        host,
        tessivum::api::ApiServerConfig {
            bind_addr: address,
            frontend: Some(frontend),
        },
        trusted_authorities,
    )
    .await
    {
        Ok(server) => server,
        Err(error) => {
            let _ = shutdown_with_escalation(runtime.shutdown(), ExitClass::Runtime.code()).await;
            return Err(Diagnostic::runtime("WEB_BIND_FAILED", error));
        }
    };
    eprintln!("Tessivum web listening at http://{}", server.local_addr());
    let signal = match shutdown_signal().await {
        Ok(signal) => signal,
        Err(error) => {
            let _ = shutdown_with_escalation(
                async {
                    let _ = server.shutdown().await;
                    let _ = runtime.shutdown().await;
                },
                ExitClass::Runtime.code(),
            )
            .await;
            return Err(Diagnostic::runtime("SIGNAL_FAILED", error));
        }
    };
    let shutdown_exit = if signal == 130 { 130 } else { 0 };
    let (server_result, host_result) = shutdown_with_escalation(
        async {
            let server_result = server
                .shutdown()
                .await
                .map_err(|error| Diagnostic::runtime("WEB_SHUTDOWN_FAILED", error));
            let host_result = runtime
                .shutdown()
                .await
                .map_err(|error| Diagnostic::runtime(error.code().to_owned(), error));
            (server_result, host_result)
        },
        shutdown_exit,
    )
    .await;
    server_result?;
    host_result?;
    if signal == 130 {
        Err(Diagnostic::cancelled("interrupted"))
    } else {
        Ok(())
    }
}

fn web_frontend(
    settings: Arc<Settings>,
) -> Result<
    (
        FrontendStatic,
        FrontendTapRegistration,
        Option<EmbeddedWebAssets>,
    ),
    Diagnostic,
> {
    let dist_override = env::var_os("TESSIVUM_WEB_DIST").map(PathBuf::from);
    let package_override = env::var_os("TESSIVUM_CLIENT_PACKAGES");
    let embedded = (dist_override.is_none() || package_override.is_none())
        .then(|| {
            EmbeddedWebAssets::materialize(dist_override.is_none(), package_override.is_none())
        })
        .transpose()?;
    let dist = dist_override.unwrap_or_else(|| {
        embedded
            .as_ref()
            .expect("embedded assets exist without TESSIVUM_WEB_DIST")
            .dist_root()
    });
    let frontend = if package_override.is_some() {
        FrontendStatic::new_with_hmr(&dist, 16)
    } else {
        FrontendStatic::new(&dist)
    }
    .map_err(|error| Diagnostic::runtime("WEB_FRONTEND_FAILED", error))?;
    let tap = frontend
        .register_tap(FrontendHtmlTap::new(
            "ui-theme-bootstrap",
            -1000,
            move |html| {
                let preference = settings
                    .get("ui-theme")
                    .ok()
                    .and_then(|snapshot| {
                        snapshot
                            .value
                            .get("preference")?
                            .as_str()
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "system".into());
                inject_boot_theme(&html, &preference)
            },
        ))
        .map_err(|error| Diagnostic::runtime("WEB_FRONTEND_FAILED", error))?;
    let mut package_roots = package_override.map_or_else(
        || {
            vec![embedded
                .as_ref()
                .expect("embedded assets exist without TESSIVUM_CLIENT_PACKAGES")
                .package_root()]
        },
        |paths| env::split_paths(&paths).collect(),
    );
    let installed = PathBuf::from(".tessivum/plugins/node_modules");
    if installed.is_dir() {
        package_roots.push(installed);
    }
    frontend
        .scan_packages(package_roots)
        .map_err(|error| Diagnostic::runtime("WEB_CLIENT_PACKAGES_FAILED", error))?;
    Ok((frontend, tap, embedded))
}

fn feature_flag(name: &str) -> Result<bool, Diagnostic> {
    match environment(name)?.as_deref() {
        None | Some("0" | "false" | "off") => Ok(false),
        Some("1" | "true" | "on") => Ok(true),
        Some(value) => Err(Diagnostic::usage(format!(
            "{name} must be 0/1, false/true, or off/on; got {value}"
        ))),
    }
}

enum SdkOutcome {
    Eof,
    Signal(i32),
}

async fn run_sdk() -> Result<(), Diagnostic> {
    let runtime = boot_host(None, false, true, None, Vec::new()).await?;
    let server = tessivum::sdk::JsonRpcServer::new(Arc::new(runtime.handle()));
    let reader = tokio::io::stdin();
    let writer = tokio::io::stdout();
    let outcome = tokio::select! {
        result = server.serve(reader, writer) => result
            .map(|()| SdkOutcome::Eof)
            .map_err(|error| Diagnostic::runtime("SDK_RUNTIME_FAILED", error)),
        signal = shutdown_signal() => signal
            .map(SdkOutcome::Signal)
            .map_err(|error| Diagnostic::runtime("SIGNAL_FAILED", error)),
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = shutdown_with_escalation(runtime.shutdown(), ExitClass::Runtime.code()).await;
            return Err(error);
        }
    };
    let shutdown_exit = match outcome {
        SdkOutcome::Signal(130) => 130,
        SdkOutcome::Eof | SdkOutcome::Signal(0) => 0,
        SdkOutcome::Signal(_) => ExitClass::Runtime.code(),
    };
    shutdown_with_escalation(runtime.shutdown(), shutdown_exit)
        .await
        .map_err(|error| Diagnostic::runtime(error.code().to_owned(), error))?;
    match outcome {
        SdkOutcome::Eof | SdkOutcome::Signal(0) => Ok(()),
        SdkOutcome::Signal(130) => Err(Diagnostic::cancelled("interrupted")),
        SdkOutcome::Signal(_) => Err(Diagnostic::runtime(
            "SIGNAL_FAILED",
            "unknown signal result",
        )),
    }
}

async fn boot_host(
    recorded_replay: Option<WebReplay>,
    dynamic_cordis: bool,
    enable_trusted_bash: bool,
    system_prompt: Option<String>,
    cli_patches: Vec<Value>,
) -> Result<HostRuntime, Diagnostic> {
    let cwd =
        env::current_dir().map_err(|error| Diagnostic::runtime("CWD_RESOLUTION_FAILED", error))?;
    let mut config = HostConfig::new(cwd.clone(), cwd.join(".tessivum"));
    config.enable_trusted_bash = enable_trusted_bash;
    config.system_prompt = system_prompt;
    config.dynamic_cordis = dynamic_cordis;
    config.cli_patches = cli_patches;
    match environment("TESSIVUM_TOOLS_MODE")?.as_deref() {
        None | Some("native") => {}
        Some("code") => {
            config.enable_trusted_bash = true;
            config.code_runtime = Some(
                ProcessCodeRuntime::new(ProcessCodeRuntimeConfig::javascript("node"))
                    .map_err(|error| Diagnostic::runtime("CODE_RUNTIME_CONFIG_FAILED", error))?,
            );
        }
        Some(mode) => {
            return Err(Diagnostic::usage(format!(
                "TESSIVUM_TOOLS_MODE must be native or code, got {mode}"
            )))
        }
    }
    if let Some(root) = environment("TESSIVUM_AGENT_PRESET_ROOT")? {
        config = config.with_agent_preset_root(root, AgentPresetTrust::System);
    }
    if let Some(replay) = recorded_replay {
        let route = replay_route(&replay.recording);
        config = config
            .with_recorded_replay(replay.recording)
            .with_recorded_replay_pace_ms(replay.pace_ms);
        config.recorded_replay_context_window = replay.context_window;
        if let Some(override_document) = replay.override_document {
            config = config.with_recorded_replay_override(override_document);
        }
        if let Some((provider, model)) = route {
            config.provider = if provider == "deepseek" {
                "deepseek-official".into()
            } else {
                provider
            };
            config.model = model;
        }
    } else if let Some(deployment) = deployment_from_env()? {
        config.provider = deployment.provider.clone();
        config.model = deployment.model.clone();
        config.profile_patch = serde_json::json!({
            "llm-pi-ai": {
                "providers": {
                    deployment.provider: {
                        "displayName": "OpenAI Responses",
                        "api": "openai-responses",
                        "baseURL": deployment.base_url,
                        "apiKeyEnv": "OPENAI_API_KEY",
                        "models": [{"id": deployment.model, "input": ["text"]}]
                    }
                }
            }
        });
    } else {
        // Web/SDK start providerless; settings can install the first live route.
        config.provider = "openai-responses".into();
        config.model = "unconfigured".into();
    }
    configure_host_plugins(&mut config)
        .map_err(|error| Diagnostic::runtime("PLUGIN_PROFILE_FAILED", error))?;
    HostRuntime::boot(config)
        .await
        .map_err(|error| Diagnostic::runtime(error.code().to_owned(), error))
}

struct Deployment {
    provider: String,
    model: String,
    base_url: String,
}

fn deployment_from_env() -> Result<Option<Deployment>, Diagnostic> {
    let Some(model) = environment("OPENAI_MODEL")? else {
        return Ok(None);
    };
    if model.trim().is_empty() {
        return Err(Diagnostic::usage("OPENAI_MODEL must not be empty"));
    }
    let provider =
        environment("TESSIVUM_LLM_PROVIDER")?.unwrap_or_else(|| "openai-responses".into());
    if provider.trim().is_empty() {
        return Err(Diagnostic::usage("TESSIVUM_LLM_PROVIDER must not be empty"));
    }
    Ok(Some(Deployment {
        provider,
        model,
        base_url: environment("OPENAI_BASE_URL")?
            .unwrap_or_else(|| "https://api.openai.com/v1".into()),
    }))
}

fn live_adapter(provider: &str) -> Result<Arc<dyn LlmAdapter>, Diagnostic> {
    if provider != "openai-responses" {
        return Err(Diagnostic::usage(format!(
            "live provider {provider:?} is not installed; use openai-responses"
        )));
    }
    openai_adapter_from_env()
}

fn openai_adapter_from_env() -> Result<Arc<dyn LlmAdapter>, Diagnostic> {
    let api_key = environment("OPENAI_API_KEY")?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| Diagnostic::usage("OPENAI_API_KEY is required"))?;
    let base_url =
        environment("OPENAI_BASE_URL")?.unwrap_or_else(|| "https://api.openai.com/v1".into());
    OpenAiResponsesAdapter::new(&base_url, &api_key)
        .map(|adapter| Arc::new(adapter) as Arc<dyn LlmAdapter>)
        .map_err(|error| Diagnostic::usage(error.to_string()))
}

fn environment(name: &str) -> Result<Option<String>, Diagnostic> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            Err(Diagnostic::usage(format!("{name} must be valid Unicode")))
        }
    }
}
struct WebReplay {
    recording: String,
    override_document: Option<String>,
    pace_ms: u64,
    context_window: Option<u64>,
}

fn replay_route(recording: &str) -> Option<(String, String)> {
    recording
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|row| {
            let config = row.get("data")?.get("header")?.get("config")?;
            let provider = config.get("provider")?.as_str()?.trim();
            let model = config.get("model")?.as_str()?.trim();
            (!provider.is_empty() && !model.is_empty()).then(|| (provider.into(), model.into()))
        })
}

async fn web_replay() -> Result<Option<WebReplay>, Diagnostic> {
    let recording = match (
        environment("TESSIVUM_REPLAY")?,
        environment("TESSIVUM_REPLAY_FILE")?,
    ) {
        (Some(_), Some(_)) => {
            return Err(Diagnostic::usage(
                "TESSIVUM_REPLAY and TESSIVUM_REPLAY_FILE are mutually exclusive",
            ));
        }
        (Some(replay), None) => Some(replay),
        (None, Some(path)) => Some(
            tokio::fs::read_to_string(path)
                .await
                .map_err(|error| Diagnostic::runtime("REPLAY_READ_FAILED", error))?,
        ),
        (None, None) => None,
    };
    let override_path = environment("TESSIVUM_REPLAY_OVERRIDE_FILE")?;
    let pace = environment("TESSIVUM_REPLAY_PACE_MS")?;
    let context_window = environment("TESSIVUM_REPLAY_CONTEXT_WINDOW")?;
    let Some(recording) = recording else {
        if override_path.is_some() || pace.is_some() || context_window.is_some() {
            return Err(Diagnostic::usage(
                "TESSIVUM_REPLAY_OVERRIDE_FILE, TESSIVUM_REPLAY_PACE_MS, and TESSIVUM_REPLAY_CONTEXT_WINDOW require a replay",
            ));
        }
        return Ok(None);
    };
    let override_document = match override_path {
        Some(path) => Some(
            tokio::fs::read_to_string(path)
                .await
                .map_err(|error| Diagnostic::runtime("REPLAY_OVERRIDE_READ_FAILED", error))?,
        ),
        None => None,
    };
    let pace_ms = match pace {
        Some(value) => value.parse::<u64>().map_err(|error| {
            Diagnostic::usage(format!(
                "TESSIVUM_REPLAY_PACE_MS must be a non-negative integer: {error}"
            ))
        })?,
        None => 0,
    };
    let context_window = context_window
        .map(|value| {
            value
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    Diagnostic::usage("TESSIVUM_REPLAY_CONTEXT_WINDOW must be a positive integer")
                })
        })
        .transpose()?;
    Ok(Some(WebReplay {
        recording,
        override_document,
        pace_ms,
        context_window,
    }))
}

async fn load_cli_patches(paths: &[PathBuf]) -> Result<Vec<Value>, Diagnostic> {
    let mut patches = Vec::with_capacity(paths.len());
    for path in paths {
        let document = tokio::fs::read_to_string(path)
            .await
            .map_err(|error| Diagnostic::runtime("PATCH_READ_FAILED", error))?;
        let mut documents = serde_yaml::Deserializer::from_str(&document);
        let document = documents.next().ok_or_else(|| {
            Diagnostic::usage(format!(
                "patch {} must contain one YAML document",
                path.display()
            ))
        })?;
        let patch = Value::deserialize(document).map_err(|error| {
            Diagnostic::usage(format!(
                "could not parse YAML patch {}: {error}",
                path.display()
            ))
        })?;
        if documents.next().is_some() {
            return Err(Diagnostic::usage(format!(
                "patch {} must contain exactly one YAML document",
                path.display()
            )));
        }
        if !patch.is_object() {
            return Err(Diagnostic::usage(format!(
                "patch {} must be a YAML mapping that converts to a JSON object",
                path.display()
            )));
        }
        patches.push(patch);
    }
    Ok(patches)
}

async fn config(command: HeadlessCommand) -> Result<(HeadlessConfig, String), Diagnostic> {
    let cwd =
        env::current_dir().map_err(|error| Diagnostic::runtime("CWD_RESOLUTION_FAILED", error))?;
    let data_dir = command.data_dir.map_or_else(
        || cwd.join(".tessivum"),
        |path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        },
    );
    let replay_jsonl = match command.replay {
        Some(path) => tokio::fs::read_to_string(path)
            .await
            .map_err(|error| Diagnostic::runtime("REPLAY_READ_FAILED", error))?,
        None => String::new(),
    };
    let session_id = command
        .session
        .map(SessionId::from)
        .unwrap_or_else(SessionId::random);

    Ok((
        HeadlessConfig {
            data_dir,
            cwd,
            session_id,
            resume: command.resume,
            provider: command.provider,
            model: command.model,
            max_tokens: command.max_tokens.map(|tokens| tokens.get()),
            replay_jsonl,
            enable_trusted_bash: command.trusted_bash,
            system_prompt: None,
        },
        command.task,
    ))
}

#[derive(Debug)]
struct Diagnostic {
    class: ExitClass,
    code: String,
    message: String,
}

impl Diagnostic {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            class: ExitClass::Usage,
            code: "USAGE".into(),
            message: diagnostic_message(message.into()),
        }
    }

    fn runtime(code: impl Into<String>, error: impl std::fmt::Display) -> Self {
        Self {
            class: ExitClass::Runtime,
            code: code.into(),
            message: diagnostic_message(error),
        }
    }

    fn cancelled(message: impl Into<String>) -> Self {
        Self {
            class: ExitClass::Cancelled,
            code: "CANCELLED".into(),
            message: diagnostic_message(message.into()),
        }
    }
}

fn diagnostic_message(message: impl std::fmt::Display) -> String {
    let message = message.to_string();
    let line = message.lines().next().unwrap_or("unknown error").trim();
    line.strip_prefix("error: ").unwrap_or(line).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn patch_dir(name: &str) -> PathBuf {
        let root = env::temp_dir().join(format!(
            "tessivum-cli-{name}-{}-{}",
            process::id(),
            EMBEDDED_WEB_INSTANCE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).expect("patch fixture directory creates");
        root
    }

    #[tokio::test]
    async fn yaml_patch_overlays_preserve_order() {
        let root = patch_dir("ordered-patches");
        let base = root.join("base.yml");
        let local = root.join("local.yml");
        fs::write(&base, "ui-theme:\n  preference: light\n").expect("base patch writes");
        fs::write(&local, "ui-theme:\n  preference: dark\n").expect("local patch writes");

        let patches = load_cli_patches(&[base, local])
            .await
            .expect("YAML mappings load");

        assert_eq!(
            patches,
            vec![
                json!({"ui-theme": {"preference": "light"}}),
                json!({"ui-theme": {"preference": "dark"}}),
            ]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn yaml_patch_rejects_non_object_documents() {
        let root = patch_dir("invalid-patch");
        let patch = root.join("invalid.yml");
        fs::write(&patch, "- not\n- a mapping\n").expect("invalid patch writes");

        let error = load_cli_patches(&[patch])
            .await
            .expect_err("array patch must fail before host boot");

        assert_eq!(error.class, ExitClass::Usage);
        assert!(error.message.contains("YAML mapping"));
        let _ = fs::remove_dir_all(root);
    }
}

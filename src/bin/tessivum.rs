use std::{
    env, fs,
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{self, ExitCode},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};

use clap::error::ErrorKind;
use serde_json::Value;
use tessivum::{
    agent_mode::AgentModeId,
    boot_theme::inject_boot_theme,
    bridge::{HostLifecycle, WebListenerRegistry, WebListenerSnapshot},
    cli::{
        parse_cli, resolve_data_root, CliCommand, DataRootError, ExitClass, HeadlessCommand,
        PluginAction, PluginCommand, SdkCommand,
    },
    cloudflare_tunnel::{
        resolve_cloudflared, CloudflareQuickTunnel, CloudflareTunnelEndpoint, CloudflareTunnelEvent,
    },
    frontend::{FrontendHtmlTap, FrontendStatic, FrontendTapRegistration},
    headless::{run_headless, run_headless_with_adapter, HeadlessConfig},
    host::{shutdown_signal, HostApi, HostConfig, HostRuntime},
    llm::LlmAdapter,
    openai_responses::OpenAiResponsesAdapter,
    plugin_manager::{
        configure_host_plugins, enabled_client_plugin_names, install_first_party_market,
        mutate_plugins, plugin_profile_root, PluginMutation,
    },
    remote_access::{RemoteAccess, RemoteAccessConfig},
    settings::Settings,
    SessionId, TessivumError,
};
use tokio::sync::Notify;

const PROCESS_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_REMOTE_SESSION_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
const MIN_REMOTE_SESSION_TTL_SECONDS: u64 = 5 * 60;
const MAX_REMOTE_SESSION_TTL_SECONDS: u64 = 90 * 24 * 60 * 60;
static EMBEDDED_WEB_INSTANCE: AtomicU64 = AtomicU64::new(0);
const WEB_LIFECYCLE_RUNNING: u8 = 0;
const WEB_LIFECYCLE_RESTART_ACCEPTED: u8 = 1;
const WEB_LIFECYCLE_SHUTTING_DOWN: u8 = 2;

#[derive(Default)]
struct WebLifecycle {
    state: AtomicU8,
    restart: Notify,
}

impl WebLifecycle {
    async fn wait_for_restart(&self) {
        loop {
            let notified = self.restart.notified();
            if self.state.load(Ordering::Acquire) == WEB_LIFECYCLE_RESTART_ACCEPTED {
                return;
            }
            notified.await;
        }
    }

    fn begin_shutdown(&self) {
        self.state
            .store(WEB_LIFECYCLE_SHUTTING_DOWN, Ordering::Release);
    }
}

impl HostLifecycle for WebLifecycle {
    fn restart(&self) -> Result<(), TessivumError> {
        match self.state.compare_exchange(
            WEB_LIFECYCLE_RUNNING,
            WEB_LIFECYCLE_RESTART_ACCEPTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.restart.notify_one();
                Ok(())
            }
            Err(WEB_LIFECYCLE_RESTART_ACCEPTED) => Err(TessivumError::new(
                "HOST_BUSY",
                "restart is already pending",
                "hostLifecycle",
                Value::Null,
            )),
            Err(_) => Err(TessivumError::new(
                "HOST_SHUTTING_DOWN",
                "host is shutting down",
                "hostLifecycle",
                Value::Null,
            )),
        }
    }
}

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
            eprintln!(
                "Tessivum shutdown exceeded {} seconds; forcing exit",
                PROCESS_SHUTDOWN_TIMEOUT.as_secs()
            );
            process::exit(exit_code);
        }
    }
}
#[cfg(unix)]
fn relaunch_web_process() -> Result<(), Diagnostic> {
    use std::os::unix::process::CommandExt;

    let executable =
        env::current_exe().map_err(|error| Diagnostic::runtime("WEB_RESTART_FAILED", error))?;
    let error = process::Command::new(executable)
        .args(env::args_os().skip(1))
        .exec();
    Err(Diagnostic::runtime("WEB_RESTART_FAILED", error))
}

#[cfg(not(unix))]
fn relaunch_web_process() -> Result<(), Diagnostic> {
    let executable =
        env::current_exe().map_err(|error| Diagnostic::runtime("WEB_RESTART_FAILED", error))?;
    process::Command::new(executable)
        .args(env::args_os().skip(1))
        .spawn()
        .map_err(|error| Diagnostic::runtime("WEB_RESTART_FAILED", error))?;
    Ok(())
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
        CliCommand::Sdk(command) => run_sdk(command).await,
        // Keep plugin inspection isolated in the existing dedicated binary.
        CliCommand::PluginReport => Err(Diagnostic::runtime(
            "PLUGIN_REPORT_BINARY",
            "run the existing plugin_report binary for package inspection",
        )),
    }
}

fn run_plugin_command(command: PluginCommand) -> Result<(), Diagnostic> {
    let (_, data_dir) = host_paths(command.data_dir)?;
    let mutation = match command.action {
        PluginAction::Add(specifier) => PluginMutation::Add(specifier),
        PluginAction::Remove(package) => PluginMutation::Remove(package),
    };
    mutate_plugins(data_dir, mutation)
        .map_err(|error| Diagnostic::runtime("PLUGIN_MANAGEMENT_FAILED", error))
}

fn install_packaged_market(data_dir: &Path) -> Result<(), Diagnostic> {
    let tarball = env::var_os("TESSIVUM_MARKET_TARBALL").map(PathBuf::from);
    let checksum_file = env::var_os("TESSIVUM_MARKET_SHA256_FILE").map(PathBuf::from);
    let (tarball, checksum_file) = match (tarball, checksum_file) {
        (None, None) => return Ok(()),
        (Some(tarball), Some(checksum_file)) => (tarball, checksum_file),
        _ => {
            return Err(Diagnostic::runtime(
                "MARKET_ARTIFACT_CONFIG_INVALID",
                "TESSIVUM_MARKET_TARBALL and TESSIVUM_MARKET_SHA256_FILE must be set together",
            ))
        }
    };
    let metadata = fs::symlink_metadata(&checksum_file)
        .map_err(|error| Diagnostic::runtime("MARKET_CHECKSUM_INVALID", error))?;
    if !metadata.is_file() || metadata.len() > 1024 {
        return Err(Diagnostic::runtime(
            "MARKET_CHECKSUM_INVALID",
            format!(
                "{} must be a regular checksum file",
                checksum_file.display()
            ),
        ));
    }
    let checksum_document = fs::read_to_string(&checksum_file)
        .map_err(|error| Diagnostic::runtime("MARKET_CHECKSUM_INVALID", error))?;
    let mut fields = checksum_document.split_whitespace();
    let expected_sha256 = fields.next();
    let checksum_name = fields.next();
    if expected_sha256.is_none()
        || checksum_name.and_then(|name| Path::new(name).file_name()) != tarball.file_name()
        || fields.next().is_some()
    {
        return Err(Diagnostic::runtime(
            "MARKET_CHECKSUM_INVALID",
            format!(
                "{} is not a checksum for {}",
                checksum_file.display(),
                tarball.display()
            ),
        ));
    }
    install_first_party_market(data_dir, tarball, expected_sha256.unwrap())
        .map_err(|error| Diagnostic::runtime("MARKET_INSTALL_FAILED", error))
}

async fn run_headless_command(command: HeadlessCommand) -> Result<(), Diagnostic> {
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
    let (cwd, data_dir) = host_paths(command.data_dir)?;
    install_packaged_market(&data_dir)?;
    let cli_patches = load_cli_patches(&command.patches).await?;
    let address = env::var("TESSIVUM_WEB_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".into())
        .parse::<SocketAddr>()
        .map_err(|error| Diagnostic::usage(format!("invalid TESSIVUM_WEB_ADDR: {error}")))?;
    env::set_var("DSH_WEB_URL", format!("http://{address}"));
    let manual_trusted_authorities = environment("TESSIVUM_WEB_TRUSTED_AUTHORITIES")?
        .map(|authorities| {
            authorities
                .split(',')
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let auto_tunnel_enabled = match environment("TESSIVUM_REMOTE_AUTO_TUNNEL")?.as_deref() {
        None => false,
        Some("cloudflare") => true,
        Some(_) => {
            return Err(Diagnostic::usage(
                "TESSIVUM_REMOTE_AUTO_TUNNEL must be cloudflare when set",
            ))
        }
    };
    let remote_enabled = environment_flag("TESSIVUM_REMOTE_ACCESS")?;
    let trusted_tunnel = environment_flag("TESSIVUM_REMOTE_TRUSTED_TUNNEL")? || auto_tunnel_enabled;
    let session_ttl_seconds = environment("TESSIVUM_REMOTE_SESSION_TTL_SECONDS")?
        .map(|value| {
            value
                .parse::<u64>()
                .ok()
                .filter(|value| {
                    (MIN_REMOTE_SESSION_TTL_SECONDS..=MAX_REMOTE_SESSION_TTL_SECONDS)
                        .contains(value)
                })
                .ok_or_else(|| {
                    Diagnostic::usage(format!(
                        "TESSIVUM_REMOTE_SESSION_TTL_SECONDS must be an integer from {MIN_REMOTE_SESSION_TTL_SECONDS} to {MAX_REMOTE_SESSION_TTL_SECONDS}"
                    ))
                })
        })
        .transpose()?
        .unwrap_or(DEFAULT_REMOTE_SESSION_TTL_SECONDS);
    if !remote_enabled
        && (!manual_trusted_authorities.is_empty() || trusted_tunnel || auto_tunnel_enabled)
    {
        return Err(Diagnostic::usage(
            "trusted Web authorities and tunnel posture require TESSIVUM_REMOTE_ACCESS=1",
        ));
    }
    if remote_enabled && manual_trusted_authorities.is_empty() && !auto_tunnel_enabled {
        return Err(Diagnostic::usage(
            "TESSIVUM_REMOTE_ACCESS=1 requires TESSIVUM_WEB_TRUSTED_AUTHORITIES or TESSIVUM_REMOTE_AUTO_TUNNEL=cloudflare",
        ));
    }
    if remote_enabled && !address.ip().is_loopback() {
        return Err(Diagnostic::usage(
            "Remote Access requires a loopback TESSIVUM_WEB_ADDR behind the trusted TLS tunnel",
        ));
    }
    if auto_tunnel_enabled && address.port() == 0 {
        return Err(Diagnostic::usage(
            "Cloudflare Quick Tunnel requires a non-zero TESSIVUM_WEB_ADDR port",
        ));
    }
    let mut auto_tunnel = if auto_tunnel_enabled {
        let executable = resolve_cloudflared(&data_dir)
            .await
            .map_err(|error| Diagnostic::runtime("CLOUDFLARED_SETUP_FAILED", error))?;
        eprintln!("Starting Cloudflare Quick Tunnel...");
        Some(
            CloudflareQuickTunnel::start(executable, format!("http://{address}"))
                .await
                .map_err(|error| Diagnostic::runtime("CLOUDFLARE_TUNNEL_FAILED", error))?,
        )
    } else {
        None
    };
    let tunnel_endpoint = auto_tunnel.as_ref().map(|tunnel| tunnel.endpoint().clone());
    let trusted_authorities =
        effective_trusted_authorities(&manual_trusted_authorities, tunnel_endpoint.as_ref());
    let remote_access = RemoteAccess::open(
        data_dir.join("remote-access.json"),
        RemoteAccessConfig {
            enabled: remote_enabled,
            trusted_tunnel,
            session_ttl: Duration::from_secs(session_ttl_seconds),
            ..RemoteAccessConfig::default()
        },
    )
    .await
    .map_err(|error| Diagnostic::runtime(error.code(), error))?;
    let initial_origins = advertised_origins(address, &trusted_authorities);
    let listener = WebListenerRegistry::new(WebListenerSnapshot {
        host: address.ip().to_string(),
        port: address.port(),
        loopback: address.ip().is_loopback(),
        advertised_origins: initial_origins.clone(),
        remote_access_enabled: remote_enabled,
    });
    if let Some(dist) = env::var_os("TESSIVUM_WEB_DIST") {
        FrontendStatic::new(PathBuf::from(dist))
            .map_err(|error| Diagnostic::runtime("WEB_FRONTEND_FAILED", error))?;
    }
    let lifecycle = Arc::new(WebLifecycle::default());
    let mut host_config = HostConfig::new(cwd, data_dir.clone());
    host_config.enable_trusted_bash = true;
    host_config.system_prompt = Some(web_system_prompt(address));
    host_config.host_lifecycle = Some(Arc::clone(&lifecycle) as Arc<dyn HostLifecycle>);
    host_config.web_listener = Some(listener.clone());
    host_config.remote_access = Some(remote_access.clone());
    let runtime = boot_host(host_config, web_replay().await?, cli_patches).await?;
    let (frontend, _theme_tap, _embedded_assets) = match web_frontend(
        runtime
            .handle()
            .settings()
            .expect("a booted Host always publishes settings"),
        &data_dir,
    ) {
        Ok(frontend) => frontend,
        Err(error) => {
            let _ = shutdown_with_escalation(runtime.shutdown(), ExitClass::Runtime.code()).await;
            return Err(error);
        }
    };
    let host_handle = runtime.handle();
    let web_routes = host_handle.web_route_registry();
    let host: Arc<dyn HostApi> = Arc::new(host_handle);
    let mut server = match tessivum::api::ApiServer::bind_with_remote_access(
        host,
        tessivum::api::ApiServerConfig {
            bind_addr: address,
            frontend: Some(frontend),
        },
        trusted_authorities.clone(),
        web_routes,
        Some(remote_access),
    )
    .await
    {
        Ok(server) => server,
        Err(error) => {
            let _ = shutdown_with_escalation(runtime.shutdown(), ExitClass::Runtime.code()).await;
            return Err(Diagnostic::runtime("WEB_BIND_FAILED", error));
        }
    };
    let bound = server.local_addr();
    listener.publish(WebListenerSnapshot {
        host: bound.ip().to_string(),
        port: bound.port(),
        loopback: bound.ip().is_loopback(),
        advertised_origins: initial_origins,
        remote_access_enabled: remote_enabled,
    });
    eprintln!("Tessivum web listening at http://{}", server.local_addr());
    if let Some(endpoint) = tunnel_endpoint.as_ref() {
        eprintln!("Tessivum remote access at {}/remote", endpoint.origin);
    }
    enum WebStop {
        Signal(i32),
        Restart,
        TunnelFailed(String),
    }
    let stop = loop {
        let tunnel_event = async {
            match auto_tunnel.as_mut() {
                Some(tunnel) => tunnel.next_event().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            signal = shutdown_signal() => break match signal {
                Ok(signal) => WebStop::Signal(signal),
                Err(error) => {
                    lifecycle.begin_shutdown();
                    let _ = shutdown_with_escalation(
                        async {
                            if let Some(tunnel) = auto_tunnel.take() {
                                tunnel.shutdown().await;
                            }
                            let _ = server.shutdown().await;
                            let _ = runtime.shutdown().await;
                        },
                        ExitClass::Runtime.code(),
                    )
                    .await;
                    return Err(Diagnostic::runtime("SIGNAL_FAILED", error));
                }
            },
            () = lifecycle.wait_for_restart() => break WebStop::Restart,
            event = tunnel_event => {
                match event {
                    Some(CloudflareTunnelEvent::Down) => {
                        let authorities = effective_trusted_authorities(
                            &manual_trusted_authorities,
                            None,
                        );
                        if let Err(error) = server.replace_trusted_authorities(authorities.clone()) {
                            break WebStop::TunnelFailed(error.to_string());
                        }
                        listener.publish(WebListenerSnapshot {
                            host: bound.ip().to_string(),
                            port: bound.port(),
                            loopback: bound.ip().is_loopback(),
                            advertised_origins: advertised_origins(bound, &authorities),
                            remote_access_enabled: remote_enabled,
                        });
                        eprintln!("Cloudflare Quick Tunnel is unavailable; remote authority removed");
                    }
                    Some(CloudflareTunnelEvent::Running(endpoint)) => {
                        let authorities = effective_trusted_authorities(
                            &manual_trusted_authorities,
                            Some(&endpoint),
                        );
                        if let Err(error) = server.replace_trusted_authorities(authorities.clone()) {
                            break WebStop::TunnelFailed(error.to_string());
                        }
                        listener.publish(WebListenerSnapshot {
                            host: bound.ip().to_string(),
                            port: bound.port(),
                            loopback: bound.ip().is_loopback(),
                            advertised_origins: advertised_origins(bound, &authorities),
                            remote_access_enabled: remote_enabled,
                        });
                        eprintln!("Tessivum remote access at {}/remote", endpoint.origin);
                    }
                    None => break WebStop::TunnelFailed(
                        "Cloudflare Quick Tunnel supervisor stopped".into(),
                    ),
                }
            }
        }
    };
    lifecycle.begin_shutdown();
    listener.clear();
    let shutdown_exit = match stop {
        WebStop::Signal(130) => 130,
        WebStop::Signal(_) | WebStop::Restart | WebStop::TunnelFailed(_) => 0,
    };
    let (server_result, host_result) = shutdown_with_escalation(
        async {
            if let Some(tunnel) = auto_tunnel.take() {
                tunnel.shutdown().await;
            }
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
    match stop {
        WebStop::Signal(130) => Err(Diagnostic::cancelled("interrupted")),
        WebStop::Signal(_) => Ok(()),
        WebStop::Restart => relaunch_web_process(),
        WebStop::TunnelFailed(error) => Err(Diagnostic::runtime("CLOUDFLARE_TUNNEL_FAILED", error)),
    }
}
fn effective_trusted_authorities(
    configured: &[String],
    tunnel: Option<&CloudflareTunnelEndpoint>,
) -> Vec<String> {
    let mut authorities = Vec::with_capacity(configured.len() + usize::from(tunnel.is_some()));
    if let Some(tunnel) = tunnel {
        authorities.push(tunnel.authority.clone());
    }
    authorities.extend(
        configured
            .iter()
            .filter(|authority| tunnel.is_none_or(|tunnel| *authority != &tunnel.authority))
            .cloned(),
    );
    authorities
}

fn advertised_origins(address: SocketAddr, authorities: &[String]) -> Vec<String> {
    std::iter::once(format!("http://{address}"))
        .chain(
            authorities
                .iter()
                .map(|authority| format!("https://{authority}")),
        )
        .collect()
}

fn web_frontend(
    settings: Arc<Settings>,
    data_dir: &Path,
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
    package_roots.extend(enabled_client_package_roots(data_dir)?);
    frontend
        .scan_packages(package_roots)
        .map_err(|error| Diagnostic::runtime("WEB_CLIENT_PACKAGES_FAILED", error))?;
    Ok((frontend, tap, embedded))
}
fn enabled_client_package_roots(data_dir: &Path) -> Result<Vec<PathBuf>, Diagnostic> {
    let profile = plugin_profile_root(data_dir);
    let names = enabled_client_plugin_names(&profile)
        .map_err(|error| Diagnostic::runtime("WEB_CLIENT_PACKAGES_FAILED", error))?;
    let installed = profile.join("node_modules");
    Ok(names
        .into_iter()
        .map(|name| installed.join(name))
        .filter(|root| root.is_dir())
        .collect())
}

enum SdkOutcome {
    Eof,
    Signal(i32),
}

async fn run_sdk(command: SdkCommand) -> Result<(), Diagnostic> {
    let (cwd, data_dir) = host_paths(command.data_dir)?;
    let mut host_config = HostConfig::new(cwd, data_dir);
    host_config.enable_trusted_bash = true;
    let runtime = boot_host(host_config, None, Vec::new()).await?;
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
    mut config: HostConfig,
    recorded_replay: Option<WebReplay>,
    cli_patches: Vec<Value>,
) -> Result<HostRuntime, Diagnostic> {
    for patch in cli_patches {
        config = config.with_cli_patch(patch);
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
    if config.host_lifecycle.is_some() {
        if let Some(host) = &mut config.legacy_host {
            host.command
                .env
                .push(("TESSIVUM_HOST_LIFECYCLE".into(), "1".into()));
        }
    }
    if let (Some(host), Some(listener)) = (&mut config.legacy_host, &config.web_listener) {
        if let Some(snapshot) = listener.describe() {
            host.command
                .env
                .push(("TESSIVUM_WEB_LISTENER_HOST".into(), snapshot.host.into()));
            host.command.env.push((
                "TESSIVUM_WEB_LISTENER_PORT".into(),
                snapshot.port.to_string().into(),
            ));
        }
    }
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

fn environment_flag(name: &str) -> Result<bool, Diagnostic> {
    match environment(name)?.as_deref() {
        None | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(_) => Err(Diagnostic::usage(format!("{name} must be 0 or 1"))),
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

fn host_paths(data_dir: Option<PathBuf>) -> Result<(PathBuf, PathBuf), Diagnostic> {
    let paths = resolve_data_root(data_dir).map_err(|error| match error {
        DataRootError::CurrentDir(source) => Diagnostic::runtime("CWD_RESOLUTION_FAILED", source),
        error => Diagnostic::usage(error.to_string()),
    })?;
    Ok((paths.cwd, paths.data_dir))
}

async fn load_cli_patches(paths: &[PathBuf]) -> Result<Vec<Value>, Diagnostic> {
    let mut patches = Vec::with_capacity(paths.len());
    for path in paths {
        let source = tokio::fs::read_to_string(path).await.map_err(|error| {
            Diagnostic::runtime(
                "PATCH_READ_FAILED",
                format!("cannot read {}: {error}", path.display()),
            )
        })?;
        let patch = serde_yaml::from_str::<Value>(&source).map_err(|error| {
            Diagnostic::usage(format!("invalid patch {}: {error}", path.display()))
        })?;
        if !patch.is_object() {
            return Err(Diagnostic::usage(format!(
                "patch {} must contain a YAML mapping",
                path.display()
            )));
        }
        patches.push(patch);
    }
    Ok(patches)
}

async fn config(command: HeadlessCommand) -> Result<(HeadlessConfig, String), Diagnostic> {
    let (cwd, data_dir) = host_paths(command.data_dir)?;
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
            agent_mode: AgentModeId::standard(),
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

    #[test]
    fn active_quick_tunnel_precedes_manual_authorities() {
        let tunnel = CloudflareTunnelEndpoint {
            origin: "https://quick.trycloudflare.com".into(),
            authority: "quick.trycloudflare.com".into(),
        };
        assert_eq!(
            effective_trusted_authorities(
                &[
                    "stable.example.test".into(),
                    "quick.trycloudflare.com".into()
                ],
                Some(&tunnel),
            ),
            ["quick.trycloudflare.com", "stable.example.test"],
        );
    }

    fn patch_dir(name: &str) -> PathBuf {
        let root = env::temp_dir().join(format!(
            "tessivum-cli-{name}-{}-{}",
            process::id(),
            EMBEDDED_WEB_INSTANCE.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).expect("test fixture directory creates");
        root
    }

    #[tokio::test]
    async fn cli_patches_load_yaml_mappings_in_order() {
        let root = patch_dir("patch-overlays");
        let base = root.join("base.yml");
        let local = root.join("local.yml");
        fs::write(&base, "feature:\n  enabled: false\n").unwrap();
        fs::write(&local, "feature:\n  enabled: true\n").unwrap();

        assert_eq!(
            load_cli_patches(&[base, local]).await.unwrap(),
            [
                serde_json::json!({"feature": {"enabled": false}}),
                serde_json::json!({"feature": {"enabled": true}}),
            ]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_roots_respect_explicit_bundle_selection() {
        let data = patch_dir("explicit-browser-roots");
        let profile = plugin_profile_root(&data);
        let modules = profile.join("node_modules");
        for (name, manifest) in [
            (
                "enabled-bundle",
                r#"{"name":"enabled-bundle","dsh":{"bundle":{"patch":"./bundle.yml"},"client":{"platform":"web"}}}"#,
            ),
            (
                "disabled-bundle",
                r#"{"name":"disabled-bundle","dsh":{"bundle":{"patch":"./bundle.yml"},"client":{"platform":"web"}}}"#,
            ),
            (
                "client-only",
                r#"{"name":"client-only","dsh":{"client":{"platform":"web"}}}"#,
            ),
            ("dependency-only", r#"{"name":"dependency-only"}"#),
        ] {
            let root = modules.join(name);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("package.json"), manifest).unwrap();
            fs::write(root.join("bundle.yml"), "[]\n").unwrap();
        }
        fs::write(
            profile.join("package.json"),
            r#"{"dependencies":{"enabled-bundle":"1.0.0","disabled-bundle":"1.0.0","client-only":"1.0.0","dependency-only":"1.0.0"},"dsh":{"profile":{"bundles":["enabled-bundle"]}}}"#,
        )
        .unwrap();

        assert_eq!(
            enabled_client_package_roots(&data).unwrap(),
            vec![modules.join("client-only"), modules.join("enabled-bundle")]
        );

        fs::write(
            profile.join("package.json"),
            r#"{"dependencies":{"enabled-bundle":"1.0.0","disabled-bundle":"1.0.0","client-only":"1.0.0","dependency-only":"1.0.0"},"dsh":{"profile":{"bundles":[]}}}"#,
        )
        .unwrap();
        assert_eq!(
            enabled_client_package_roots(&data).unwrap(),
            vec![modules.join("client-only")]
        );
        let _ = fs::remove_dir_all(data);
    }

    #[test]
    fn browser_roots_migrate_bundle_selection_without_dependency_leaks() {
        let data = patch_dir("migrated-browser-roots");
        let profile = plugin_profile_root(&data);
        let modules = profile.join("node_modules");
        for (name, manifest) in [
            (
                "first-bundle",
                r#"{"name":"first-bundle","dsh":{"bundle":{"patch":"./bundle.yml"},"client":{"platform":"web"}}}"#,
            ),
            (
                "second-bundle",
                r#"{"name":"second-bundle","dsh":{"bundle":{"patch":"./bundle.yml"},"client":{"platform":"web"}}}"#,
            ),
            (
                "client-only",
                r#"{"name":"client-only","dsh":{"client":{"platform":"web"}}}"#,
            ),
            ("dependency-only", r#"{"name":"dependency-only"}"#),
        ] {
            let root = modules.join(name);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("package.json"), manifest).unwrap();
            fs::write(root.join("bundle.yml"), "[]\n").unwrap();
        }
        fs::write(
            profile.join("package.json"),
            r#"{"dependencies":{"first-bundle":"1.0.0","dependency-only":"1.0.0","second-bundle":"1.0.0","client-only":"1.0.0"}}"#,
        )
        .unwrap();

        assert_eq!(
            enabled_client_package_roots(&data).unwrap(),
            vec![
                modules.join("client-only"),
                modules.join("first-bundle"),
                modules.join("second-bundle"),
            ]
        );
        let _ = fs::remove_dir_all(data);
    }
    #[tokio::test]
    async fn web_restart_is_single_shot_and_rejects_shutdown_requests() {
        let lifecycle = WebLifecycle::default();
        lifecycle.restart().expect("first restart is accepted");
        tokio::time::timeout(Duration::from_secs(1), lifecycle.wait_for_restart())
            .await
            .expect("accepted restart wakes the owner");
        assert_eq!(lifecycle.restart().unwrap_err().code, "HOST_BUSY");
        lifecycle.begin_shutdown();
        assert_eq!(lifecycle.restart().unwrap_err().code, "HOST_SHUTTING_DOWN");
    }
}
